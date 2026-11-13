//! macOS: a launchd user agent, `~/Library/LaunchAgents/dev.pooltime.daemon.plist`, loaded into
//! the user's GUI domain (`gui/<uid>`) with the modern `bootstrap`/`bootout` verbs.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};

use super::{run_checked, stable_bin, CmdOutput, Env, Runner, Status, BOOTSTRAP_ATTEMPTS};
use crate::config::Paths;
use crate::fsutil::atomic_write;

pub const LABEL: &str = "dev.pooltime.daemon";

pub fn plist_path(env: &Env) -> PathBuf {
    env.home
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// The launch agent definition for `bin serve --supervise` (the supervisor is the job's main
/// process; its workers live in its process group, so launchd's tracking, `KeepAlive` and
/// `bootout` cover them). A Homebrew Cellar path is replaced by the stable
/// `<prefix>/bin/pooltime` symlink so `brew upgrade` does not strand the agent.
pub fn render_launchd_plist(bin: &Path, paths: &Paths) -> String {
    let s = |p: &Path| xml_escape(&p.to_string_lossy());
    let bin = stable_bin(bin);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{bin}</string>
    <string>serve</string>
    <string>--supervise</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>POOLTIME_HOME</key>
    <string>{home}</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>ExitTimeOut</key>
  <integer>600</integer>
  <key>StandardOutPath</key>
  <string>{out}</string>
  <key>StandardErrorPath</key>
  <string>{err}</string>
</dict>
</plist>
"#,
        bin = s(&bin),
        home = s(&paths.home),
        out = s(&paths.logs.join("daemon.out")),
        err = s(&paths.logs.join("daemon.err")),
    )
}

fn domain(env: &Env) -> String {
    format!("gui/{}", env.uid)
}

fn target(env: &Env) -> String {
    format!("{}/{LABEL}", domain(env))
}

fn is_loaded(env: &Env, r: &dyn Runner) -> bool {
    r.run("launchctl", &["print", &target(env)])
        .is_ok_and(|o| o.success)
}

/// `launchctl bootout` says the job was not loaded: exit 3 or 113, or its usual messages.
fn not_loaded(out: &CmdOutput) -> bool {
    matches!(out.code, Some(3 | 113))
        || out.stderr.contains("Could not find service")
        || out.stderr.contains("No such process")
}

/// Unload the agent and wait until launchd has let go of it (the daemon may take a while to
/// drain). `Ok(false)` when it was not loaded. Any other failure is an error unless the job turns
/// out to be gone anyway, so a still-running daemon is never mistaken for a stopped one.
fn unload(env: &Env, r: &dyn Runner) -> anyhow::Result<bool> {
    if is_loaded(env, r) {
        eprintln!("stopping the running service...");
    }
    let out = r
        .run("launchctl", &["bootout", &target(env)])
        .context("running `launchctl`: is it on PATH?")?;
    if out.success {
        let gone = env.timing.wait_until_gone(|| is_loaded(env, r));
        return if gone {
            Ok(true)
        } else {
            Err(anyhow!(
                "the pooltime service is still loaded {}s after `launchctl bootout`",
                env.timing.unload_timeout.as_secs()
            ))
        };
    }
    if not_loaded(&out) {
        return Ok(false);
    }
    if is_loaded(env, r) {
        let why = out.stderr.trim();
        return Err(anyhow!(
            "`launchctl bootout {}` failed ({}) and the service is still loaded{}{}",
            target(env),
            out.code.map_or("signal".into(), |c| format!("exit {c}")),
            if why.is_empty() { "" } else { ": " },
            why
        ));
    }
    Ok(false)
}

/// Write the plist atomically and (re)load it. An already-loaded definition is unloaded first
/// (and waited for) so the new one takes effect; the daemon restarts as part of that. A load that
/// fails is retried: launchd can still be tearing the old job down.
pub fn install(env: &Env, r: &dyn Runner) -> anyhow::Result<()> {
    let bin = env.service_bin()?;
    env.paths
        .ensure()
        .context("creating the pooltime data dir")?; // launchd opens daemon.out/.err
    let path = plist_path(env);
    fs::create_dir_all(path.parent().expect("plist has a parent"))
        .with_context(|| format!("creating {}", path.display()))?;
    atomic_write(
        &path,
        render_launchd_plist(&bin, &env.paths).as_bytes(),
        0o644,
    )
    .with_context(|| format!("writing {}", path.display()))?;
    unload(env, r)?;
    let mut attempt = 1;
    loop {
        match run_checked(
            r,
            "launchctl",
            &["bootstrap", &domain(env), &path.to_string_lossy()],
        ) {
            Ok(_) => return Ok(()),
            Err(e) if attempt >= BOOTSTRAP_ATTEMPTS => return Err(e),
            Err(_) => {
                attempt += 1;
                std::thread::sleep(env.timing.retry_delay);
            }
        }
    }
}

/// Unload and delete the agent. `Ok(false)` when there was no plist. If the job cannot be
/// unloaded the plist is kept (it is the only record of what is running) and this is an error.
pub fn uninstall(env: &Env, r: &dyn Runner) -> anyhow::Result<bool> {
    unload(env, r)?;
    let path = plist_path(env);
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

pub fn status(env: &Env, r: &dyn Runner) -> anyhow::Result<Status> {
    let out = r
        .run("launchctl", &["print", &target(env)])
        .context("running `launchctl`")?;
    let detail = out
        .stdout
        .lines()
        .map(str::trim)
        .filter(|l| {
            ["state = ", "pid = ", "last exit code = "]
                .iter()
                .any(|k| l.starts_with(k))
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Status {
        installed: plist_path(env).exists(),
        running: out.success && out.stdout.lines().any(|l| l.trim() == "state = running"),
        detail,
    })
}

/// Kill and restart the loaded agent in place (the supervisor too: in-flight requests drain
/// first, and new ones get 503 meanwhile).
pub fn restart(env: &Env, r: &dyn Runner) -> anyhow::Result<()> {
    run_checked(r, "launchctl", &["kickstart", "-k", &target(env)])
        .context("is the service installed? try `pooltime service install`")?;
    Ok(())
}

/// Ask the running supervisor for a new worker generation (`SIGHUP` to the job's main process).
pub fn reload(env: &Env, r: &dyn Runner) -> anyhow::Result<()> {
    run_checked(r, "launchctl", &["kill", "SIGHUP", &target(env)])?;
    Ok(())
}
