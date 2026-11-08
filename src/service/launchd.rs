//! macOS: a launchd user agent, `~/Library/LaunchAgents/dev.pooltime.daemon.plist`, loaded into
//! the user's GUI domain (`gui/<uid>`) with the modern `bootstrap`/`bootout` verbs.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;

use super::{run_checked, stable_bin, Env, Runner, Status};
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

/// The launch agent definition for `bin serve`. A Homebrew Cellar path is replaced by the stable
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

/// Write the plist atomically and (re)load it. An already-loaded definition is unloaded first so
/// the new one takes effect; the daemon restarts as part of that.
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
    // Not loaded yet is the common case; a failure here is expected and harmless.
    let _ = r.run("launchctl", &["bootout", &target(env)]);
    run_checked(
        r,
        "launchctl",
        &["bootstrap", &domain(env), &path.to_string_lossy()],
    )?;
    Ok(())
}

/// Unload and delete the agent. `Ok(false)` when there was no plist (still unloads, in case a
/// definition was loaded from elsewhere).
pub fn uninstall(env: &Env, r: &dyn Runner) -> anyhow::Result<bool> {
    // Fails when it is not loaded; either way it is gone afterwards.
    let _ = r.run("launchctl", &["bootout", &target(env)]);
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

/// Kill and restart the loaded agent in place.
pub fn restart(env: &Env, r: &dyn Runner) -> anyhow::Result<()> {
    run_checked(r, "launchctl", &["kickstart", "-k", &target(env)])
        .context("is the service installed? try `pooltime service install`")?;
    Ok(())
}
