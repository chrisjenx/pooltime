//! Task 31: the launchd / systemd user service. Render functions are pure, so their snapshots run
//! on every platform; the install/uninstall flows run against a recording runner or a PATH shim
//! that stands in for `launchctl` / `systemctl`. Nothing here touches the real `$HOME`, the real
//! `~/Library/LaunchAgents` or `~/.config`, or the real service managers.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use pooltime::claude_code::{apply, is_connected, profile_of, EnvBlock, Pins, Profile};
use pooltime::cli::service::{run_with, ServiceCmd};
use pooltime::config::{Config, Paths, ProfileRef};
use pooltime::service::{
    launchd, render_launchd_plist, render_systemd_unit, stable_bin, systemd, Backend, CmdOutput,
    Env, Runner, Timing,
};
use serde_json::{json, Value};

fn fake_paths() -> Paths {
    Paths::under(Path::new("/Users/test/.pooltime"))
}

#[test]
fn plist_snapshot() {
    insta::assert_snapshot!(render_launchd_plist(
        Path::new("/usr/local/bin/pooltime"),
        &fake_paths()
    ));
}

#[test]
fn systemd_unit_snapshot() {
    insta::assert_snapshot!(render_systemd_unit(
        Path::new("/usr/local/bin/pooltime"),
        &fake_paths()
    ));
}

#[test]
fn cellar_path_maps_to_stable_symlink() {
    let cellar = |p: &str| stable_bin(Path::new(p));
    assert_eq!(
        cellar("/opt/homebrew/Cellar/pooltime/0.1.0/bin/pooltime"),
        PathBuf::from("/opt/homebrew/bin/pooltime")
    );
    assert_eq!(
        cellar("/home/linuxbrew/.linuxbrew/Cellar/pooltime/1.2.3_1/bin/pooltime"),
        PathBuf::from("/home/linuxbrew/.linuxbrew/bin/pooltime")
    );
    // Anything else is left alone.
    for p in [
        "/usr/local/bin/pooltime",
        "/Users/x/.cargo/bin/pooltime",
        "/opt/homebrew/Cellar/other/1.0/bin/pooltime",
        "/opt/homebrew/Cellar/pooltime/0.1.0/libexec/pooltime",
        "/Users/x/Cellar/pooltime",
    ] {
        assert_eq!(cellar(p), PathBuf::from(p), "{p}");
    }
    // Both renderers use it.
    let cellar_bin = Path::new("/opt/homebrew/Cellar/pooltime/0.1.0/bin/pooltime");
    assert!(render_launchd_plist(cellar_bin, &fake_paths())
        .contains("<string>/opt/homebrew/bin/pooltime</string>"));
    assert!(render_systemd_unit(cellar_bin, &fake_paths())
        .contains("ExecStart=/opt/homebrew/bin/pooltime serve\n"));
}

#[test]
fn plist_escapes_xml_and_unit_quotes_spaces() {
    let paths = Paths::under(Path::new("/Users/a & b/<pool>/.pooltime"));
    let plist = render_launchd_plist(Path::new("/opt/x y/pooltime"), &paths);
    assert!(plist.contains("<string>/Users/a &amp; b/&lt;pool&gt;/.pooltime</string>"));
    assert!(plist.contains("<string>/opt/x y/pooltime</string>"));
    assert!(!plist.contains("a & b"));

    let paths = Paths::under(Path::new("/home/a b/.pool\"time%"));
    let unit = render_systemd_unit(Path::new("/opt/x y/pooltime"), &paths);
    assert!(
        unit.contains("ExecStart=\"/opt/x y/pooltime\" serve\n"),
        "{unit}"
    );
    assert!(
        unit.contains("Environment=\"POOLTIME_HOME=/home/a b/.pool\\\"time%%\"\n"),
        "{unit}"
    );
}

// ---- recording runner -------------------------------------------------------------------------

