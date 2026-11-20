//! macOS: a launchd user agent, `~/Library/LaunchAgents/dev.pooltime.daemon.plist`, loaded into
//! the user's GUI domain (`gui/<uid>`) with the modern `bootstrap`/`bootout` verbs.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};

use super::{
    run_checked, stable_bin, CmdOutput, Env, Interrupts, Runner, Status, BOOTSTRAP_ATTEMPTS,
};
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
    is_job_loaded(env, r) == Some(true)
}

/// `launchctl print`: `Some(true)` loaded, `Some(false)` not; `None` when it could not tell
/// (it did not run, or a signal ended it), which a wait for the job to go must not read as gone.
fn is_job_loaded(env: &Env, r: &dyn Runner) -> Option<bool> {
    let out = r.run("launchctl", &["print", &target(env)]).ok()?;
    out.code?;
    Some(out.success)
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
    match unload_waiting(env, r, &Interrupts::none())? {
        Unloaded::Gone => Ok(true),
        Unloaded::NotLoaded => Ok(false),
        Unloaded::StillLoaded(why) => Err(anyhow!("{why}")),
    }
}

/// How an unload ended.
enum Unloaded {
    Gone,
    NotLoaded,
    /// `bootout` was accepted but the job is still there (draining): the wait timed out or was
    /// cut short.
    StillLoaded(String),
}

