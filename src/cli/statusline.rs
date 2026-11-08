//! `pooltime statusline [--wrap <profile-id>]`: the status line segment (spec §4, §16).
//!
//! Plain `pooltime statusline` prints the segment from the daemon's cache
//! ([`crate::claude_code::statusline::read_segment`]). With `--wrap` (what `connect --statusline`
//! installs) it reads Claude Code's stdin JSON, runs the profile's original `statusLine` command
//! (`sh -c`, same stdin, stdout captured, stderr dropped) and appends ` · <segment>` to its output.
//!
//! The segment is computed alongside the original command and must be ready within [`BUDGET`]
//! of starting; otherwise (or if it fails) the original output is printed alone. The original
//! itself may take up to [`ORIGINAL_TIMEOUT`]; past that its process group is killed and the
//! segment is printed alone. The status line never errors loudly: every path exits 0.

use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::claude_code::snapshot::{self, SnapshotState};
use crate::claude_code::statusline::read_segment;
use crate::config::Paths;

/// Time pooltime's own work (the segment) may take.
pub const BUDGET: Duration = Duration::from_millis(150);
/// Longest the user's original command may run before it is killed.
pub const ORIGINAL_TIMEOUT: Duration = Duration::from_secs(5);
const SEP: &str = " · ";

/// Run the command; always `0`.
pub fn run(paths: &Paths, wrap: Option<String>) -> i32 {
    let out = match wrap {
        None => {
            let mut s = read_segment(paths);
            s.push('\n');
            s.into_bytes()
        }
        Some(id) => {
            let mut stdin = Vec::new();
            let _ = std::io::stdin().read_to_end(&mut stdin);
            let prior = prior_statusline(paths, &id);
            let paths = paths.clone();
            wrap_with(
                &stdin,
                &prior,
                move || read_segment(&paths),
                BUDGET,
                ORIGINAL_TIMEOUT,
            )
        }
    };
    let mut so = std::io::stdout().lock();
    let _ = so.write_all(&out);
    let _ = so.flush();
    0
}

/// The profile's original `statusLine` from its snapshot; `null` when there is none (no
/// snapshot, an unreadable one, or an id that cannot name one).
fn prior_statusline(paths: &Paths, id: &str) -> Value {
    match snapshot::snapshot_path_for_id(paths, id).map(|p| snapshot::read(&p)) {
        Some(SnapshotState::Present(s)) => s.statusline_prior,
        _ => Value::Null,
    }
}

/// The shell command of a `{"type":"command","command":"..."}` status line.
fn command_of(prior: &Value) -> Option<&str> {
    (prior.get("type").and_then(Value::as_str) == Some("command"))
        .then(|| prior.get("command")?.as_str())
        .flatten()
        .filter(|c| !c.trim().is_empty())
}

/// Run `sh -c command` (in its own process group) with `stdin`, returning its stdout (whatever it
/// printed, even when it failed). `None` if it could not be started, or if it has not exited and
/// closed its output within `timeout`: then the whole group is killed. stderr is discarded.
fn run_original(command: &str, stdin: &[u8], timeout: Duration) -> Option<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let pid = child.id() as libc::pid_t;
    // Writer and reader threads, so a command that prints before reading cannot deadlock us and
    // a hung one cannot hold us past the deadline. Neither is joined: after a kill they end on
    // the broken pipes.
    let (mut input, mut output) = (child.stdin.take()?, child.stdout.take()?);
    let bytes = stdin.to_vec();
    std::thread::spawn(move || {
        let _ = input.write_all(&bytes);
    });
    let (out_tx, out_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = output.read_to_end(&mut buf);
        let _ = out_tx.send(buf);
    });
    let (exit_tx, exit_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = exit_tx.send(child.wait());
    });
    let left = || deadline.saturating_duration_since(Instant::now());
    let done = exit_rx.recv_timeout(left()).is_ok();
    let out = if done {
        out_rx.recv_timeout(left()).ok()
    } else {
        None
    };
    if out.is_none() {
        // SAFETY: plain kill(2) on the group we created; a stale group id fails harmlessly.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    out
}

