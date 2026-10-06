#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! #10089: `fleet drain`'s claim reset goes through the `gh` facade — each
//! call is one counted, named row, and the explicit `PATH` still reaches the
//! child.

use super::reset_claim_with;
use crate::forge_call_stats;
use crate::types::{ForgeCallCounts, ForgeOperationCounts};
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// A stub `gh` that logs `PATH` and its argv, answers `issue view` with
/// `labels_json`, and fails `issue edit` when `edit_exit` is non-zero.
fn stub(dir: &Path, labels_json: &str, edit_exit: u8) -> (PathBuf, PathBuf) {
    let log = dir.join("argv.log");
    let path = dir.join("gh-drain");
    let body = format!(
        r#"#!/bin/sh
printf 'PATH=%s|%s\n' "$PATH" "$*" >> '{log}'
case "$1 $2" in
  "issue view") printf '%s' '{labels_json}' ;;
  "issue edit") exit {edit_exit} ;;
esac
"#,
        log = log.display()
    );
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (path, log)
}

fn report_after(body: impl FnOnce()) -> (Vec<ForgeCallCounts>, Vec<ForgeOperationCounts>) {
    let sink = tempfile::tempdir().unwrap();
    forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));
    body();
    let report = forge_call_stats::status_report(chrono::Utc::now(), None);
    forge_call_stats::set_test_sink_dir(None);
    (report.host_window.unwrap_or_default(), report.operations.unwrap_or_default())
}

fn calls(rows: &[ForgeCallCounts], caller: &str) -> u64 {
    rows.iter()
        .filter(|r| r.caller == caller)
        .map(|r| r.ok + r.error)
        .sum()
}

fn op_calls(rows: &[ForgeOperationCounts], operation: &str) -> u64 {
    rows.iter()
        .filter(|r| r.operation == operation)
        .map(|r| r.ok + r.error)
        .sum()
}

/// A child `PATH` that still finds `/bin/sh` for the stub's shebang.
fn child_path() -> OsString {
    OsString::from("/opt/loom-drain-test/bin:/usr/bin:/bin")
}

#[test]
#[serial_test::serial]
fn a_held_claim_is_reset_in_three_counted_calls_under_the_explicit_path() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) = stub(tmp.path(), r#"{"labels":[{"name":"loom:building"}]}"#, 0);
    let mut got = None;
    let (rows, operations) = report_after(|| {
        got = Some(reset_claim_with(gh.as_os_str(), child_path(), "o/r", 7, "worker-1"));
    });
    assert!(matches!(got, Some(Ok(true))), "{got:?}");
    for caller in [
        "drain.issue_labels",
        "drain.reset_labels",
        "drain.reset_comment",
    ] {
        assert_eq!(calls(&rows, caller), 1, "{caller}: {rows:?}");
    }
    assert_eq!(op_calls(&operations, "issue.edit-labels"), 1, "{operations:?}");
    assert_eq!(op_calls(&operations, "comment.create"), 1, "{operations:?}");
    assert_eq!(op_calls(&operations, forge_call_stats::UNKNOWN_OPERATION), 1, "{operations:?}");

    let argv = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = argv.lines().collect();
    assert_eq!(lines.len(), 3, "{argv}");
    for line in &lines {
        assert!(line.starts_with("PATH=/opt/loom-drain-test/bin:/usr/bin:/bin|"), "{line}");
    }
    assert!(lines[0].ends_with("|issue view 7 --repo o/r --json labels"), "{argv}");
    assert!(
        lines[1].ends_with(
            "|issue edit 7 --repo o/r --remove-label loom:building --add-label loom:issue"
        ),
        "{argv}"
    );
    assert!(lines[2].contains("|issue comment 7 --repo o/r --body "), "{argv}");
    assert!(lines[2].contains("host `worker-1`"), "{argv}");
}

#[test]
#[serial_test::serial]
fn a_released_claim_costs_one_read_and_no_write() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) = stub(tmp.path(), r#"{"labels":[{"name":"loom:issue"}]}"#, 0);
    let mut got = None;
    let (rows, _) = report_after(|| {
        got = Some(reset_claim_with(gh.as_os_str(), child_path(), "o/r", 8, "worker-1"));
    });
    assert!(matches!(got, Some(Ok(false))), "{got:?}");
    assert_eq!(calls(&rows, "drain.issue_labels"), 1, "{rows:?}");
    assert_eq!(calls(&rows, "drain.reset_labels"), 0, "{rows:?}");
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
}

#[test]
#[serial_test::serial]
fn a_failed_label_edit_is_an_error_and_posts_no_comment() {
    let tmp = tempfile::tempdir().unwrap();
    let (gh, log) = stub(tmp.path(), r#"{"labels":[{"name":"loom:building"}]}"#, 1);
    let got = reset_claim_with(gh.as_os_str(), child_path(), "o/r", 9, "worker-1");
    let err = got.unwrap_err().to_string();
    assert!(err.contains("gh issue edit #9 in o/r failed"), "{err}");
    assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 2);
}