/// `launchctl bootout`, then wait (with progress) until the job is gone. A second Ctrl-C (seen
/// by `interrupts`) stops the wait early.
fn unload_waiting(env: &Env, r: &dyn Runner, interrupts: &Interrupts) -> anyhow::Result<Unloaded> {
    if is_loaded(env, r) {
        eprintln!(
            "stopping the running service: it finishes its in-flight requests first (up to 10 \
             minutes)"
        );
    }
    let out = r
        .run("launchctl", &["bootout", &target(env)])
        .context("running `launchctl`: is it on PATH?")?;
    if out.success {
        let mut warned = 0;
        let gone = env.timing.wait_until_gone_with(
            || is_job_loaded(env, r) != Some(false),
            |waited| {
                let n = interrupts.count();
                if n > warned {
                    warned = n;
                    if n == 1 {
                        eprintln!(
                            "still waiting, so the new service can be loaded once the old one \
                             is gone; Ctrl-C again stops waiting (then run `pooltime service \
                             install` once the old daemon has exited)"
                        );
                    }
                }
                if n >= 2 {
                    return false;
                }
                if waited.as_secs() > 0 && waited.as_secs() % 15 == 0 {
                    eprintln!(
                        "  ... still waiting for in-flight requests ({}s)",
                        waited.as_secs()
                    );
                }
                true
            },
        );
        return Ok(if gone {
            Unloaded::Gone
        } else {
            Unloaded::StillLoaded(format!(
                "the pooltime service is still loaded {}s after `launchctl bootout`",
                env.timing.unload_timeout.as_secs()
            ))
        });
    }
    if not_loaded(&out) {
        return Ok(Unloaded::NotLoaded);
    }
    if is_job_loaded(env, r) != Some(false) {
        // Not an error yet: `install` still tries to load the new definition (and says what to
        // do if launchd refuses); `uninstall` reports it and keeps the plist.
        let why = out.stderr.trim();
        return Ok(Unloaded::StillLoaded(format!(
            "`launchctl bootout {}` failed ({}) and the service is still loaded{}{}",
            target(env),
            out.code.map_or("signal".into(), |c| format!("exit {c}")),
            if why.is_empty() { "" } else { ": " },
            why
        )));
    }
    Ok(Unloaded::NotLoaded)
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
    // A Ctrl-C while the old daemon drains must not leave the new definition unloaded: keep
    // waiting, and load it on every way out of the wait.
    let interrupts = Interrupts::catch();
    let still = match unload_waiting(env, r, &interrupts)? {
        Unloaded::Gone | Unloaded::NotLoaded => None,
        Unloaded::StillLoaded(why) => Some(why),
    };
    let mut attempt = 1;
    loop {
        match run_checked(
            r,
            "launchctl",
            &["bootstrap", &domain(env), &path.to_string_lossy()],
        ) {
            Ok(_) => return Ok(()),
            Err(e) if attempt >= BOOTSTRAP_ATTEMPTS => {
                return Err(match still {
                    Some(why) => anyhow!(
                        "{why}, so the new definition could not be loaded yet ({e:#}); run \
                         `pooltime service install` again once the old daemon has exited"
                    ),
                    None => e,
                })
            }
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

/// What `launchctl print` says about the loaded job: the parts of it `service install` compares
/// with the plist it would write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Job {
    pub pid: Option<u32>,
    /// `arguments = { ... }`; `None` when the block is missing.
    pub arguments: Option<Vec<String>>,
    /// `environment = { KEY => value ... }` (the job's own, not the inherited or default one).
    pub environment: Vec<(String, String)>,
    pub stdout_path: Option<String>,
    pub stderr_path: Option<String>,
}

/// Parse `launchctl print <job>`: top-level lines are indented by one tab, the entries of a
/// block by two.
pub fn parse_print(out: &str) -> Job {
    let mut job = Job::default();
    let mut block: Option<&str> = None;
    for line in out.lines() {
        if let Some(b) = block {
            if line == "\t}" {
                block = None;
                continue;
            }
            let Some(entry) = line.strip_prefix("\t\t") else {
                continue;
            };
            match b {
                "arguments" => job
                    .arguments
                    .get_or_insert_with(Vec::new)
                    .push(entry.into()),
                "environment" => {
                    if let Some((k, v)) = entry.split_once(" => ") {
                        job.environment.push((k.into(), v.into()));
                    }
                }
                _ => {}
            }
            continue;
        }
        let Some(top) = line.strip_prefix('\t').filter(|l| !l.starts_with('\t')) else {
            continue;
        };
        if let Some(name) = top.strip_suffix(" = {") {
            block = Some(match name {
                "arguments" => {
                    job.arguments.get_or_insert_with(Vec::new);
                    "arguments"
                }
                "environment" => "environment",
                _ => "other",
            });
        } else if let Some(p) = top.strip_prefix("pid = ") {
            job.pid = p.trim().parse().ok();
        } else if let Some(p) = top.strip_prefix("stdout path = ") {
            job.stdout_path = Some(p.into());
        } else if let Some(p) = top.strip_prefix("stderr path = ") {
            job.stderr_path = Some(p.into());
        }
    }
    job
}

/// Whether the loaded `job` runs what [`render_launchd_plist`] writes for `bin`: the same
/// arguments, `POOLTIME_HOME` and log paths. The other keys (`KeepAlive`, `ThrottleInterval`,
/// `ExitTimeOut`, ...) are not shown by `launchctl print` in a comparable form: a plist edited
/// by hand there and not reloaded is not noticed.
pub fn job_matches(job: &Job, bin: &Path, paths: &Paths) -> bool {
    let bin = stable_bin(bin).to_string_lossy().into_owned();
    let s = |p: &Path| p.to_string_lossy().into_owned();
    job.arguments.as_deref() == Some(&[bin, "serve".into(), "--supervise".into()][..])
        && job
            .environment
            .iter()
            .any(|(k, v)| k == "POOLTIME_HOME" && *v == s(&paths.home))
        && job.stdout_path.as_deref() == Some(s(&paths.logs.join("daemon.out")).as_str())
        && job.stderr_path.as_deref() == Some(s(&paths.logs.join("daemon.err")).as_str())
}

/// The job's pid, and whether the loaded job differs from what the plist for `bin` would load
/// (also when `launchctl print` fails or shows nothing comparable).
pub fn loaded(env: &Env, r: &dyn Runner, bin: &Path) -> (Option<u32>, bool) {
    let Ok(out) = r.run("launchctl", &["print", &target(env)]) else {
        return (None, true);
    };
    if !out.success {
        return (None, true);
    }
    let job = parse_print(&out.stdout);
    (job.pid, !job_matches(&job, bin, &env.paths))
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

#[cfg(test)]
mod tests {
    use super::*;

    const PRINT: &str = "gui/501/dev.pooltime.daemon = {
\tactive count = 2
\tpath = /Users/u/Library/LaunchAgents/dev.pooltime.daemon.plist
\tstate = running

\tprogram = /opt/homebrew/bin/pooltime
\targuments = {
\t\t/opt/homebrew/bin/pooltime
\t\tserve
\t\t--supervise
\t}

\tstdout path = /Users/u/.pooltime/logs/daemon.out
\tstderr path = /Users/u/.pooltime/logs/daemon.err
\tinherited environment = {
\t\tSSH_AUTH_SOCK => /var/run/x/Listeners
\t}

\tdefault environment = {
\t\tPATH => /usr/bin:/bin:/usr/sbin:/sbin
\t}

\tenvironment = {
\t\tPOOLTIME_HOME => /Users/u/.pooltime
\t\tXPC_SERVICE_NAME => dev.pooltime.daemon
\t}

\tpid = 4242
\tevent channels = {
\t\t\"com.apple.x\" = {
\t\t\tpid = 9
\t\t}
\t}
}
";

    #[test]
    fn the_loaded_job_is_compared_with_the_plist() {
        let job = parse_print(PRINT);
        assert_eq!(job.pid, Some(4242));
        assert_eq!(
            job.arguments.as_deref(),
            Some(
                &[
                    "/opt/homebrew/bin/pooltime".to_string(),
                    "serve".into(),
                    "--supervise".into()
                ][..]
            )
        );
        let paths = Paths::under(Path::new("/Users/u/.pooltime"));
        let bin = Path::new("/opt/homebrew/Cellar/pooltime/1.4.0/bin/pooltime");
        assert!(job_matches(&job, bin, &paths));
        // Another binary, data dir or argument list: changed.
        assert!(!job_matches(
            &job,
            Path::new("/usr/local/bin/pooltime"),
            &paths
        ));
        assert!(!job_matches(&job, bin, &Paths::under(Path::new("/tmp/pt"))));
        let plain = parse_print(&PRINT.replace("\t\t--supervise\n", ""));
        assert!(!job_matches(&plain, bin, &paths));
        // POOLTIME_HOME only inherited (not the job's own): changed.
        let inherited = PRINT
            .replace("\t\tPOOLTIME_HOME => /Users/u/.pooltime\n", "")
            .replace(
                "SSH_AUTH_SOCK => /var/run/x/Listeners",
                "POOLTIME_HOME => /Users/u/.pooltime",
            );
        assert!(!job_matches(&parse_print(&inherited), bin, &paths));
        // Nothing comparable (an unknown print format): changed.
        assert!(!job_matches(
            &parse_print("state = running\npid = 1\n"),
            bin,
            &paths
        ));
    }
}
