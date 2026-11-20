//! Move a running supervisor onto the binary at its path, in place (`SIGUSR2`, see
//! [`crate::supervisor::resume`]): shared by `pooltime update` and `pooltime service install`.
//!
//! Only the supervisor that runs right now is signalled: the state file is read again and its pid
//! must still be a `pooltime serve --supervise` process (never a reused pid). Then the caller
//! waits for the supervisor's verdict (`upgrades` goes up) and checks the versions it reports.
//! Everything that touches the system goes through [`Live`], so the flows are testable.

use std::time::{Duration, Instant};

use anyhow::Context;

use super::Timing;
use crate::config::Paths;
use crate::supervisor::proto::{self, SupervisorState, UpgradeResult};
use crate::version::Version;

/// The running supervisor, as the CLI sees it.
pub trait Live {
    /// The running supervisor, if any (`run/supervisor.json` with a live pid).
    fn supervisor(&self) -> Option<SupervisorState>;
    /// Whether `pid` is a running `pooltime serve --supervise` process.
    fn is_supervisor(&self, pid: u32) -> bool;
    /// `SIGUSR2` to the supervisor.
    fn signal_upgrade(&self, pid: u32) -> anyhow::Result<()>;
    /// Wait (blocking) until the supervisor has finished an upgrade after the first `after`.
    fn wait_upgrade(&self, after: u64) -> Option<SupervisorState>;
}

/// The real thing, for the pool at `paths`.
pub struct SystemLive {
    pub paths: Paths,
    pub timing: Timing,
}

impl Live for SystemLive {
    fn supervisor(&self) -> Option<SupervisorState> {
        proto::live_state(&self.paths)
    }

    fn is_supervisor(&self, pid: u32) -> bool {
        command_line(pid).is_some_and(|c| looks_like_supervisor(&c))
    }

    fn signal_upgrade(&self, pid: u32) -> anyhow::Result<()> {
        super::signal_upgrade(pid)
    }

    fn wait_upgrade(&self, after: u64) -> Option<SupervisorState> {
        let budget = upgrade_timeout(&self.timing);
        let start = Instant::now();
        loop {
            if let Some(s) = proto::read_state(&self.paths).filter(|s| s.upgrades > after) {
                return Some(s);
            }
            if start.elapsed() >= budget {
                return None;
            }
            std::thread::sleep(self.timing.poll_step);
        }
    }
}

/// How long an upgrade may take: the new binary's check, waiting for a restart under way to
/// finish, then the new supervisor's first handover.
pub fn upgrade_timeout(timing: &Timing) -> Duration {
    timing.handoff_timeout
        + crate::supervisor::PROBE_TIMEOUT
        + crate::supervisor::UPGRADE_IDLE_TIMEOUT
        + Duration::from_secs(10)
}

/// How an in-place upgrade ended.
#[derive(Debug, Clone, PartialEq, Eq)]
// Returned once per command; boxing the state would only add noise at every match.
#[allow(clippy::large_enum_variant)]
pub enum Upgrade {
    /// The supervisor in the state file is gone, or its pid belongs to another process now:
    /// nothing was signalled.
    Vanished,
    /// Signalled, but no verdict within [`upgrade_timeout`].
    NoVerdict,
    /// The supervisor runs the new binary and a worker from it is active.
    Done {
        result: UpgradeResult,
        after: SupervisorState,
    },
    /// The supervisor refused or failed; it keeps serving as it was.
    Failed {
        error: String,
        after: SupervisorState,
    },
    /// The counter went up without a result.
    NoResult,
}