/// A scripted `launchctl` / `systemctl`: records every call and models whether the job is loaded
/// (launchd) or active (systemd).
#[derive(Default)]
struct Rec {
    calls: Mutex<Vec<String>>,
    /// Command prefixes that fail with exit 1 / stderr "boom" and change no state.
    fail: Vec<&'static str>,
    /// launchd: the job is loaded.
    loaded: Mutex<bool>,
    /// systemd: the unit is active.
    active: Mutex<bool>,
    /// launchctl bootout succeeds but the job stays loaded.
    stuck: bool,
    /// The next N `launchctl bootstrap` calls fail.
    bootstrap_failures: Mutex<u32>,
    /// systemctl disable reports the unit as unknown.
    missing_unit: bool,
}

impl Rec {
    fn launchd_loaded() -> Rec {
        Rec {
            loaded: Mutex::new(true),
            ..Rec::default()
        }
    }

    fn systemd_active() -> Rec {
        Rec {
            active: Mutex::new(true),
            ..Rec::default()
        }
    }

    /// The calls so far (and forget them).
    fn calls(&self) -> Vec<String> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }
}

fn reply(code: i32, stdout: &str, stderr: &str) -> io::Result<CmdOutput> {
    Ok(CmdOutput {
        success: code == 0,
        code: Some(code),
        stdout: stdout.into(),
        stderr: stderr.into(),
    })
}

impl Runner for Rec {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<CmdOutput> {
        let line = format!("{program} {}", args.join(" "));
        self.calls.lock().unwrap().push(line.clone());
        if self.fail.iter().any(|f| line.starts_with(f)) {
            return reply(1, "", "boom");
        }
        let verb = args.iter().find(|a| !a.starts_with("--")).unwrap_or(&"");
        let mut loaded = self.loaded.lock().unwrap();
        let mut active = self.active.lock().unwrap();
        match (program, *verb) {
            ("launchctl", "bootstrap") => {
                let mut left = self.bootstrap_failures.lock().unwrap();
                if *left > 0 {
                    *left -= 1;
                    return reply(5, "", "Bootstrap failed: 5: Input/output error");
                }
                *loaded = true;
                reply(0, "", "")
            }
            ("launchctl", "bootout") if *loaded => {
                *loaded = self.stuck;
                reply(0, "", "")
            }
            ("launchctl", "bootout") => {
                reply(113, "", "Boot-out failed: 113: Could not find service")
            }
            ("launchctl", "print") if *loaded => reply(0, "\tstate = running\n\tpid = 4242\n", ""),
            ("launchctl", "print") => reply(113, "", "Could not find service"),
            ("systemctl", "enable") => {
                *active = true;
                reply(0, "", "")
            }
            ("systemctl", "disable") if self.missing_unit => reply(
                1,
                "",
                "Failed to disable unit: Unit file pooltime.service does not exist.",
            ),
            ("systemctl", "disable") => {
                *active = false;
                reply(0, "", "")
            }
            ("systemctl", "is-active") if *active => reply(0, "active\n", ""),
            ("systemctl", "is-active") => reply(3, "inactive\n", ""),
            ("systemctl", "status") if *active => reply(0, "Active: active (running)\n", ""),
            ("systemctl", "status") => reply(3, "Active: inactive (dead)\n", ""),
            _ => reply(0, "", ""),
        }
    }
}

fn env_in(root: &Path) -> Env {
    Env {
        paths: Paths::under(&root.join("pt")),
        bin: PathBuf::from("/usr/local/bin/pooltime"),
        home: root.join("home"),
        xdg_config_home: None,
        uid: 501,
        timing: Timing::instant(),
    }
}

const BOOTOUT: &str = "launchctl bootout gui/501/dev.pooltime.daemon";
const PRINT: &str = "launchctl print gui/501/dev.pooltime.daemon";

