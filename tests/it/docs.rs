//! The documentation stays true to the binary: `scripts/check-docs.sh` runs every ```sh run
//! block against a temp home (offline) and checks that every documented `pooltime <subcommand>`
//! exists. Hermetic: the script uses its own temp HOME / POOLTIME_HOME and a clean environment.

use std::process::Command;

use crate::support::procs;

#[test]
fn docs_commands_run_and_name_real_subcommands() {
    let root = env!("CARGO_MANIFEST_DIR");
    let out = procs::output(
        Command::new("sh")
            .arg(format!("{root}/scripts/check-docs.sh"))
            .env("POOLTIME_BIN", env!("CARGO_BIN_EXE_pooltime")),
    );
    assert!(
        out.status.success(),
        "check-docs failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
