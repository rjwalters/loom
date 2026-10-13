//! Integration: J1 execution-journal shadow writes (#11346) driven through
//! the public producer API with the real environment switch, then checked by
//! the `loom-daemon journal verify --concern execution` binary.
//!
//! One test function: it mutates this process's environment, so nothing
//! else in this binary may run beside it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

use chrono::Utc;
use loom_daemon::journal::execution::{self, DIR_ENV, ENABLE_ENV};
use loom_daemon::sweep_journal;

fn verify(root: &Path) -> (i32, serde_json::Value) {
    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args([
            "journal",
            "verify",
            "--concern",
            "execution",
            "--json",
            "--root",
        ])
        .arg(root)
        .env_remove("LOOM_JOURNAL_ROOT")
        .output()
        .unwrap();
    let report = serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null);
    (out.status.code().unwrap(), report)
}

#[test]
fn shadow_writes_follow_the_env_switch_and_verify_through_the_cli() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let e1 = ws.path().join("sweeps.json");
    let pid = std::process::id();
    std::env::set_var(DIR_ENV, root.path());

    // Default off: the legacy write happens, the journal does not.
    std::env::remove_var(ENABLE_ENV);
    sweep_journal::record_sweep_at(&e1, "/repo/a", 1, pid, Utc::now()).unwrap();
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    let (code, _) = verify(root.path());
    assert_eq!(code, 2, "no execution journal yet is not a green");

    // Enabled: the legacy write is shadowed and verifies clean.
    std::env::set_var(ENABLE_ENV, "shadow");
    sweep_journal::record_sweep_at(&e1, "/repo/a", 2, pid, Utc::now()).unwrap();
    assert!(root.path().join(execution::STREAM).is_dir());
    let (code, report) = verify(root.path());
    assert_eq!(code, 0, "{report:#}");
    assert_eq!(report["ok"], true);

    // A legacy change the journal never saw is an unexplained divergence;
    // verification itself changes nothing.
    std::env::remove_var(ENABLE_ENV);
    sweep_journal::remove_sweep_at(&e1, "/repo/a", 2).unwrap();
    let before = std::fs::read(&e1).unwrap();
    let (code, report) = verify(root.path());
    assert_eq!(code, 1, "{report:#}");
    assert_eq!(report["unexplained"][0]["item"], "/repo/a#2");
    assert_eq!(std::fs::read(&e1).unwrap(), before);

    // The legacy store stays fully readable without the journal (downgrade).
    std::fs::remove_dir_all(root.path().join(execution::STREAM)).unwrap();
    let legacy = sweep_journal::load(&e1);
    assert_eq!(legacy.entries.len(), 1);
    assert_eq!(legacy.entries[0].issue, 1);
    std::env::remove_var(DIR_ENV);
}
