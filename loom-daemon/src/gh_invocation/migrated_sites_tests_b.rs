#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! #10089 (increment 5): the disarm and stacked-children `gh` spawns reach
//! `forge_call_stats` under their stable operation names and keep their
//! classification.

use crate::forge_call_stats;
use crate::forge_disable_auto_merge::{disarm_auto_merge, Disarm};
use crate::merge_pr::stacked_children::discover_open_children;
use crate::types::ForgeCallCounts;
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
#[serial_test::serial]
fn disarm_costs_one_counted_read_when_nothing_is_armed() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(tmp.path(), "gh-unarmed", r#"echo '{"id":"PR_x","autoMergeRequest":null}'"#);
    let mut out = None;
    let rows = rows_after(|| out = Some(disarm_auto_merge(&gh, None, 7)));
    assert_eq!(out, Some(Disarm::NotArmed));
    assert_eq!(calls(&rows, "disarm.arm_state"), 1, "{rows:?}");
    assert_eq!(calls(&rows, "disarm.mutation"), 0, "{rows:?}");
}

#[test]
#[serial_test::serial]
fn disarm_of_an_armed_pr_counts_read_and_mutation() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(
        tmp.path(),
        "gh-armed",
        r#"case "$1" in pr) echo '{"id":"PR_x","autoMergeRequest":{}}';; *) echo ok;; esac"#,
    );
    let mut out = None;
    let rows = rows_after(|| out = Some(disarm_auto_merge(&gh, None, 7)));
    assert_eq!(out, Some(Disarm::Disarmed));
    assert_eq!(calls(&rows, "disarm.arm_state"), 1, "{rows:?}");
    assert_eq!(calls(&rows, "disarm.mutation"), 1, "{rows:?}");
}

#[test]
#[serial_test::serial]
fn unreadable_arm_state_is_failed_never_not_armed() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(tmp.path(), "gh-fail", "echo boom >&2; exit 1");
    let mut out = None;
    let rows = rows_after(|| out = Some(disarm_auto_merge(&gh, None, 7)));
    assert!(matches!(out, Some(Disarm::Failed(_))), "{out:?}");
    assert_eq!(calls(&rows, "disarm.arm_state"), 1, "{rows:?}");
}

#[test]
#[serial_test::serial]
fn stacked_children_discovery_is_counted_and_fails_open() {
    let tmp = tempfile::tempdir().unwrap();
    let ok = stub(tmp.path(), "gh-ok", r#"echo '[{"number":1,"headRefName":"b"}]'"#);
    let bad = stub(tmp.path(), "gh-bad", "exit 1");
    let (mut good, mut failed) = (String::new(), String::new());
    let rows = rows_after(|| {
        good = discover_open_children(ok.to_str().unwrap(), "o/r", "main");
        failed = discover_open_children(bad.to_str().unwrap(), "o/r", "main");
    });
    assert!(good.contains("headRefName"));
    assert_eq!(failed, "[]");
    assert_eq!(calls(&rows, "merge_guard.stacked_children"), 2, "{rows:?}");
}