#[test]
fn launchd_install_writes_plist_bootstraps_and_reinstall_replaces() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_in(dir.path());
    let rec = Rec::default();
    let plist = dir
        .path()
        .join("home/Library/LaunchAgents/dev.pooltime.daemon.plist");
    assert_eq!(launchd::plist_path(&env), plist);

    launchd::install(&env, &rec).unwrap();
    let body = fs::read_to_string(&plist).unwrap();
    assert_eq!(body, render_launchd_plist(&env.bin, &env.paths));
    assert!(
        env.paths.logs.is_dir(),
        "log dir exists before launchd opens daemon.out"
    );
    let boot = format!("launchctl bootstrap gui/501 {}", plist.display());
    // Not loaded: bootout reports "could not find service", which is fine.
    assert_eq!(rec.calls(), [PRINT, BOOTOUT, &boot]);

    // Re-install: unload the old definition, wait until it is gone, then load the new one.
    launchd::install(&env, &rec).unwrap();
    assert_eq!(rec.calls(), [PRINT, BOOTOUT, PRINT, &boot]);
}

#[test]
fn launchd_install_waits_for_the_unload_and_retries_bootstrap() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_in(dir.path());

    // Two transient bootstrap failures, then success.
    let rec = Rec {
        bootstrap_failures: Mutex::new(2),
        ..Rec::default()
    };
    launchd::install(&env, &rec).unwrap();
    let boots = |rec: &Rec| {
        rec.calls()
            .iter()
            .filter(|c| c.starts_with("launchctl bootstrap"))
            .count()
    };
    assert_eq!(boots(&rec), 3);

    // Always failing: gives up after three attempts, with the tool's message.
    let rec = Rec {
        fail: vec!["launchctl bootstrap"],
        ..Rec::default()
    };
    let err = launchd::install(&env, &rec).unwrap_err();
    assert!(format!("{err:#}").contains("boom"), "{err:#}");
    assert_eq!(boots(&rec), 3);

    // A job that will not unload is an error, and nothing is bootstrapped over it.
    let rec = Rec {
        stuck: true,
        ..Rec::launchd_loaded()
    };
    let err = launchd::install(&env, &rec).unwrap_err();
    assert!(format!("{err:#}").contains("still loaded"), "{err:#}");
    assert_eq!(boots(&rec), 0);
}

#[test]
fn launchd_install_stops_on_a_real_bootout_failure() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_in(dir.path());
    let rec = Rec {
        fail: vec!["launchctl bootout"],
        ..Rec::launchd_loaded()
    };
    let err = launchd::install(&env, &rec).unwrap_err();
    assert!(format!("{err:#}").contains("boom"), "{err:#}");
    assert!(!rec.calls().iter().any(|c| c.contains("bootstrap")));
}

#[test]
fn launchd_status_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_in(dir.path());
    launchd::install(&env, &Rec::default()).unwrap();

    let rec = Rec::launchd_loaded();
    let st = launchd::status(&env, &rec).unwrap();
    assert!(st.installed && st.running, "{st:?}");
    assert!(st.detail.contains("pid = 4242"), "{st:?}");
    assert_eq!(rec.calls(), [PRINT]);

    launchd::restart(&env, &rec).unwrap();
    assert_eq!(
        rec.calls(),
        ["launchctl kickstart -k gui/501/dev.pooltime.daemon"]
    );
}

#[test]
fn launchd_uninstall_status_restart() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_in(dir.path());
    let plist = launchd::plist_path(&env);
    launchd::install(&env, &Rec::default()).unwrap();

    // Loaded: bootout, wait until gone, delete.
    let rec = Rec::launchd_loaded();
    assert!(launchd::uninstall(&env, &rec).unwrap());
    assert_eq!(rec.calls(), [PRINT, BOOTOUT, PRINT]);
    assert!(!plist.exists());

    // "Not loaded" (exit 113 / "Could not find service") is benign; removing again is a no-op.
    launchd::install(&env, &Rec::default()).unwrap();
    let rec = Rec::default();
    assert!(launchd::uninstall(&env, &rec).unwrap());
    assert!(!plist.exists());
    assert!(!launchd::uninstall(&env, &rec).unwrap());
}

