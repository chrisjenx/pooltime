//! Settings `env` override with per-profile snapshots (INV §6). Ported from
//! bro's `settings.test.js`; everything runs under tempdirs, never the real home.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use pooltime::claude_code::{
    apply, clear, is_connected, scrub_legacy_pins, scrub_managed_context, snapshot_path, EnvBlock,
    Pin, Pins, Profile, MANAGED_KEYS,
};
use pooltime::config::Paths;
use pooltime::fsutil::WriteOutcome;
use serde_json::{json, Map, Value};

const URL: &str = "http://127.0.0.1:3460";
const TOKEN: &str = "pooltime-local";

struct Fix {
    _dir: tempfile::TempDir,
    home: PathBuf,
    snaps: Paths,
}

impl Fix {
    fn new() -> Fix {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let snaps = Paths::under(&dir.path().join("pooltime"));
        Fix {
            home,
            snaps,
            _dir: dir,
        }
    }
    fn profile(&self, dir_name: &str) -> Profile {
        let dir = self.home.join(dir_name);
        fs::create_dir_all(&dir).unwrap();
        Profile::from_dir(&dir, &self.home)
    }
    /// A profile whose settings.json holds `settings`.
    fn with_settings(&self, dir_name: &str, settings: Value) -> Profile {
        let p = self.profile(dir_name);
        fs::write(&p.settings_json, settings.to_string()).unwrap();
        p
    }
    fn snap_file(&self, p: &Profile) -> PathBuf {
        snapshot_path(&self.snaps, p)
    }
    fn write_snap(&self, p: &Profile, snap: Value) {
        let f = self.snap_file(p);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(f, snap.to_string()).unwrap();
    }
    fn read_snap(&self, p: &Profile) -> Value {
        read(&self.snap_file(p))
    }
    fn snap_exists(&self, p: &Profile) -> bool {
        self.snap_file(p).exists()
    }
}

fn read(f: &std::path::Path) -> Value {
    serde_json::from_slice(&fs::read(f).unwrap()).unwrap()
}

fn settings(p: &Profile) -> Value {
    read(&p.settings_json)
}

fn env(p: &Profile) -> Value {
    settings(p)["env"].clone()
}

fn block() -> EnvBlock {
    EnvBlock {
        base_url: URL.into(),
        auth_token: TOKEN.into(),
        pins: Pins::default(),
        max_context: None,
    }
}

fn with_ctx(n: u32) -> EnvBlock {
    EnvBlock {
        max_context: Some(n),
        ..block()
    }
}

fn pin(id: &str, name: &str, desc: &str) -> Pin {
    Pin {
        id: id.into(),
        name: name.into(),
        description: desc.into(),
    }
}

fn haiku_pin() -> Pin {
    pin(
        "claude-haiku-5-5",
        "Haiku",
        "Haiku 5.5 · Fastest for quick answers",
    )
}

fn sonnet_pin() -> Pin {
    pin(
        "claude-sonnet-5[1m]",
        "Sonnet",
        "Sonnet 5 with 1M context · Efficient for routine tasks",
    )
}

