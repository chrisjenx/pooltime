//! Linux: a systemd user unit, `$XDG_CONFIG_HOME/systemd/user/pooltime.service`
//! (default `~/.config/systemd/user`), managed with `systemctl --user`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};

use super::{run_checked, stable_bin, CmdOutput, Env, Runner, Status};
use crate::config::Paths;
use crate::fsutil::atomic_write;

pub const UNIT: &str = "pooltime.service";

pub fn unit_path(env: &Env) -> PathBuf {
    let config = env
        .xdg_config_home
        .clone()
        .unwrap_or_else(|| env.home.join(".config"));
    config.join("systemd/user").join(UNIT)
}

/// A value for a unit file word: bare when it is plain, else double-quoted with `\` and `"`
/// escaped. `%` is a specifier introducer in unit files, so it is doubled; in `ExecStart=` `$`
/// introduces environment substitution and is doubled too.
fn word(s: &str, exec: bool) -> String {
    let mut esc = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => esc.push_str("\\\\"),
            '"' => esc.push_str("\\\""),
            '%' => esc.push_str("%%"),
            '$' if exec => esc.push_str("$$"),
            c => esc.push(c),
        }
    }
    let plain = !s.is_empty() && !s.chars().any(|c| c.is_whitespace() || "\"'\\".contains(c));
    if plain {
        esc
    } else {
        format!("\"{esc}\"")
    }
}

/// The user unit for `bin serve`. A Homebrew Cellar path is replaced by the stable
/// `<prefix>/bin/pooltime` symlink.
pub fn render_systemd_unit(bin: &Path, paths: &Paths) -> String {
    let bin = word(&stable_bin(bin).to_string_lossy(), true);
    let home = word(
        &format!("POOLTIME_HOME={}", paths.home.to_string_lossy()),
        false,
    );
    format!(
        "[Unit]
Description=pooltime: pooled Claude and Codex accounts behind one local endpoint
After=network.target

[Service]
Type=simple
ExecStart={bin} serve
Environment={home}
Restart=on-failure
RestartSec=10
TimeoutStopSec=600

[Install]
WantedBy=default.target
"
    )
}

fn systemctl(r: &dyn Runner, args: &[&str]) -> anyhow::Result<()> {
    let mut full = vec!["--user"];
    full.extend_from_slice(args);
    run_checked(r, "systemctl", &full).map(|_| ())
}

/// Write the unit atomically, enable it and start it. Re-installing over an existing unit also
/// restarts it so the running daemon picks up the new definition.
pub fn install(env: &Env, r: &dyn Runner) -> anyhow::Result<()> {
    let bin = env.service_bin()?;
    env.paths
        .ensure()
        .context("creating the pooltime data dir")?;
    let path = unit_path(env);
    let existed = path.exists();
    fs::create_dir_all(path.parent().expect("unit has a parent"))
        .with_context(|| format!("creating {}", path.display()))?;
    atomic_write(
        &path,
        render_systemd_unit(&bin, &env.paths).as_bytes(),
        0o644,
    )
    .with_context(|| format!("writing {}", path.display()))?;
    systemctl(r, &["daemon-reload"])?;
    systemctl(r, &["enable", "--now", UNIT])?;
    if existed {
        systemctl(r, &["restart", UNIT])?;
    }
    Ok(())
}

fn is_active(r: &dyn Runner) -> bool {
    r.run("systemctl", &["--user", "is-active", UNIT])
        .is_ok_and(|o| o.success)
}

/// `systemctl disable` says there is no such unit.
fn unit_absent(out: &CmdOutput) -> bool {
    out.code == Some(5)
        || ["not loaded", "does not exist", "not found", "No such file"]
            .iter()
            .any(|m| out.stderr.contains(m))
}

/// Stop and disable the unit. Anything but "no such unit" is an error unless the unit turns out
/// not to be running, so a still-running daemon is never mistaken for a stopped one.
fn unload(r: &dyn Runner) -> anyhow::Result<()> {
    if is_active(r) {
        eprintln!("stopping the running service...");
    }
    let out = r
        .run("systemctl", &["--user", "disable", "--now", UNIT])
        .context("running `systemctl`: is it on PATH?")?;
    if out.success || unit_absent(&out) || !is_active(r) {
        return Ok(());
    }
    let why = out.stderr.trim();
    Err(anyhow!(
        "`systemctl --user disable --now {UNIT}` failed ({}) and the unit is still active{}{}",
        out.code.map_or("signal".into(), |c| format!("exit {c}")),
        if why.is_empty() { "" } else { ": " },
        why
    ))
}

/// Stop, disable and delete the unit. `Ok(false)` when there was no unit file. If the unit cannot
/// be stopped the file is kept and this is an error.
pub fn uninstall(env: &Env, r: &dyn Runner) -> anyhow::Result<bool> {
    unload(r)?;
    let path = unit_path(env);
    let removed = match fs::remove_file(&path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
    };
    if removed {
        systemctl(r, &["daemon-reload"])?;
    }
    Ok(removed)
}

pub fn status(env: &Env, r: &dyn Runner) -> anyhow::Result<Status> {
    let out = r
        .run("systemctl", &["--user", "status", UNIT])
        .context("running `systemctl`")?;
    Ok(Status {
        installed: unit_path(env).exists(),
        running: out.success,
        detail: out.stdout.trim_end().to_string(),
    })
}

pub fn restart(_env: &Env, r: &dyn Runner) -> anyhow::Result<()> {
    systemctl(r, &["restart", UNIT])
        .context("is the service installed? try `pooltime service install`")
}