#[test]
fn launchd_uninstall_failure_keeps_the_definition() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_in(dir.path());
    let plist = launchd::plist_path(&env);
    launchd::install(&env, &Rec::default()).unwrap();

    // bootout fails and the job is still loaded: error, plist kept.
    let rec = Rec {
        fail: vec!["launchctl bootout"],
        ..Rec::launchd_loaded()
    };
    let err = launchd::uninstall(&env, &rec).unwrap_err();
    assert!(format!("{err:#}").contains("boom"), "{err:#}");
    assert_eq!(rec.calls(), [PRINT, BOOTOUT, PRINT]);
    assert!(plist.exists(), "a loaded job keeps its definition");

    // bootout succeeds but the job lingers: also an error.
    let rec = Rec {
        stuck: true,
        ..Rec::launchd_loaded()
    };
    assert!(launchd::uninstall(&env, &rec).is_err());
    assert!(plist.exists());

    // bootout fails oddly but the job is gone anyway: fine.
    let rec = Rec {
        fail: vec!["launchctl bootout"],
        ..Rec::default()
    };
    assert!(launchd::uninstall(&env, &rec).unwrap());
    assert!(!plist.exists());
}

#[test]
fn systemd_install_enables_and_reinstall_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let mut env = env_in(dir.path());
    assert_eq!(
        systemd::unit_path(&env),
        dir.path()
            .join("home/.config/systemd/user/pooltime.service")
    );
    env.xdg_config_home = Some(dir.path().join("xdg"));
    let unit = dir.path().join("xdg/systemd/user/pooltime.service");
    assert_eq!(
        systemd::unit_path(&env),
        unit,
        "XDG_CONFIG_HOME is respected"
    );

    let rec = Rec::default();
    systemd::install(&env, &rec).unwrap();
    assert_eq!(
        fs::read_to_string(&unit).unwrap(),
        render_systemd_unit(&env.bin, &env.paths)
    );
    assert_eq!(
        rec.calls(),
        [
            "systemctl --user daemon-reload",
            "systemctl --user enable --now pooltime.service"
        ]
    );

    systemd::install(&env, &rec).unwrap();
    assert_eq!(
        rec.calls(),
        [
            "systemctl --user daemon-reload",
            "systemctl --user enable --now pooltime.service",
            "systemctl --user restart pooltime.service"
        ],
        "a replaced unit is picked up by the running daemon"
    );

    let st = systemd::status(&env, &rec).unwrap();
    assert!(st.installed && st.running);
    assert_eq!(rec.calls(), ["systemctl --user status pooltime.service"]);

    systemd::restart(&env, &rec).unwrap();
    assert_eq!(rec.calls(), ["systemctl --user restart pooltime.service"]);

    assert!(systemd::uninstall(&env, &rec).unwrap());
    assert!(!unit.exists());
    assert_eq!(
        rec.calls(),
        [
            "systemctl --user is-active pooltime.service",
            "systemctl --user disable --now pooltime.service",
            "systemctl --user daemon-reload"
        ]
    );
}

#[test]
fn systemd_uninstall_failure_policy() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_in(dir.path());
    let unit = systemd::unit_path(&env);
    systemd::install(&env, &Rec::default()).unwrap();

    // disable fails and the unit is still active: error, unit file kept.
    let rec = Rec {
        fail: vec!["systemctl --user disable"],
        ..Rec::systemd_active()
    };
    let err = systemd::uninstall(&env, &rec).unwrap_err();
    assert!(format!("{err:#}").contains("boom"), "{err:#}");
    assert!(unit.exists());

    // An unknown unit ("does not exist") is benign.
    let rec = Rec {
        missing_unit: true,
        ..Rec::default()
    };
    assert!(systemd::uninstall(&env, &rec).unwrap());
    assert!(!unit.exists());
    assert!(!systemd::uninstall(&env, &rec).unwrap());

    // A failure for another reason, with the unit not active: nothing left running, so proceed.
    systemd::install(&env, &Rec::default()).unwrap();
    let rec = Rec {
        fail: vec!["systemctl --user disable"],
        ..Rec::default()
    };
    assert!(systemd::uninstall(&env, &rec).unwrap());
    assert!(!unit.exists());
}

