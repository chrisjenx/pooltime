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

use pooltime::claude_code::{apply, is_connected, profile_of, EnvBlock, Pins};
use pooltime::config::{Config, Paths, ProfileRef};
use pooltime::service::{
    launchd, render_launchd_plist, render_systemd_unit, stable_bin, systemd, CmdOutput, Env, Runner,
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

#[derive(Default)]
struct Rec {
    calls: Mutex<Vec<String>>,
    /// Command prefixes that fail.
    fail: Vec<&'static str>,
}

impl Rec {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl Runner for Rec {
    fn run(&self, program: &str, args: &[&str]) -> io::Result<CmdOutput> {
        let line = format!("{program} {}", args.join(" "));
        let ok = !self.fail.iter().any(|f| line.starts_with(f));
        self.calls.lock().unwrap().push(line);
        Ok(CmdOutput {
            success: ok,
            code: Some(if ok { 0 } else { 1 }),
            stdout: "\tstate = running\n\tpid = 4242\n".into(),
            stderr: if ok { "" } else { "boom" }.into(),
        })
    }
}

fn env_in(root: &Path) -> Env {
    Env {
        paths: Paths::under(&root.join("pt")),
        bin: PathBuf::from("/usr/local/bin/pooltime"),
        home: root.join("home"),
        xdg_config_home: None,
        uid: 501,
    }
}

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
    assert_eq!(rec.calls().last().unwrap(), &boot, "{:?}", rec.calls());

    // Re-install: unload the old definition first, then load the new one.
    let rec2 = Rec::default();
    launchd::install(&env, &rec2).unwrap();
    assert_eq!(
        rec2.calls(),
        [
            "launchctl bootout gui/501/dev.pooltime.daemon".to_string(),
            boot
        ]
    );
}

#[test]
fn launchd_bootstrap_failure_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Rec {
        fail: vec!["launchctl bootstrap"],
        ..Rec::default()
    };
    let err = launchd::install(&env_in(dir.path()), &rec).unwrap_err();
    assert!(format!("{err:#}").contains("boom"), "{err:#}");
}

#[test]
fn launchd_uninstall_status_restart() {
    let dir = tempfile::tempdir().unwrap();
    let env = env_in(dir.path());
    launchd::install(&env, &Rec::default()).unwrap();

    let rec = Rec::default();
    let st = launchd::status(&env, &rec).unwrap();
    assert!(st.installed && st.running, "{st:?}");
    assert!(st.detail.contains("pid = 4242"), "{st:?}");
    assert_eq!(rec.calls(), ["launchctl print gui/501/dev.pooltime.daemon"]);

    let rec = Rec::default();
    launchd::restart(&env, &rec).unwrap();
    assert_eq!(
        rec.calls(),
        ["launchctl kickstart -k gui/501/dev.pooltime.daemon"]
    );

    // Uninstall tolerates "not loaded" (bootout fails) and still removes the file; twice is fine.
    let rec = Rec {
        fail: vec!["launchctl bootout"],
        ..Rec::default()
    };
    assert!(launchd::uninstall(&env, &rec).unwrap());
    assert!(!launchd::plist_path(&env).exists());
    assert!(!launchd::uninstall(&env, &rec).unwrap());
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

    let rec = Rec::default();
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

    let rec = Rec::default();
    let st = systemd::status(&env, &rec).unwrap();
    assert!(st.installed && st.running);
    assert_eq!(rec.calls(), ["systemctl --user status pooltime.service"]);

    let rec = Rec::default();
    systemd::restart(&env, &rec).unwrap();
    assert_eq!(rec.calls(), ["systemctl --user restart pooltime.service"]);

    let rec = Rec::default();
    assert!(systemd::uninstall(&env, &rec).unwrap());
    assert!(!unit.exists());
    assert_eq!(
        rec.calls(),
        [
            "systemctl --user disable --now pooltime.service",
            "systemctl --user daemon-reload"
        ]
    );
}

// ---- the binary, with a PATH shim -------------------------------------------------------------

/// `launchctl` and `systemctl` shims that append their arguments to `calls.log`.
fn shim_dir(root: &Path) -> (PathBuf, PathBuf) {
    let dir = root.join("shims");
    fs::create_dir_all(&dir).unwrap();
    let log = root.join("calls.log");
    for tool in ["launchctl", "systemctl"] {
        let script = dir.join(tool);
        fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"{tool} $*\" >> \"{}\"\nexit 0\n",
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

#[test]
fn uninstall_disconnects_profiles() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let paths = Paths::under(&root.path().join("pt"));
    paths.ensure().unwrap();
    let (shims, log) = shim_dir(root.path());

    // A connected profile: the user's own settings, the pool env block on top, recorded in config.
    let dir = home.join(".claude-svc");
    fs::create_dir_all(&dir).unwrap();
    let r = ProfileRef {
        id: "claude-svc".into(),
        settings_dir: dir.clone(),
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
    apply(&p, &block, &paths).unwrap();
    let mut cfg = Config::load_file(&paths).unwrap();
    cfg.profiles.push(r);
    cfg.save(&paths).unwrap();
    assert!(is_connected(&p, &paths));
    assert!(json_of(&p.settings_json)["env"]["ANTHROPIC_BASE_URL"].is_string());

    // install -> definition file + shim calls
    let out = pooltime(root.path(), &shims)
        .args(["service", "install"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (def, load, unload) = if cfg!(target_os = "macos") {
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
    };
    assert!(
        def.exists(),
        "service definition written under the temp home"
    );
    let calls = fs::read_to_string(&log).unwrap();
    assert!(calls.contains(load), "{calls}");

    // uninstall -> profile restored, service unloaded and removed
    let out = pooltime(root.path(), &shims)
        .args(["service", "uninstall"])
        .output()
        .unwrap();
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
    let out = pooltime(root.path(), &shims)
        .args(["service", "uninstall"])
        .output()
        .unwrap();
    assert!(out.status.success());
}
