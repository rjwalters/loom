#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! #10089 (increment 6): the merge-consolidation and `forge comment`
//! `--patch-created` read spawns reach `forge_call_stats`, one row each.

use crate::forge_call_stats;
use crate::merge_pr::consolidate::{default_branch, fetch_component};
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
fn consolidate_default_branch_is_one_counted_read() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(tmp.path(), "gh-db", r#"echo '{"defaultBranchRef":{"name":"main"}}'"#);
    let mut out = None;
    let rows = rows_after(|| out = Some(default_branch(&gh, tmp.path())));
    assert_eq!(out.unwrap().unwrap(), "main");
    assert_eq!(calls(&rows, "consolidate.default_branch"), 1, "{rows:?}");
}

#[test]
#[serial_test::serial]
fn consolidate_component_fetch_counts_view_and_files_and_fails_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let ok = stub(
        tmp.path(),
        "gh-ok",
        r#"case "$1" in pr) echo '{"state":"OPEN","isDraft":false,"headRefOid":"abc","baseRefName":"main","labels":[],"additions":1,"deletions":0}';; *) echo a.rs;; esac"#,
    );
    let bad = stub(tmp.path(), "gh-bad", "echo boom >&2; exit 1");
    let (mut good, mut failed) = (None, None);
    let rows = rows_after(|| {
        good = Some(fetch_component(&ok, tmp.path(), 7));
        failed = Some(fetch_component(&bad, tmp.path(), 7));
    });
    assert!(good.unwrap().is_ok());
    assert!(failed.unwrap().is_err());
    assert_eq!(calls(&rows, "consolidate.component"), 2, "{rows:?}");
    assert_eq!(calls(&rows, "consolidate.component_files"), 1, "{rows:?}");
}