/// The wrapper's output: the original status line (from `prior`, fed `stdin`) with ` · ` and the
/// segment appended. `segment` runs on its own thread while the original runs; if it has not
/// answered within `budget` of starting (or panicked), the original output is returned alone.
/// One trailing newline of the original is kept after the segment. No original command, no
/// output from it, or an original that ran past `original_timeout` (killed): the segment alone
/// (newline-terminated).
pub fn wrap_with(
    stdin: &[u8],
    prior: &Value,
    segment: impl FnOnce() -> String + Send + 'static,
    budget: Duration,
    original_timeout: Duration,
) -> Vec<u8> {
    let started = Instant::now();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(segment());
    });
    let original = command_of(prior)
        .and_then(|c| run_original(c, stdin, original_timeout))
        .unwrap_or_default();
    let segment = rx
        .recv_timeout(budget.saturating_sub(started.elapsed()))
        .ok();
    compose(original, segment)
}

fn compose(original: Vec<u8>, segment: Option<String>) -> Vec<u8> {
    let Some(segment) = segment else {
        return original;
    };
    let (body, newline) = match original.strip_suffix(b"\n") {
        Some(b) => (b, true),
        None => (original.as_slice(), false),
    };
    if body.is_empty() {
        return format!("{segment}\n").into_bytes();
    }
    let mut out = body.to_vec();
    out.extend_from_slice(SEP.as_bytes());
    out.extend_from_slice(segment.as_bytes());
    if newline {
        out.push(b'\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cmd(c: &str) -> Value {
        json!({"type": "command", "command": c, "padding": 0})
    }

    fn seg() -> String {
        "pool 6/8 · 7d 41% left · ⚠ 2 need login".into()
    }

    #[test]
    fn wrap_appends_segment_to_original_output() {
        // `cat` proves the original sees Claude Code's stdin bytes.
        let out = wrap_with(
            b"{\"model\":\"x\"}\n",
            &cmd("cat"),
            seg,
            BUDGET,
            ORIGINAL_TIMEOUT,
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "{\"model\":\"x\"} · pool 6/8 · 7d 41% left · ⚠ 2 need login\n"
        );
        // No trailing newline in, none out; stderr never reaches the output.
        let out = wrap_with(
            b"",
            &cmd("printf orig; echo noise >&2"),
            seg,
            BUDGET,
            ORIGINAL_TIMEOUT,
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "orig · pool 6/8 · 7d 41% left · ⚠ 2 need login"
        );
        // A failing original still has its stdout shown.
        let out = wrap_with(
            b"",
            &cmd("echo partial; exit 3"),
            seg,
            BUDGET,
            ORIGINAL_TIMEOUT,
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "partial · pool 6/8 · 7d 41% left · ⚠ 2 need login\n"
        );
        // No prior (or nothing printed): the segment alone.
        for prior in [Value::Null, cmd("true"), json!({"type": "static"})] {
            let out = wrap_with(b"{}", &prior, seg, BUDGET, ORIGINAL_TIMEOUT);
            assert_eq!(String::from_utf8(out).unwrap(), format!("{}\n", seg()));
        }
    }

    #[test]
    fn budget_exceeded_prints_original_only() {
        let slow = || {
            std::thread::sleep(Duration::from_secs(2));
            seg()
        };
        let t = Instant::now();
        let out = wrap_with(b"{}", &cmd("echo orig"), slow, BUDGET, ORIGINAL_TIMEOUT);
        assert_eq!(out, b"orig\n");
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    }

    #[test]
    fn hung_original_is_killed_and_segment_printed_alone() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("late");
        // Prints something, then hangs (a background grandchild holds the pipe too).
        let c = format!(
            "printf partial; (sleep 30; touch '{}') & sleep 30",
            marker.display()
        );
        let t = Instant::now();
        let out = wrap_with(b"{}", &cmd(&c), seg, BUDGET, Duration::from_millis(300));
        assert_eq!(String::from_utf8(out).unwrap(), format!("{}\n", seg()));
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        // The whole group is gone: the grandchild never gets to run on.
        std::thread::sleep(Duration::from_millis(100));
        let out = Command::new("pgrep")
            .args(["-f", &marker.to_string_lossy()])
            .output()
            .unwrap();
        assert!(out.stdout.is_empty(), "grandchild still running");
    }

    #[test]
    fn missing_snapshot_is_no_prior() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        assert!(prior_statusline(&paths, "nope").is_null());
        assert!(prior_statusline(&paths, "../x").is_null());
    }
}