#[test]
fn newlines_in_paths_are_rejected() {
    for (bin, home) in [
        ("/usr/local/bin/pool\ntime", "pt"),
        ("/usr/local/bin/pooltime", "p\nt"),
        ("/usr/local/bin/pool\rtime", "pt"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut env = env_in(dir.path());
        env.bin = PathBuf::from(bin);
        env.paths = Paths::under(&dir.path().join(home));
        for backend in [Backend::Launchd, Backend::Systemd] {
            let rec = Rec::default();
            let err = backend.install(&env, &rec).unwrap_err();
            assert!(format!("{err:#}").contains("newline"), "{err:#}");
            assert!(rec.calls().is_empty());
            assert!(!backend.definition_path(&env).exists());
        }
    }
}

#[test]
fn cli_uninstall_reports_an_unload_failure_and_keeps_the_definition() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_in(dir.path());
    let plist = launchd::plist_path(&env);
    launchd::install(&env, &Rec::default()).unwrap();
    let rec = Rec {
        fail: vec!["launchctl bootout"],
        ..Rec::launchd_loaded()
    };
    let code = run_with(Backend::Launchd, &env, &rec, ServiceCmd::Uninstall).unwrap();
    assert_eq!(code, 1);
    assert!(plist.exists());
}

// ---- the binary, with a PATH shim -------------------------------------------------------------

/// Stateful `launchctl` and `systemctl` shims: they append their arguments to `calls.log` and
/// track "loaded"/"active" in a state file, like the real tools' observable behaviour.
fn shim_dir(root: &Path) -> (PathBuf, PathBuf) {
    let dir = root.join("shims");
    fs::create_dir_all(&dir).unwrap();
    let log = root.join("calls.log");
    let state = root.join("loaded");
    let scripts = [
        (
            "launchctl",
            r#"case "$1" in
  bootstrap) touch "$STATE" ;;
  bootout) if [ -f "$STATE" ]; then rm "$STATE"; else echo "Could not find service" >&2; exit 113; fi ;;
  print) [ -f "$STATE" ] || exit 113 ;;
esac
exit 0"#,
        ),
        (
            "systemctl",
            r#"case "$2" in
  enable) touch "$STATE" ;;
  disable) rm -f "$STATE" ;;
  is-active) if [ -f "$STATE" ]; then echo active; else echo inactive; exit 3; fi ;;
esac
exit 0"#,
        ),
    ];
    for (tool, body) in scripts {
        let script = dir.join(tool);
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nSTATE=\"{}\"\necho \"{tool} $*\" >> \"{}\"\n{body}\n",
                state.display(),
                log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    (dir, log)
}

fn pooltime(root: &Path, shims: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_pooltime"));
    c.env("HOME", root.join("home"))
        .env("POOLTIME_HOME", root.join("pt"))
        .env("XDG_CONFIG_HOME", root.join("home/.config"))
        .env("PATH", shims)
        .env_remove("POOLTIME_PORT")
        .env_remove("POOLTIME_HOST");
    c
}

fn json_of(p: &Path) -> Value {
    serde_json::from_slice(&fs::read(p).unwrap()).unwrap()
}

/// Where the service definition goes under the temp home, plus the load/unload lines the shim logs.
fn platform(home: &Path) -> (PathBuf, &'static str, &'static str) {
    if cfg!(target_os = "macos") {
        (
            home.join("Library/LaunchAgents/dev.pooltime.daemon.plist"),
            "launchctl bootstrap gui/",
            "launchctl bootout gui/",
        )
    } else {
        (
            home.join(".config/systemd/user/pooltime.service"),
            "systemctl --user enable --now pooltime.service",
            "systemctl --user disable --now pooltime.service",
        )
    }
}

/// Connect a profile under the temp home: the user's own settings, the pool env block on top,
/// recorded in the config. Returns the profile and its original settings.
fn connected_profile(home: &Path, paths: &Paths, name: &str) -> (Profile, Value) {
    let dir = home.join(name);
    fs::create_dir_all(&dir).unwrap();
    let r = ProfileRef {
        id: name.trim_start_matches('.').into(),
        settings_dir: dir,
    };
    let p = profile_of(&r);
    let original = json!({"model": "opus", "env": {"KEEP": "1"}});
    fs::write(
        &p.settings_json,
        serde_json::to_vec_pretty(&original).unwrap(),
    )
    .unwrap();
    let block = EnvBlock {
        base_url: "http://127.0.0.1:3460".into(),
        auth_token: "k".into(),
        pins: Pins::default(),
        max_context: None,
    };
    apply(&p, &block, paths).unwrap();
    let mut cfg = Config::load_file(paths).unwrap();
    cfg.profiles.push(r);
    cfg.save(paths).unwrap();
    assert!(is_connected(&p, paths));
    (p, original)
}

