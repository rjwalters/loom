#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! #10089 (increment 2): the raw `gh` spawns migrated off hand-built
//! `Command`s now reach `forge_call_stats`, one row per call, under their
//! stable operation names — and keep the result classification they had.

use crate::forge_call_stats;
use crate::forge_check_claim::{read_claim_labels, read_freshest_live_lease, LabelLeg, LeaseLeg};
use crate::sweep_registry::test_support::collision_registry;
use crate::sweep_registry::CollisionClass;
use crate::types::ForgeCallCounts;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn rows_after(body: impl FnOnce()) -> Vec<ForgeCallCounts> {
    let sink = tempfile::tempdir().unwrap();
    forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));
    body();
    let report = forge_call_stats::status_report(chrono::Utc::now(), None);
    forge_call_stats::set_test_sink_dir(None);
    report.host_window.unwrap_or_default()
}

fn calls(rows: &[ForgeCallCounts], caller: &str) -> u64 {
    rows.iter()
        .filter(|r| r.caller == caller)
        .map(|r| r.ok + r.error)
        .sum()
}

#[test]
fn claim_check_legs_are_counted_and_keep_their_classification() {
    let tmp = tempfile::tempdir().unwrap();
    let labels = stub(tmp.path(), "gh-labels", r#"echo '{"labels":[{"name":"loom:issue"}]}'"#);
    let failing = stub(tmp.path(), "gh-fail", "echo boom >&2; exit 1");
    let now = chrono::Utc::now();
    let mut leg = None;
    let mut failed_labels = None;
    let mut lease = None;
    let rows = rows_after(|| {
        leg = Some(read_claim_labels(&labels, tmp.path(), 7));
        failed_labels = Some(read_claim_labels(&failing, tmp.path(), 7));
        lease = Some(read_freshest_live_lease(&failing, tmp.path(), 7, now, 30.0));
    });
    assert_eq!(leg, Some(LabelLeg::Clean));
    assert_eq!(failed_labels, Some(LabelLeg::Unknown));
    assert_eq!(lease, Some(LeaseLeg::ReadFailed));
    assert_eq!(calls(&rows, "claim.labels"), 2, "{rows:?}");
    assert_eq!(calls(&rows, "claim.lease_comments"), 1, "{rows:?}");
}

#[test]
fn dispatch_guard_probe_is_counted_under_its_operation() {
    let tmp = tempfile::tempdir().unwrap();
    let registry = collision_registry(
        tmp.path(),
        &tmp.path().join("gh.log"),
        r#"{"labels":[{"name":"loom:issue"}]}"#,
        0,
    );
    let mut class = None;
    let rows = rows_after(|| class = Some(registry.classify_preflip_labels(42)));
    assert_eq!(class, Some(CollisionClass::Clean));
    assert_eq!(calls(&rows, "guard.preflip_labels"), 1, "{rows:?}");
}

#[test]
#[serial(loom_config_env)]
fn worktree_clean_probe_is_counted_and_unreadable_is_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(tmp.path(), "gh-owner", "echo some-owner");
    std::env::set_var("LOOM_GH_BIN", &gh);
    let mut owner = None;
    let rows = rows_after(|| owner = crate::worktree_ops::clean::repo_owner_rest(tmp.path()));
    std::env::remove_var("LOOM_GH_BIN");
    assert_eq!(owner.as_deref(), Some("some-owner"));
    assert_eq!(calls(&rows, "clean.repo_owner"), 1, "{rows:?}");
}
