//! Claude Code profiles and guarded JSON patching. Everything runs under a
//! tempdir passed as `home`; nothing here touches the real home directory.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use pooltime::claude_code::{discover, patch_json, Profile};
use pooltime::fsutil::WriteOutcome;

fn mkprofile(home: &Path, name: &str) {
    let d = home.join(name);
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("settings.json"), "{}\n").unwrap();
}

#[test]
fn default_profile_uses_home_claude_json() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".claude");
    let p = Profile::from_dir(&dir, home.path());
    assert_eq!(p.id, "default");
    assert_eq!(p.settings_dir, dir);
    assert_eq!(p.settings_json, dir.join("settings.json"));
    assert_eq!(p.claude_json, home.path().join(".claude.json"));
}

#[test]
fn other_profile_uses_inner_claude_json() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".claude-personal");
    let p = Profile::from_dir(&dir, home.path());
    assert_eq!(p.id, "claude-personal");
    assert_eq!(p.settings_json, dir.join("settings.json"));
    assert_eq!(p.claude_json, dir.join(".claude.json"));

    // A dir outside home is never the default profile.
    let elsewhere = tempfile::tempdir().unwrap();
    let p = Profile::from_dir(&elsewhere.path().join(".claude"), home.path());
    assert_ne!(p.id, "default");
    assert_eq!(p.claude_json, elsewhere.path().join(".claude/.claude.json"));
}

#[test]
fn discover_excludes_pool_dirs() {
    let home = tempfile::tempdir().unwrap();
    mkprofile(home.path(), ".claude");
    mkprofile(home.path(), ".claude-work");
    mkprofile(home.path(), ".claude-personal");
    mkprofile(home.path(), ".claude-max-pool"); // bro's pool dir: excluded
    mkprofile(home.path(), ".other"); // not a claude dir
    fs::create_dir_all(home.path().join(".claude-nosettings")).unwrap(); // no settings.json
    fs::write(home.path().join(".claude-file"), "x").unwrap(); // not a dir
    fs::write(home.path().join(".claude.json"), "{}").unwrap(); // not a dir

    let found = discover(home.path());
    let ids: Vec<&str> = found.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, vec!["default", "claude-personal", "claude-work"]);
    assert_eq!(found[0].claude_json, home.path().join(".claude.json"));
    assert_eq!(
        found[1].claude_json,
        home.path().join(".claude-personal/.claude.json")
    );
    // Stable across calls.
    assert_eq!(discover(home.path()), found);
    // Missing home: nothing, no panic.
    assert!(discover(&home.path().join("absent")).is_empty());
}

#[test]
fn patch_preserves_unknown_keys_and_order() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("settings.json");
    fs::write(
        &p,
        "{\n  \"zeta\": 1,\n  \"env\": {\n    \"B\": \"2\",\n    \"A\": \"1\"\n  },\n  \"alpha\": [1, 2.50, 12345678901234567890],\n  \"nested\": {\"k\": null}\n}",
    )
    .unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o640)).unwrap();

    let out = patch_json(&p, |m| {
        let env = m.get_mut("env").unwrap().as_object_mut().unwrap();
        env.insert("ANTHROPIC_BASE_URL".into(), "http://127.0.0.1:3460".into());
        true
    })
    .unwrap();
    assert_eq!(out, WriteOutcome::Written);
    assert_eq!(
        fs::read_to_string(&p).unwrap(),
        "{\n  \"zeta\": 1,\n  \"env\": {\n    \"B\": \"2\",\n    \"A\": \"1\",\n    \"ANTHROPIC_BASE_URL\": \"http://127.0.0.1:3460\"\n  },\n  \"alpha\": [\n    1,\n    2.50,\n    12345678901234567890\n  ],\n  \"nested\": {\n    \"k\": null\n  }\n}\n"
    );
    assert_eq!(
        fs::metadata(&p).unwrap().permissions().mode() & 0o777,
        0o640
    );

    // closure reports no change -> file (and mtime) untouched.
    let before = fs::metadata(&p).unwrap().modified().unwrap();
    assert_eq!(patch_json(&p, |_| false).unwrap(), WriteOutcome::Unchanged);
    // closure "changes" but result is byte-identical -> no write.
    assert_eq!(patch_json(&p, |_| true).unwrap(), WriteOutcome::Unchanged);
    assert_eq!(fs::metadata(&p).unwrap().modified().unwrap(), before);
}