fn service(root: &Path, shims: &Path, verb: &str) -> std::process::Output {
    crate::support::procs::output(pooltime(root, shims).args(["service", verb]))
}

#[test]
fn uninstall_disconnects_profiles() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let paths = Paths::under(&root.path().join("pt"));
    paths.ensure().unwrap();
    let (shims, log) = shim_dir(root.path());
    let (p, original) = connected_profile(&home, &paths, ".claude-svc");
    assert!(json_of(&p.settings_json)["env"]["ANTHROPIC_BASE_URL"].is_string());

    // install -> definition file + shim calls
    let out = service(root.path(), &shims, "install");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (def, load, unload) = platform(&home);
    assert!(
        def.exists(),
        "service definition written under the temp home"
    );
    let calls = fs::read_to_string(&log).unwrap();
    assert!(calls.contains(load), "{calls}");

    // uninstall -> profile restored, service unloaded and removed
    let out = service(root.path(), &shims, "uninstall");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !is_connected(&p, &paths),
        "profile no longer points at the pool"
    );
    assert_eq!(json_of(&p.settings_json), original, "settings restored");
    assert!(Config::load_file(&paths).unwrap().profiles.is_empty());
    assert!(!def.exists());
    let calls = fs::read_to_string(&log).unwrap();
    assert!(calls.contains(unload), "{calls}");
    assert!(calls.rfind(unload).unwrap() > calls.find(load).unwrap());

    // A second uninstall is a harmless no-op.
    assert!(service(root.path(), &shims, "uninstall").status.success());
}

#[test]
fn uninstall_with_an_unrestorable_profile_exits_1_but_removes_the_service() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let paths = Paths::under(&root.path().join("pt"));
    paths.ensure().unwrap();
    let (shims, log) = shim_dir(root.path());
    let (good, original) = connected_profile(&home, &paths, ".claude-good");
    let (bad, _) = connected_profile(&home, &paths, ".claude-bad");
    assert!(service(root.path(), &shims, "install").status.success());

    // `env` is no longer an object: that profile cannot be restored.
    fs::write(&bad.settings_json, r#"{"env": 5}"#).unwrap();

    let out = service(root.path(), &shims, "uninstall");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not restored"), "{stderr}");
    assert!(
        !stderr.contains("\"k\""),
        "no credentials in the output: {stderr}"
    );
    let (def, _, unload) = platform(&home);
    assert!(!def.exists(), "the service is removed regardless");
    assert!(fs::read_to_string(&log).unwrap().contains(unload));
    assert_eq!(json_of(&good.settings_json), original, "others restored");
    let left = Config::load_file(&paths).unwrap().profiles;
    assert_eq!(left.len(), 1, "the failed profile stays recorded: {left:?}");
    assert_eq!(left[0].id, "claude-bad");
}

#[test]
fn uninstall_aborts_on_a_corrupt_config_without_touching_the_service() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let paths = Paths::under(&root.path().join("pt"));
    paths.ensure().unwrap();
    let (shims, log) = shim_dir(root.path());
    assert!(service(root.path(), &shims, "install").status.success());
    let (def, _, _) = platform(&home);
    let def_before = fs::read(&def).unwrap();
    let calls_before = fs::read_to_string(&log).unwrap();

    fs::write(&paths.config, "this is = not [valid toml").unwrap();
    let out = service(root.path(), &shims, "uninstall");
    assert!(!out.status.success());
    assert_eq!(fs::read(&def).unwrap(), def_before, "definition untouched");
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        calls_before,
        "no launchctl/systemctl calls after the abort"
    );
}
