//! Desktop notifications (spec §4 "Alerts"). No signed app bundle, so the backend is picked per
//! send from what is installed:
//!
//! - macOS: `terminal-notifier` (clicking opens the dashboard), else `osascript display
//!   notification` whose text says where to go. The text reaches AppleScript as an `argv` item,
//!   never spliced into the script, so nothing in it can break out of the string.
//! - Linux: `notify-send` if present.
//! - Otherwise nothing is shown (the event is still logged by the caller).
//!
//! Commands run without a shell, with a [`SEND_TIMEOUT`]; [`Notifier::notify`] blocks, so async
//! callers run it under `spawn_blocking`. Notification text and arguments never carry secrets
//! (no management key, token or email): the click URL is the plain dashboard URL.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};

/// Longest a notifier command may run before it is killed.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// Notification title.
pub const TITLE: &str = "pooltime";
/// The AppleScript run by the `osascript` fallback: the text is `argv` item 1.
pub const OSASCRIPT_LINES: [&str; 3] = [
    "on run argv",
    "display notification (item 1 of argv) with title \"pooltime\"",
    "end run",
];

/// Directories searched after `PATH` on macOS: a launchd service's `PATH` lacks Homebrew's.
const MACOS_EXTRA_DIRS: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];

/// One notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// What happened, e.g. `work needs login`.
    pub message: String,
    /// Where to go, e.g. `open pooltime → Re-login`.
    pub action: String,
    /// The dashboard URL a click opens (no key, no secrets).
    pub url: String,
}

impl Notification {
    /// The text shown where a click cannot open the dashboard: `<message> — <action>`.
    pub fn text(&self) -> String {
        format!("{} — {}", self.message, self.action)
    }
}

/// Shows notifications. Blocking.
pub trait Notifier: Send + Sync {
    fn notify(&self, n: &Notification) -> anyhow::Result<()>;
}

/// The real notifier: runs whichever backend is installed.
#[derive(Debug, Clone)]
pub struct SystemNotifier {
    /// Where commands are looked up (a `PATH`-style list).
    search_path: OsString,
    macos: bool,
    timeout: Duration,
}

impl SystemNotifier {
    /// From this process's `PATH` (plus Homebrew's bin dirs on macOS) and platform.
    pub fn from_env() -> SystemNotifier {
        let macos = cfg!(target_os = "macos");
        let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();
        if macos {
            dirs.extend(MACOS_EXTRA_DIRS.iter().map(PathBuf::from));
        }
        let search_path = std::env::join_paths(dirs).unwrap_or_default();
        SystemNotifier::new(search_path, macos)
    }

    /// Commands are looked up only in `search_path`; `macos` picks the backend family.
    pub fn new(search_path: impl Into<OsString>, macos: bool) -> SystemNotifier {
        SystemNotifier {
            search_path: search_path.into(),
            macos,
            timeout: SEND_TIMEOUT,
        }
    }

    /// The command (program, arguments) that would show `n`, `None` if no backend is installed.
    pub fn command_for(&self, n: &Notification) -> Option<(PathBuf, Vec<OsString>)> {
        if self.macos {
            if let Some(tn) = self.find("terminal-notifier") {
                let args = ["-title", TITLE, "-message", &n.message, "-open", &n.url];
                return Some((tn, args.iter().map(OsString::from).collect()));
            }
            let osa = self.find("osascript")?;
            let mut args = Vec::new();
            for line in OSASCRIPT_LINES {
                args.push(OsString::from("-e"));
                args.push(OsString::from(line));
            }
            args.push(OsString::from(n.text()));
            return Some((osa, args));
        }
        let ns = self.find("notify-send")?;
        Some((ns, vec![OsString::from(TITLE), OsString::from(n.text())]))
    }