fn map(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

fn mode_of(p: &std::path::Path) -> u32 {
    fs::metadata(p).unwrap().permissions().mode() & 0o777
}

// ---- snapshot file location (bro defaultPaths* tests) ----

/// bro `defaultPaths follows CLAUDE_CONFIG_DIR` + `legacy state filename`: the
/// settings target is the profile's own settings.json, the snapshot is
/// `run/profiles/<id>.json` (the default profile is `default.json`).
#[test]
fn snapshot_file_is_run_profiles_id_json() {
    let f = Fix::new();
    let personal = f.profile(".claude-personal");
    let default = f.profile(".claude");
    assert_eq!(
        snapshot_path(&f.snaps, &personal),
        f.snaps.run.join("profiles").join("claude-personal.json")
    );
    assert_eq!(
        snapshot_path(&f.snaps, &default),
        f.snaps.run.join("profiles").join("default.json")
    );
}

/// bro `defaultPaths gives each profile its own state file` + `not dot-mangled`.
#[test]
fn snapshot_files_are_distinct_per_profile_and_not_dot_mangled() {
    let f = Fix::new();
    let a = snapshot_path(&f.snaps, &f.profile(".claude-personal"));
    let b = snapshot_path(&f.snaps, &f.profile(".claude-work"));
    let d = snapshot_path(&f.snaps, &f.profile(".claude"));
    assert!(a != b && a != d && b != d);
    // Stable across calls.
    assert_eq!(a, snapshot_path(&f.snaps, &f.profile(".claude-personal")));
    for p in [&a, &b, &d] {
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(!name.contains(".."), "double dot in {name}");
        assert!(!name.starts_with('.'), "dotfile {name}");
    }
}

// ---- apply / clear ----

/// bro `clearing one profile leaves another profile's override intact`.
#[test]
fn profiles_do_not_cross_contaminate() {
    let f = Fix::new();
    let a = f.with_settings(".claude-personal", json!({"model": "opus"}));
    let b = f.with_settings(".claude-work", json!({"model": "opus"}));
    apply(&a, &block(), &f.snaps).unwrap();
    apply(&b, &block(), &f.snaps).unwrap();
    assert_eq!(clear(&a, &f.snaps).unwrap(), WriteOutcome::Written);
    assert_eq!(settings(&a), json!({"model": "opus"}));
    assert!(!is_connected(&a, &f.snaps));
    assert!(!f.snap_exists(&a));
    assert!(is_connected(&b, &f.snaps));
    assert!(f.snap_exists(&b));
    assert_eq!(env(&b)["ANTHROPIC_BASE_URL"], URL);
}

#[test]
fn apply_adds_env_keys_and_preserves_other_settings() {
    let f = Fix::new();
    let p = f.with_settings(
        ".claude",
        json!({"model": "opus", "permissions": {"defaultMode": "auto"}}),
    );
    assert_eq!(
        apply(&p, &block(), &f.snaps).unwrap(),
        WriteOutcome::Written
    );
    let s = settings(&p);
    assert_eq!(s["env"]["ANTHROPIC_BASE_URL"], URL);
    assert_eq!(s["env"]["ANTHROPIC_AUTH_TOKEN"], TOKEN);
    assert_eq!(s["model"], "opus");
    assert_eq!(s["permissions"], json!({"defaultMode": "auto"}));
    assert!(is_connected(&p, &f.snaps));
}

/// bro `poolEnvBlock writes only the max-context fallback, never global auto-compact`.
#[test]
fn max_context_writes_value_and_marker_never_auto_compact() {
    let f = Fix::new();
    let p = f.profile(".claude");
    apply(&p, &with_ctx(272000), &f.snaps).unwrap();
    let e = env(&p);
    assert_eq!(e["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "272000");
    assert_eq!(e["POOLTIME_MANAGED_MAX_CONTEXT_TOKENS"], "272000");
    assert!(e.get("CLAUDE_CODE_AUTO_COMPACT_WINDOW").is_none());
    assert!(MANAGED_KEYS.contains(&"POOLTIME_MANAGED_MAX_CONTEXT_TOKENS"));
    assert!(MANAGED_KEYS.contains(&"CLAUDE_CODE_AUTO_COMPACT_WINDOW"));
}

/// bro `apply sets only the base URL and token unless ... context or pins are given`.
#[test]
fn apply_sets_only_base_url_and_token_without_pins_or_context() {
    let f = Fix::new();
    let p = f.profile(".claude");
    apply(&p, &block(), &f.snaps).unwrap();
    assert_eq!(
        env(&p),
        json!({"ANTHROPIC_BASE_URL": URL, "ANTHROPIC_AUTH_TOKEN": TOKEN})
    );
}

/// bro `apply and clear restore user context settings exactly`.
#[test]
fn apply_and_clear_restore_user_context_settings_exactly() {
    let f = Fix::new();
    let user = json!({
        "CLAUDE_CODE_MAX_CONTEXT_TOKENS": "333000",
        "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "222000",
    });
    let p = f.with_settings(".claude", json!({"env": user}));
    apply(&p, &with_ctx(272000), &f.snaps).unwrap();
    let active = env(&p);
    assert_eq!(active["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "272000");
    assert_eq!(active["CLAUDE_CODE_AUTO_COMPACT_WINDOW"], "222000");
    clear(&p, &f.snaps).unwrap();
    assert_eq!(env(&p), user);
}

/// bro `an apply without catalog context removes bro fallback but preserves the user snapshot`.
#[test]
fn apply_without_context_removes_fallback_but_preserves_user_snapshot() {
    let f = Fix::new();
    let p = f.with_settings(
        ".claude",
        json!({"env": {"CLAUDE_CODE_MAX_CONTEXT_TOKENS": "333000"}}),
    );
    apply(&p, &with_ctx(272000), &f.snaps).unwrap();
    apply(&p, &block(), &f.snaps).unwrap();
    assert_eq!(env(&p)["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "333000");
    assert!(env(&p).get("POOLTIME_MANAGED_MAX_CONTEXT_TOKENS").is_none());
    clear(&p, &f.snaps).unwrap();
    assert_eq!(env(&p)["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "333000");
}

/// bro `apply retires an old pool auto-compact value using its existing snapshot`.
#[test]
fn apply_retires_old_auto_compact_value_using_existing_snapshot() {
    let f = Fix::new();
    let p = f.with_settings(
        ".claude",
        json!({"env": {
            "ANTHROPIC_BASE_URL": URL,
            "ANTHROPIC_AUTH_TOKEN": TOKEN,
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "272000",
        }}),
    );
    f.write_snap(
        &p,
        json!({"managed": true, "prior": {
            "ANTHROPIC_BASE_URL": null,
            "ANTHROPIC_AUTH_TOKEN": null,
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW": null,
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS": null,
        }}),
    );
    apply(&p, &with_ctx(272000), &f.snaps).unwrap();
    assert!(env(&p).get("CLAUDE_CODE_AUTO_COMPACT_WINDOW").is_none());
}

/// bro `an older snapshot backfills the new max-context key before overwrite`,
/// and the brief's `snapshot_once_then_backfill`.
#[test]
fn snapshot_once_then_backfill() {
    let f = Fix::new();
    let p = f.with_settings(
        ".claude",
        json!({"env": {
            "ANTHROPIC_BASE_URL": URL,
            "ANTHROPIC_AUTH_TOKEN": TOKEN,
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS": "333000",
        }}),
    );
    f.write_snap(
        &p,
        json!({"managed": true, "prior": {"ANTHROPIC_BASE_URL": null, "ANTHROPIC_AUTH_TOKEN": null}}),
    );
    apply(&p, &with_ctx(272000), &f.snaps).unwrap();
    let snap = f.read_snap(&p);
    assert_eq!(snap["prior"]["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "333000");
    // Already-snapshotted keys keep their original (null) prior.
    assert_eq!(snap["prior"]["ANTHROPIC_BASE_URL"], Value::Null);
    // A second apply never re-snapshots our own values over the originals.
    apply(&p, &with_ctx(111000), &f.snaps).unwrap();
    assert_eq!(f.read_snap(&p), snap);
    clear(&p, &f.snaps).unwrap();
    assert_eq!(env(&p)["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "333000");
}

/// bro `clear preserves user max context absent from a legacy managed snapshot`.
#[test]
fn clear_preserves_user_max_context_absent_from_legacy_snapshot() {
    let f = Fix::new();
    let p = f.with_settings(
        ".claude",
        json!({"env": {
            "ANTHROPIC_BASE_URL": URL,
            "ANTHROPIC_AUTH_TOKEN": TOKEN,
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS": "333000",
        }}),
    );
    f.write_snap(
        &p,
        json!({"managed": true, "prior": {"ANTHROPIC_BASE_URL": null, "ANTHROPIC_AUTH_TOKEN": null}}),
    );
    clear(&p, &f.snaps).unwrap();
    assert_eq!(env(&p), json!({"CLAUDE_CODE_MAX_CONTEXT_TOKENS": "333000"}));
}

/// A managed value with no prior entry that we can prove is ours (marker) goes.
#[test]
fn clear_removes_marker_proven_context_with_no_prior_entry() {
    let f = Fix::new();
    for marker in [
        "POOLTIME_MANAGED_MAX_CONTEXT_TOKENS",
        "BRO_POOL_MANAGED_MAX_CONTEXT_TOKENS",
    ] {
        let p = f.with_settings(
            ".claude",
            json!({"env": {
                "ANTHROPIC_BASE_URL": URL,
                "CLAUDE_CODE_MAX_CONTEXT_TOKENS": "272000",
                marker: "272000",
                "KEEP": "1",
            }}),
        );
        f.write_snap(&p, json!({"managed": true, "prior": {}}));
        clear(&p, &f.snaps).unwrap();
        assert_eq!(env(&p), json!({"KEEP": "1"}), "{marker}");
    }
}

/// bro `apply scrubs model pins written by an earlier bro`.
#[test]
fn apply_scrubs_model_pins_written_by_an_earlier_bro() {
    let f = Fix::new();
    let p = f.with_settings(
        ".claude",
        json!({"env": {
            "ANTHROPIC_BASE_URL": URL,
            "ANTHROPIC_AUTH_TOKEN": TOKEN,
            "ANTHROPIC_DEFAULT_OPUS_MODEL": "claude-opus-5[1m]",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME": "Opus",
            "ANTHROPIC_DEFAULT_FABLE_MODEL": "claude-fable-5-1[1m]",
        }}),
    );
    f.write_snap(
        &p,
        json!({"managed": true, "prior": {
            "ANTHROPIC_BASE_URL": null,
            "ANTHROPIC_AUTH_TOKEN": null,
            "ANTHROPIC_DEFAULT_OPUS_MODEL": "claude-opus-4-8",
            "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME": null,
            "ANTHROPIC_DEFAULT_FABLE_MODEL": null,
        }}),
    );
    apply(&p, &block(), &f.snaps).unwrap();
    let e = env(&p);
    assert_eq!(e["ANTHROPIC_DEFAULT_OPUS_MODEL"], "claude-opus-4-8");
    assert!(e.get("ANTHROPIC_DEFAULT_OPUS_MODEL_NAME").is_none());
    assert!(e.get("ANTHROPIC_DEFAULT_FABLE_MODEL").is_none());
    assert_eq!(e["ANTHROPIC_BASE_URL"], URL);
}

/// bro `apply scrubs old bro pins even when no state file survived`.
#[test]
fn apply_scrubs_old_bro_pins_even_when_no_snapshot_survived() {
    let f = Fix::new();
    let p = f.with_settings(
        ".claude",
        json!({"env": {
            "ANTHROPIC_DEFAULT_SONNET_MODEL": "claude-sonnet-5[1m]",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME": "Sonnet",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL": "my-haiku",
        }}),
    );
    apply(&p, &block(), &f.snaps).unwrap();
    let e = env(&p);
    assert!(e.get("ANTHROPIC_DEFAULT_SONNET_MODEL").is_none());
    assert!(e.get("ANTHROPIC_DEFAULT_SONNET_MODEL_NAME").is_none());
    assert_eq!(e["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "my-haiku");
    clear(&p, &f.snaps).unwrap();
    assert_eq!(
        settings(&p),
        json!({"env": {"ANTHROPIC_DEFAULT_HAIKU_MODEL": "my-haiku"}})
    );
}

/// bro `apply and clear leave a user-set ANTHROPIC_DEFAULT_FABLE_MODEL alone`.
#[test]
fn apply_and_clear_leave_user_set_fable_pin_alone() {
    let f = Fix::new();
    let p = f.with_settings(
        ".claude",
        json!({"env": {"ANTHROPIC_DEFAULT_FABLE_MODEL": "claude-fable-5"}}),
    );
    apply(&p, &block(), &f.snaps).unwrap();
    assert_eq!(env(&p)["ANTHROPIC_DEFAULT_FABLE_MODEL"], "claude-fable-5");
    clear(&p, &f.snaps).unwrap();
    assert_eq!(
        settings(&p),
        json!({"env": {"ANTHROPIC_DEFAULT_FABLE_MODEL": "claude-fable-5"}})
    );
}

/// bro `clear removes pins an earlier bro wrote and the user never set`.
#[test]
fn clear_removes_pins_an_earlier_bro_wrote_and_user_never_set() {
    let f = Fix::new();
    let p = f.with_settings(
        ".claude",
        json!({
            "model": "opus",
            "env": {"ANTHROPIC_BASE_URL": URL, "ANTHROPIC_DEFAULT_SONNET_MODEL": "claude-sonnet-5[1m]"},
        }),
    );
    f.write_snap(
        &p,
        json!({"managed": true, "prior": {"ANTHROPIC_BASE_URL": null, "ANTHROPIC_DEFAULT_SONNET_MODEL": null}}),
    );
    clear(&p, &f.snaps).unwrap();
    assert_eq!(settings(&p), json!({"model": "opus"}));
}

/// A recognisable bro pin with no prior entry at all (snapshot predates the key) goes too.
#[test]
fn clear_removes_recognisable_pin_without_prior_entry() {
    let f = Fix::new();
    let p = f.with_settings(
        ".claude",
        json!({"env": {
            "ANTHROPIC_BASE_URL": URL,
            "ANTHROPIC_DEFAULT_SONNET_MODEL": "claude-sonnet-6[1m]",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL": "claude-haiku-4-5",
        }}),
    );
    f.write_snap(
        &p,
        json!({"managed": true, "prior": {"ANTHROPIC_BASE_URL": null}}),
    );
    clear(&p, &f.snaps).unwrap();
    assert_eq!(
        env(&p),
        json!({"ANTHROPIC_DEFAULT_HAIKU_MODEL": "claude-haiku-4-5"})
    );
}

/// bro `clear restores a file that had no env block`.
#[test]
fn clear_restores_a_file_that_had_no_env_block() {
    let f = Fix::new();
    let original = json!({"model": "opus", "permissions": {"defaultMode": "auto"}});
    let p = f.with_settings(".claude", original.clone());
    apply(&p, &block(), &f.snaps).unwrap();
    assert_eq!(clear(&p, &f.snaps).unwrap(), WriteOutcome::Written);
    assert_eq!(settings(&p), original);
    assert!(!is_connected(&p, &f.snaps));
}

/// bro `clear restores a pre-existing user ANTHROPIC_BASE_URL exactly`.
#[test]
fn clear_restores_pre_existing_user_base_url_exactly() {
    let f = Fix::new();
    let p = f.with_settings(
        ".claude",
        json!({"env": {"ANTHROPIC_BASE_URL": "https://gw.example", "FOO": "bar"}}),
    );
    apply(&p, &block(), &f.snaps).unwrap();
    assert_eq!(env(&p)["ANTHROPIC_BASE_URL"], URL);
    clear(&p, &f.snaps).unwrap();
    let e = env(&p);
    assert_eq!(e["ANTHROPIC_BASE_URL"], "https://gw.example");
    assert_eq!(e["FOO"], "bar");
    assert!(e.get("ANTHROPIC_AUTH_TOKEN").is_none());
}

/// bro `clear is a no-op when nothing is managed`; also creates no files.
#[test]
fn clear_is_a_noop_when_nothing_is_managed() {
    let f = Fix::new();
    let p = f.with_settings(".claude", json!({"model": "opus"}));
    let before = fs::read(&p.settings_json).unwrap();
    assert_eq!(clear(&p, &f.snaps).unwrap(), WriteOutcome::Unchanged);
    assert_eq!(fs::read(&p.settings_json).unwrap(), before);
    assert!(!f.snaps.run.exists(), "clear must not create the run dir");

    // Absent settings.json: still nothing created.
    let q = f.profile(".claude-other");
    assert_eq!(clear(&q, &f.snaps).unwrap(), WriteOutcome::Unchanged);
    assert!(!q.settings_json.exists());
    assert!(!f.snaps.run.exists());

    // A snapshot with managed:false is not "managed" either.
    f.write_snap(
        &p,
        json!({"managed": false, "prior": {"ANTHROPIC_BASE_URL": "x"}}),
    );
    assert_eq!(clear(&p, &f.snaps).unwrap(), WriteOutcome::Unchanged);
    assert_eq!(fs::read(&p.settings_json).unwrap(), before);
    assert!(f.snap_exists(&p));
}

/// bro `apply creates settings.json when absent, clear removes empty env`, and the
/// brief's `clear_restores_and_drops_empty_env`.
#[test]
fn clear_restores_and_drops_empty_env() {
    let f = Fix::new();
    let p = f.profile(".claude");
    assert!(!p.settings_json.exists());
    apply(&p, &with_ctx(272000), &f.snaps).unwrap();
    assert_eq!(env(&p)["ANTHROPIC_BASE_URL"], URL);
    assert_eq!(env(&p)["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "272000");
    clear(&p, &f.snaps).unwrap();
    let s = settings(&p);
    assert!(s.get("env").is_none(), "empty env must be dropped: {s}");
    assert!(!f.snap_exists(&p));
}

/// bro `apply twice keeps the original snapshot`.
#[test]
fn apply_twice_keeps_the_original_snapshot() {
    let f = Fix::new();
    let p = f.with_settings(
        ".claude",
        json!({"env": {"ANTHROPIC_BASE_URL": "https://gw.example"}}),
    );
    apply(&p, &block(), &f.snaps).unwrap();
    let first = f.read_snap(&p);
    let other = EnvBlock {
        base_url: "http://127.0.0.1:9999".into(),
        auth_token: "x".into(),
        ..block()
    };
    apply(&p, &other, &f.snaps).unwrap();
    assert_eq!(f.read_snap(&p), first);
    assert_eq!(env(&p)["ANTHROPIC_BASE_URL"], "http://127.0.0.1:9999");
    clear(&p, &f.snaps).unwrap();
    assert_eq!(env(&p)["ANTHROPIC_BASE_URL"], "https://gw.example");
}

// ---- pins ----

/// bro `apply writes the derived Haiku pin over a user pin; clear restores the user`.
#[test]
fn apply_writes_haiku_pin_over_user_pin_and_clear_restores_user() {
    let f = Fix::new();
    let p = f.with_settings(
        ".claude",
        json!({"env": {"ANTHROPIC_DEFAULT_HAIKU_MODEL": "claude-haiku-4-5"}}),
    );
    let b = EnvBlock {
        pins: Pins {
            haiku: Some(haiku_pin()),
            sonnet: None,
        },
        ..block()
    };
    apply(&p, &b, &f.snaps).unwrap();
    let e = env(&p);
    assert_eq!(e["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "claude-haiku-5-5");
    assert_eq!(e["ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME"], "Haiku");
    assert_eq!(
        e["ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION"],
        "Haiku 5.5 · Fastest for quick answers"
    );
    assert!(e.get("ANTHROPIC_DEFAULT_SONNET_MODEL").is_none());
    clear(&p, &f.snaps).unwrap();
    assert_eq!(
        settings(&p),
        json!({"env": {"ANTHROPIC_DEFAULT_HAIKU_MODEL": "claude-haiku-4-5"}})
    );
}

/// bro `apply does not snapshot bro's Haiku pin when no state file survived`, and the
/// brief's `own_pins_never_snapshotted`.
#[test]
fn own_pins_never_snapshotted() {
    let f = Fix::new();
    let p = f.with_settings(
        ".claude",
        json!({"env": {
            "ANTHROPIC_DEFAULT_HAIKU_MODEL": "claude-haiku-5-5",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME": "Haiku",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION": "Haiku 5.5 · Fastest for quick answers",
            "ANTHROPIC_DEFAULT_SONNET_MODEL": "claude-sonnet-5[1m]",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION": "Sonnet 5 with 1M context · Efficient for routine tasks",
        }}),
    );
    apply(&p, &block(), &f.snaps).unwrap();
    let e = env(&p);
    for k in [
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION",
    ] {
        assert!(e.get(k).is_none(), "{k}");
        assert_eq!(f.read_snap(&p)["prior"][k], Value::Null, "{k}");
    }
    clear(&p, &f.snaps).unwrap();
    assert_eq!(settings(&p), json!({}));
}

/// Same for a pooltime/bro-managed context value (the marker proves it) when the
/// snapshot was lost; a bare matching number without a marker stays the user's.
#[test]
fn own_context_value_never_snapshotted() {
    let f = Fix::new();
    for marker in [
        "POOLTIME_MANAGED_MAX_CONTEXT_TOKENS",
        "BRO_POOL_MANAGED_MAX_CONTEXT_TOKENS",
    ] {
        let p = f.with_settings(
            ".claude",
            json!({"env": {"CLAUDE_CODE_MAX_CONTEXT_TOKENS": "272000", marker: "272000"}}),
        );
        apply(&p, &block(), &f.snaps).unwrap();
        let snap = f.read_snap(&p);
        assert_eq!(snap["prior"]["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], Value::Null);
        assert_eq!(snap["prior"][marker], Value::Null);
        let e = env(&p);
        assert!(e.get("CLAUDE_CODE_MAX_CONTEXT_TOKENS").is_none());
        assert!(e.get(marker).is_none());
        clear(&p, &f.snaps).unwrap();
        assert_eq!(settings(&p), json!({}));
        assert!(!f.snap_exists(&p));
    }
    let p = f.with_settings(
        ".claude",
        json!({"env": {"CLAUDE_CODE_MAX_CONTEXT_TOKENS": "272000"}}),
    );
    apply(&p, &block(), &f.snaps).unwrap();
    assert_eq!(
        f.read_snap(&p)["prior"]["CLAUDE_CODE_MAX_CONTEXT_TOKENS"],
        "272000"
    );
}

/// bro `apply writes the derived Sonnet pin; an apply without one removes it; clear restores the user`.
#[test]
fn sonnet_pin_applied_removed_by_pinless_apply_and_restored_by_clear() {
    let f = Fix::new();
    let p = f.with_settings(
        ".claude",
        json!({"env": {"ANTHROPIC_DEFAULT_SONNET_MODEL": "claude-sonnet-4-6"}}),
    );
    let b = EnvBlock {
        pins: Pins {
            sonnet: Some(sonnet_pin()),
            haiku: None,
        },
        ..block()
    };
    apply(&p, &b, &f.snaps).unwrap();
    assert_eq!(
        env(&p)["ANTHROPIC_DEFAULT_SONNET_MODEL"],
        "claude-sonnet-5[1m]"
    );
    assert_eq!(env(&p)["ANTHROPIC_DEFAULT_SONNET_MODEL_NAME"], "Sonnet");
    apply(&p, &block(), &f.snaps).unwrap(); // catalog unreachable this time
    assert_eq!(
        env(&p)["ANTHROPIC_DEFAULT_SONNET_MODEL"],
        "claude-sonnet-4-6"
    );
    assert!(env(&p).get("ANTHROPIC_DEFAULT_SONNET_MODEL_NAME").is_none());
    clear(&p, &f.snaps).unwrap();
    assert_eq!(
        settings(&p),
        json!({"env": {"ANTHROPIC_DEFAULT_SONNET_MODEL": "claude-sonnet-4-6"}})
    );
}

/// bro `a future derived Sonnet pin is recognised as bro's when no snapshot survived`.
#[test]
fn future_sonnet_pin_recognised_as_ours_when_no_snapshot_survived() {
    let f = Fix::new();
    let p = f.with_settings(
        ".claude",
        json!({"env": {
            "ANTHROPIC_DEFAULT_SONNET_MODEL": "claude-sonnet-6[1m]",
            "ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION": "Sonnet 6 with 1M context · Efficient for routine tasks",
        }}),
    );
    apply(&p, &block(), &f.snaps).unwrap();
    assert!(env(&p).get("ANTHROPIC_DEFAULT_SONNET_MODEL").is_none());
}

// ---- skip-unchanged / snapshot handling ----

/// bro `apply leaves settings.json byte-for-byte alone when nothing changed`, and the
/// brief's `unchanged_env_skips_write`.
#[test]
fn unchanged_env_skips_write() {
    let f = Fix::new();
    let p = f.profile(".claude");
    assert_eq!(
        apply(&p, &block(), &f.snaps).unwrap(),
        WriteOutcome::Written
    );
    // Rewrite compactly: any rewrite by us would restore pretty printing.
    let compact = serde_json::to_string(&settings(&p)).unwrap();
    fs::write(&p.settings_json, &compact).unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    fs::File::options()
        .write(true)
        .open(&p.settings_json)
        .unwrap()
        .set_modified(old)
        .unwrap();
    let mtime = fs::metadata(&p.settings_json).unwrap().modified().unwrap();
    assert_eq!(
        apply(&p, &block(), &f.snaps).unwrap(),
        WriteOutcome::Unchanged
    );
    assert_eq!(fs::read_to_string(&p.settings_json).unwrap(), compact);
    assert_eq!(
        fs::metadata(&p.settings_json).unwrap().modified().unwrap(),
        mtime
    );
}

#[test]
fn snapshot_file_is_0600_in_a_0700_profiles_dir() {
    let f = Fix::new();
    let p = f.profile(".claude-personal");
    apply(&p, &block(), &f.snaps).unwrap();
    let file = f.snap_file(&p);
    assert_eq!(mode_of(&file), 0o600);
    assert_eq!(mode_of(file.parent().unwrap()), 0o700);
    let snap = f.read_snap(&p);
    assert_eq!(snap["managed"], true);
    assert_eq!(snap["statusline_prior"], Value::Null);
    // No temp files left behind.
    assert_eq!(fs::read_dir(file.parent().unwrap()).unwrap().count(), 1);
}

/// An unreadable snapshot is left alone (bro: "matches the original behaviour"):
/// apply still points the profile at the pool but never writes over the file and
/// has no prior to restore; clear then sees nothing managed.
#[test]
fn unreadable_snapshot_left_alone() {
    let f = Fix::new();
    let p = f.with_settings(
        ".claude",
        json!({"env": {"ANTHROPIC_BASE_URL": "https://gw.example"}}),
    );
    let file = f.snap_file(&p);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, b"{ not json").unwrap();
    apply(&p, &block(), &f.snaps).unwrap();
    assert_eq!(fs::read(&file).unwrap(), b"{ not json");
    assert_eq!(env(&p)["ANTHROPIC_BASE_URL"], URL);
    assert!(!is_connected(&p, &f.snaps));
    let before = fs::read(&p.settings_json).unwrap();
    assert_eq!(clear(&p, &f.snaps).unwrap(), WriteOutcome::Unchanged);
    assert_eq!(fs::read(&p.settings_json).unwrap(), before);
    assert_eq!(fs::read(&file).unwrap(), b"{ not json");
    // A snapshot that is valid JSON but not an object is equally unreadable.
    fs::write(&file, b"[1,2]").unwrap();
    apply(&p, &block(), &f.snaps).unwrap();
    assert_eq!(fs::read(&file).unwrap(), b"[1,2]");
}

#[test]
fn statusline_prior_and_unknown_snapshot_fields_survive_rewrites() {
    let f = Fix::new();
    let p = f.with_settings(
        ".claude",
        json!({"env": {"CLAUDE_CODE_MAX_CONTEXT_TOKENS": "333000"}}),
    );
    f.write_snap(
        &p,
        json!({
            "managed": true,
            "prior": {"ANTHROPIC_BASE_URL": null},
            "statusline_prior": {"type": "command", "command": "x"},
            "future_field": 7,
        }),
    );
    apply(&p, &block(), &f.snaps).unwrap(); // backfills, rewrites the snapshot
    let snap = f.read_snap(&p);
    assert_eq!(snap["prior"]["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "333000");
    assert_eq!(
        snap["statusline_prior"],
        json!({"type": "command", "command": "x"})
    );
    assert_eq!(snap["future_field"], 7);
}

#[test]
fn corrupt_settings_is_an_error_and_never_overwritten() {
    let f = Fix::new();
    let p = f.profile(".claude");
    fs::write(&p.settings_json, b"{ nope").unwrap();
    assert!(apply(&p, &block(), &f.snaps).is_err());
    assert_eq!(fs::read(&p.settings_json).unwrap(), b"{ nope");
    assert!(
        !f.snap_exists(&p),
        "no snapshot for a settings file we cannot patch"
    );

    // Non-object env: refuse rather than guess; settings untouched.
    let q = f.with_settings(".claude-q", json!({"env": "oops"}));
    let before = fs::read(&q.settings_json).unwrap();
    assert!(apply(&q, &block(), &f.snaps).is_err());
    assert_eq!(fs::read(&q.settings_json).unwrap(), before);

    // Clear with a corrupt settings.json keeps the snapshot so it can be retried.
    let r = f.profile(".claude-r");
    apply(&r, &block(), &f.snaps).unwrap();
    fs::write(&r.settings_json, b"{ nope").unwrap();
    assert!(clear(&r, &f.snaps).is_err());
    assert!(f.snap_exists(&r));
    assert_eq!(fs::read(&r.settings_json).unwrap(), b"{ nope");
}

#[test]
fn settings_key_order_and_unknown_keys_are_preserved() {
    let f = Fix::new();
    let p = f.with_settings(
        ".claude",
        json!({"zeta": 1, "env": {"B": "2", "A": "1"}, "alpha": {"n": 1.5}}),
    );
    apply(&p, &block(), &f.snaps).unwrap();
    let keys: Vec<_> = settings(&p).as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["zeta", "env", "alpha"]);
    assert!(fs::read_to_string(&p.settings_json)
        .unwrap()
        .ends_with('\n'));
    clear(&p, &f.snaps).unwrap();
    assert_eq!(env(&p), json!({"B": "2", "A": "1"}));
}

#[test]
fn is_connected_needs_managed_snapshot_and_base_url() {
    let f = Fix::new();
    let p = f.with_settings(".claude", json!({}));
    assert!(!is_connected(&p, &f.snaps));
    apply(&p, &block(), &f.snaps).unwrap();
    assert!(is_connected(&p, &f.snaps));
    // Base URL removed behind our back -> not connected.
    fs::write(&p.settings_json, "{}").unwrap();
    assert!(!is_connected(&p, &f.snaps));
    // Unreadable settings -> not connected, no panic.
    fs::write(&p.settings_json, "{ bad").unwrap();
    assert!(!is_connected(&p, &f.snaps));
}

// ---- scrub (bro scrubLegacyPins / scrubManagedContext) ----

/// bro `scrubManagedContext removes only an inherited value proven to be bro-managed`.
#[test]
fn scrub_managed_context_removes_only_proven_value() {
    for marker in [
        "POOLTIME_MANAGED_MAX_CONTEXT_TOKENS",
        "BRO_POOL_MANAGED_MAX_CONTEXT_TOKENS",
    ] {
        let mut managed =
            map(json!({"CLAUDE_CODE_MAX_CONTEXT_TOKENS": "272000", marker: "272000"}));
        scrub_managed_context(&mut managed);
        assert!(managed.is_empty());

        let mut overridden =
            map(json!({"CLAUDE_CODE_MAX_CONTEXT_TOKENS": "333000", marker: "272000"}));
        scrub_managed_context(&mut overridden);
        assert_eq!(
            Value::Object(overridden),
            json!({"CLAUDE_CODE_MAX_CONTEXT_TOKENS": "333000"})
        );
    }
    let mut user = map(json!({"CLAUDE_CODE_MAX_CONTEXT_TOKENS": "333000"}));
    scrub_managed_context(&mut user);
    assert_eq!(
        Value::Object(user),
        json!({"CLAUDE_CODE_MAX_CONTEXT_TOKENS": "333000"})
    );
}

/// bro `scrubLegacyPins drops bro's Haiku pin as a unit and keeps a user Haiku pin`.
#[test]
fn scrub_legacy_pins_drops_haiku_pin_as_a_unit_and_keeps_user_pin() {
    let mut bros = map(json!({
        "ANTHROPIC_DEFAULT_HAIKU_MODEL": "claude-haiku-5-5",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME": "Haiku",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION": "Haiku 5.5 · Fastest for quick answers",
    }));
    scrub_legacy_pins(&mut bros);
    assert!(bros.is_empty());

    let user = json!({
        "ANTHROPIC_DEFAULT_HAIKU_MODEL": "claude-haiku-4-5",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME": "Haiku",
    });
    let mut m = map(user.clone());
    scrub_legacy_pins(&mut m);
    assert_eq!(Value::Object(m), user);
}

#[test]
fn scrub_legacy_pins_recognises_historical_literals_and_shapes() {
    let mut m = map(json!({
        "ANTHROPIC_DEFAULT_SONNET_MODEL": "claude-sonnet-5[1m]",
        "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME": "Sonnet",
        "ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION": "Sonnet 7 with 1M context · Efficient for routine tasks",
        "ANTHROPIC_DEFAULT_OPUS_MODEL": "claude-opus-5[1m]",
        "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME": "Opus",
        "ANTHROPIC_DEFAULT_OPUS_MODEL_DESCRIPTION": "Opus 5 with 1M context · Best for everyday, complex tasks",
        "ANTHROPIC_DEFAULT_FABLE_MODEL": "claude-fable-5-2[1m]",
        "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME": "Fable",
        "ANTHROPIC_DEFAULT_FABLE_MODEL_DESCRIPTION": "Fable 5.2 · Most capable for your hardest and longest-running tasks",
        "OTHER": "kept",
    }));
    scrub_legacy_pins(&mut m);
    assert_eq!(Value::Object(m), json!({"OTHER": "kept"}));

    // User values of the same keys stay.
    let user = json!({
        "ANTHROPIC_DEFAULT_SONNET_MODEL": "claude-sonnet-4-6",
        "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME": "My Sonnet",
        "ANTHROPIC_DEFAULT_OPUS_MODEL": "claude-opus-4-8",
        "ANTHROPIC_DEFAULT_FABLE_MODEL": "claude-fable-5",
        "CLAUDE_CODE_AUTO_COMPACT_WINDOW": "222000",
    });
    let mut m = map(user.clone());
    scrub_legacy_pins(&mut m);
    assert_eq!(Value::Object(m), user);

    // Non-string values never match and never panic.
    let weird = json!({"ANTHROPIC_DEFAULT_SONNET_MODEL": 5, "ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION": null});
    let mut m = map(weird.clone());
    scrub_legacy_pins(&mut m);
    assert_eq!(Value::Object(m), weird);
}

/// Crash safety: the snapshot is on disk before the settings.json write lands. A
/// read-only profile dir makes the settings write fail after the snapshot step.
#[test]
fn snapshot_is_persisted_before_the_settings_write() {
    let f = Fix::new();
    let p = f.with_settings(
        ".claude",
        json!({"env": {"ANTHROPIC_BASE_URL": "https://gw.example"}}),
    );
    fs::set_permissions(&p.settings_dir, fs::Permissions::from_mode(0o500)).unwrap();
    let res = apply(&p, &block(), &f.snaps);
    fs::set_permissions(&p.settings_dir, fs::Permissions::from_mode(0o700)).unwrap();
    if res.is_ok() {
        return; // permissions not enforced here (e.g. running as root)
    }
    assert_eq!(
        f.read_snap(&p)["prior"]["ANTHROPIC_BASE_URL"],
        "https://gw.example"
    );
    assert_eq!(env(&p)["ANTHROPIC_BASE_URL"], "https://gw.example");
    // Retrying after the failure completes and restores cleanly.
    apply(&p, &block(), &f.snaps).unwrap();
    clear(&p, &f.snaps).unwrap();
    assert_eq!(env(&p)["ANTHROPIC_BASE_URL"], "https://gw.example");
}