#[test]
fn patch_creates_missing_file_and_rejects_non_object() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("new").join("x");
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    let p = p.parent().unwrap().join(".claude.json");
    // Missing + no change -> nothing created.
    assert_eq!(patch_json(&p, |_| false).unwrap(), WriteOutcome::Unchanged);
    assert!(!p.exists());
    let out = patch_json(&p, |m| {
        assert!(m.is_empty());
        m.insert("a".into(), 1.into());
        true
    })
    .unwrap();
    assert_eq!(out, WriteOutcome::Written);
    assert_eq!(fs::read_to_string(&p).unwrap(), "{\n  \"a\": 1\n}\n");
    assert_eq!(
        fs::metadata(&p).unwrap().permissions().mode() & 0o777,
        0o644
    );

    let arr = dir.path().join("arr.json");
    fs::write(&arr, "[1,2]").unwrap();
    assert!(patch_json(&arr, |_| true).is_err());
    assert_eq!(fs::read(&arr).unwrap(), b"[1,2]");
}

#[test]
fn corrupt_settings_is_error_and_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("settings.json");
    let junk = b"{\"env\": {\"A\": ";
    fs::write(&p, junk).unwrap();
    let mut called = false;
    let err = patch_json(&p, |_| {
        called = true;
        true
    })
    .unwrap_err();
    assert!(!called, "closure must not run on corrupt input");
    assert!(err.to_string().contains("settings.json"));
    assert_eq!(fs::read(&p).unwrap(), junk);

    // Empty file is also not valid JSON: error, untouched.
    fs::write(&p, b"").unwrap();
    assert!(patch_json(&p, |_| true).is_err());
    assert_eq!(fs::read(&p).unwrap(), b"");

    // No temp leftovers.
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn guarded_write_retries_when_file_changes_between_read_and_rename() {
    let home = tempfile::tempdir().unwrap();
    mkprofile(home.path(), ".claude-work");
    let profile = Profile::from_dir(&home.path().join(".claude-work"), home.path());
    let p = profile.settings_json.clone();
    fs::write(&p, "{\n  \"theme\": \"dark\"\n}\n").unwrap();

    let mut calls = 0;
    let out = patch_json(&p, |m| {
        calls += 1;
        if calls == 1 {
            // Simulate Claude Code rewriting the file between our read and rename.
            fs::write(&p, "{\n  \"theme\": \"dark\",\n  \"ccKey\": true\n}\n").unwrap();
        }
        m.insert("env".into(), serde_json::json!({"ANTHROPIC_BASE_URL": "x"}));
        true
    })
    .unwrap();
    assert_eq!(out, WriteOutcome::Written);
    assert_eq!(calls, 2);
    let v: serde_json::Value = serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
    assert_eq!(v["ccKey"], true, "Claude Code's concurrent write was lost");
    assert_eq!(v["theme"], "dark");
    assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], "x");
    assert_eq!(
        fs::read_dir(home.path().join(".claude-work"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn patch_gives_up_when_file_always_changes() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("settings.json");
    fs::write(&p, "{}\n").unwrap();
    let mut calls = 0usize;
    let out = patch_json(&p, |m| {
        calls += 1;
        // Growing content: stamp differs on every attempt.
        fs::write(&p, format!("{{\"cc\": \"{}\"}}\n", "x".repeat(calls))).unwrap();
        m.insert("mine".into(), true.into());
        true
    })
    .unwrap();
    assert_eq!(out, WriteOutcome::GaveUp);
    assert_eq!(calls, 6); // first attempt + 5 retries
    assert!(!fs::read_to_string(&p).unwrap().contains("mine"));
}