    fn find(&self, name: &str) -> Option<PathBuf> {
        std::env::split_paths(&self.search_path)
            .map(|d| d.join(name))
            .find(|p| is_executable(p))
    }
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

impl Notifier for SystemNotifier {
    fn notify(&self, n: &Notification) -> anyhow::Result<()> {
        let Some((program, args)) = self.command_for(n) else {
            tracing::info!("no desktop notifier installed: alert logged only");
            return Ok(());
        };
        let name = program
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut child = Command::new(&program)
            .args(&args)
            .env("PATH", &self.search_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("running {name}"))?;
        let deadline = Instant::now() + self.timeout;
        loop {
            if let Some(status) = child
                .try_wait()
                .with_context(|| format!("waiting for {name}"))?
            {
                if !status.success() {
                    bail!("{name} exited with {status}");
                }
                return Ok(());
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("{name} timed out after {:?}", self.timeout);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Records notifications instead of showing them (tests).
#[derive(Debug, Default)]
pub struct RecordingNotifier {
    sent: Mutex<Vec<Notification>>,
    /// Make every send fail (the notification is still recorded as attempted).
    pub fail: bool,
}

impl RecordingNotifier {
    /// One whose every send fails.
    pub fn failing() -> RecordingNotifier {
        RecordingNotifier {
            fail: true,
            ..Default::default()
        }
    }

    pub fn sent(&self) -> Vec<Notification> {
        self.sent.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Notifier for RecordingNotifier {
    fn notify(&self, n: &Notification) -> anyhow::Result<()> {
        self.sent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(n.clone());
        if self.fail {
            bail!("recording notifier set to fail");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn note() -> Notification {
        Notification {
            message: "work \"x\" \\ needs login".into(),
            action: "open pooltime → Re-login".into(),
            url: "http://127.0.0.1:3460/".into(),
        }
    }

    /// A shim `name` in `bin` that writes its arguments, one per line, to `out/<name>.args`.
    fn shim(bin: &Path, out: &Path, name: &str) {
        let p = bin.join(name);
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
            out.join(format!("{name}.args")).display()
        );
        std::fs::write(&p, script).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn args_of(out: &Path, name: &str) -> Option<Vec<String>> {
        let raw = std::fs::read_to_string(out.join(format!("{name}.args"))).ok()?;
        Some(raw.lines().map(str::to_string).collect())
    }

    #[test]
    fn terminal_notifier_preferred_when_present() {
        let (bin, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        shim(bin.path(), out.path(), "terminal-notifier");
        shim(bin.path(), out.path(), "osascript");
        let n = SystemNotifier::new(bin.path(), true);
        n.notify(&note()).unwrap();
        assert_eq!(
            args_of(out.path(), "terminal-notifier").unwrap(),
            vec![
                "-title",
                "pooltime",
                "-message",
                "work \"x\" \\ needs login",
                "-open",
                "http://127.0.0.1:3460/",
            ]
        );
        assert!(
            args_of(out.path(), "osascript").is_none(),
            "only one backend runs"
        );
    }

    #[test]
    fn osascript_fallback_text() {
        let (bin, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        shim(bin.path(), out.path(), "osascript");
        let n = SystemNotifier::new(bin.path(), true);
        n.notify(&note()).unwrap();
        let args = args_of(out.path(), "osascript").unwrap();
        // The script is fixed; the text arrives as argv, so quotes/backslashes cannot inject.
        assert_eq!(
            args[..6],
            [
                "-e",
                "on run argv",
                "-e",
                "display notification (item 1 of argv) with title \"pooltime\"",
                "-e",
                "end run",
            ]
        );
        assert_eq!(args.len(), 7);
        assert_eq!(
            args[6],
            "work \"x\" \\ needs login — open pooltime → Re-login"
        );
        assert!(args[6].ends_with("open pooltime → Re-login"));
    }

    #[test]
    fn notify_send_on_linux_and_none_is_ok() {
        let (bin, out) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        // Nothing installed: a no-op, not an error.
        SystemNotifier::new(bin.path(), false)
            .notify(&note())
            .unwrap();
        SystemNotifier::new(bin.path(), true)
            .notify(&note())
            .unwrap();
        shim(bin.path(), out.path(), "notify-send");
        shim(bin.path(), out.path(), "terminal-notifier");
        SystemNotifier::new(bin.path(), false)
            .notify(&note())
            .unwrap();
        assert_eq!(
            args_of(out.path(), "notify-send").unwrap(),
            vec![
                "pooltime",
                "work \"x\" \\ needs login — open pooltime → Re-login"
            ]
        );
        assert!(args_of(out.path(), "terminal-notifier").is_none());
    }

    #[test]
    fn failing_or_hanging_notifier_is_an_error() {
        let bin = tempfile::tempdir().unwrap();
        let p = bin.path().join("notify-send");
        std::fs::write(&p, "#!/bin/sh\nexit 3\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(SystemNotifier::new(bin.path(), false)
            .notify(&note())
            .is_err());
        std::fs::write(&p, "#!/bin/sh\nexec /bin/sleep 30\n").unwrap();
        let mut n = SystemNotifier::new(bin.path(), false);
        n.timeout = Duration::from_millis(200);
        let started = Instant::now();
        let e = n.notify(&note()).unwrap_err();
        assert!(e.to_string().contains("timed out"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