/// Ask the supervisor `sup` (as read a moment ago) to replace itself with the binary at its path
/// and wait for its verdict.
pub fn upgrade_in_place<L: Live + ?Sized>(
    live: &L,
    sup: &SupervisorState,
) -> anyhow::Result<Upgrade> {
    match live.supervisor() {
        Some(now) if now.pid == sup.pid && live.is_supervisor(sup.pid) => {}
        _ => return Ok(Upgrade::Vanished),
    }
    live.signal_upgrade(sup.pid)
        .context("signalling the pooltime supervisor")?;
    let Some(after) = live.wait_upgrade(sup.upgrades) else {
        return Ok(Upgrade::NoVerdict);
    };
    Ok(match after.last_upgrade.clone() {
        Some(result) if result.ok => Upgrade::Done { result, after },
        Some(result) => Upgrade::Failed {
            error: result.error.unwrap_or_else(|| "unknown error".into()),
            after,
        },
        None => Upgrade::NoResult,
    })
}

/// `supervisor 1.2.0 -> 1.3.0, worker 1.2.0 -> 1.3.0 (pid 42 kept, no connection dropped)`.
pub fn upgraded_line(
    before: &SupervisorState,
    result: &UpgradeResult,
    after: &SupervisorState,
) -> String {
    format!(
        "supervisor {} -> {}, worker {} -> {} (pid {} kept, no connection dropped)",
        result.from_version,
        result.version,
        before.worker_version.as_deref().unwrap_or("?"),
        after.worker_version.as_deref().unwrap_or("?"),
        after.pid
    )
}

/// The first of the supervisor's and its worker's versions that is not `target` (`Some(None)`
/// when one is unknown); `None` when both run it.
pub fn off_target(after: &SupervisorState, target: &Version) -> Option<Option<String>> {
    [
        Some(after.version.as_str()),
        after.worker_version.as_deref(),
    ]
    .into_iter()
    .find(|v| v.and_then(Version::parse).as_ref() != Some(target))
    .map(|v| v.map(str::to_string))
}

/// The command line of process `pid`, its arguments separated by spaces.
fn command_line(pid: u32) -> Option<String> {
    if cfg!(target_os = "linux") {
        let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let args: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        return (!args.is_empty()).then(|| args.join(" "));
    }
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !line.is_empty()).then_some(line)
}

/// A `pooltime serve --supervise` command line (any path to the binary, any further flags).
fn looks_like_supervisor(cmdline: &str) -> bool {
    let words: Vec<&str> = cmdline.split_whitespace().collect();
    let Some(serve) = words.iter().position(|w| *w == "serve") else {
        return false;
    };
    serve > 0
        && words[serve - 1]
            .rsplit('/')
            .next()
            .is_some_and(|n| n.starts_with("pooltime"))
        && words[serve + 1..].contains(&"--supervise")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervisor_command_lines() {
        for yes in [
            "/opt/homebrew/bin/pooltime serve --supervise",
            "pooltime serve --supervise --i-stopped-bro",
            "/home/u/.local/bin/pooltime serve --supervise --resume {\"proto\":1}",
        ] {
            assert!(looks_like_supervisor(yes), "{yes}");
        }
        for no in [
            "/usr/bin/vim notes",
            "pooltime serve",
            "pooltime serve --listen-fd 3 --control-fd 4",
            "/bin/sh -c echo serve --supervise",
            "",
        ] {
            assert!(!looks_like_supervisor(no), "{no}");
        }
        assert!(command_line(std::process::id()).is_some());
    }

    #[test]
    fn versions_off_the_target() {
        let v = Version::parse("1.3.0").unwrap();
        let st = |s: &str, w: Option<&str>| SupervisorState {
            version: s.into(),
            worker_version: w.map(str::to_string),
            ..SupervisorState::default()
        };
        assert_eq!(off_target(&st("1.3.0", Some("1.3.0")), &v), None);
        assert_eq!(
            off_target(&st("1.2.0", Some("1.3.0")), &v),
            Some(Some("1.2.0".into()))
        );
        assert_eq!(
            off_target(&st("1.3.0", Some("1.2.9")), &v),
            Some(Some("1.2.9".into()))
        );
        assert_eq!(off_target(&st("1.3.0", None), &v), Some(None));
    }
}
