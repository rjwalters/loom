//! `ci-telemetry status --json` must keep emitting the deprecated top-level
//! `"org"` alias (issue #9197 item 1) — the comma-joined owner logins,
//! exactly as `status.json` (`PollStatus::org`) already writes. #9188
//! (`owners`/`owners_source`) dropped it from the JSON output when it
//! replaced the single-owner `org` field; a consumer keyed on `.org` (a
//! script, a dashboard) must not silently start reading nothing.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::process::Command;

fn run_status_json(root: &std::path::Path) -> serde_json::Value {
    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args([
            "ci-telemetry",
            "status",
            "--json",
            "--workspace",
            root.to_str().unwrap(),
        ])
        .output()
        .expect("run loom-daemon ci-telemetry status --json");
    assert!(
        out.status.success(),
        "status --json failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("status --json must print valid JSON")
}

fn init_workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".git")).unwrap();
    std::fs::create_dir_all(dir.path().join(".loom").join("state").join("ci-telemetry")).unwrap();
    dir
}

#[test]
fn a_never_polled_workspace_still_has_a_null_org_key() {
    // The key must exist (not be entirely absent) even before any cycle has
    // ever run and set `PollStatus::org`.
    let dir = init_workspace();
    let value = run_status_json(dir.path());
    assert!(value.get("org").is_some(), "{value}");
    assert!(value["org"].is_null(), "{value}");
}

#[test]
fn the_comma_joined_owner_logins_from_status_json_are_echoed_as_the_org_alias() {
    let dir = init_workspace();
    let status_path = dir
        .path()
        .join(".loom")
        .join("state")
        .join("ci-telemetry")
        .join("status.json");
    std::fs::write(&status_path, r#"{"org": "fixture-org,fixture-user"}"#).unwrap();

    let value = run_status_json(dir.path());
    assert_eq!(value["org"].as_str(), Some("fixture-org,fixture-user"), "{value}");
    // Present alongside, not instead of, the #9188 `owners` list.
    assert!(value.get("owners").is_some(), "{value}");
    assert!(value.get("owners_source").is_some(), "{value}");
}
