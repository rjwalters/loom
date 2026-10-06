#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! #10089 (increment 7): the sequence-hold reads, `cmd_out::gh_json` and the
//! write-scope probe reach `forge_call_stats`, and the claim-reconciliation
//! family's telemetry names record an inventoried operation, not `unknown`.

use crate::claim_reconciliation::gh_call;
use crate::forge_call_stats::{self, ops};
use crate::types::{ForgeCallCounts, ForgeOperationCounts};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A stub that appends its argv to `log` and prints `out`.
fn logging_stub(dir: &Path, log: &Path, out: &str) -> PathBuf {
    stub(
        dir,
        "gh-log",
        &format!("printf '%s\\n' \"$*\" >> '{}'\nprintf '%s' '{out}'", log.display()),
    )
}

type Report = (Vec<ForgeCallCounts>, Vec<ForgeOperationCounts>);

fn report_after(body: impl FnOnce()) -> Report {
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

/// Every name the mapping covers resolves to an inventoried constant (and so,
/// by `every_named_operation_is_inventoried`, to an active inventory row).
#[test]
fn every_mapped_claim_name_is_an_inventoried_op() {
    let mapped = [
        "claim.pr_timeline",
        "quarantine.issue_timeline",
        "claim.lease_comments",
        "claim.pr_activity_comments",
        "verdict.pr_comments",
        "quarantine.issue_comments",
        "sequence.trusted_bodies",
        "roster.comments",
        "claim.pr_labels",
        "sequence.predecessor",
        "intake.list_open",
        "quarantine.issue_list",
        "claim.issue_reclaim",
        "claim.pr_add_label",
        "claim.pr_reclaim",
        "quarantine.issue_release",
        "heal.issue_add_label",
        "intake.add_triage",
        "verdict.clear_labels",
        "sequence.pr_edit",
        "review_conflict.pr_edit",
        "verdict.anchor_comment",
        "verdict.reanchor_comment",
        "verdict.stale_comment",
        "sequence.pr_comment",
        "review_conflict.pr_comment",
        "roster.delete",
        "roster.patch",
        "guard.open_pr_timeline",
        "outcome.label_timeline",
        "guard.open_pr_graphql",
        "guard.lease_comments",
        "guard.flip_building",
        "quarantine.label",
        "quarantine.release",
        "restore.label",
        "prless.hold_label",
        "guard.lease_comment",
        "guard.lease_yield_comment",
        "quarantine.comment",
        "watchdog.gaveup_comment",
        "watchdog.stale_comment",
        "outcome.writeback_comment",
        "prless.comment",
    ];
    for name in mapped {
        let op = gh_call::forge_op_for(name).unwrap_or_else(|| panic!("{name} is unmapped"));
        assert!(ops::ALL_INVENTORIED.contains(&op), "{name} -> {op:?} not inventoried");
    }
    for deliberate in [
        "claim.issue_state",
        "claim.issue_labels",
        "star.api",
        "sequence.pr_view",
    ] {
        assert_eq!(gh_call::forge_op_for(deliberate), None, "{deliberate}");
    }
}

/// End to end: a mapped claim read books its inventoried operation, an
/// unmapped one stays a visible `unknown`.
#[test]
#[serial_test::serial]
fn a_claim_timeline_read_records_timeline_read_not_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(tmp.path(), "gh-ok", "echo '[]'");
    let (rows, operations) = report_after(|| {
        let _ = gh_call::ok_stdout(gh_call::read("claim.pr_timeline", &gh, tmp.path()));
        let _ = gh_call::ok_stdout(gh_call::read("claim.issue_state", &gh, tmp.path()));
    });
    assert_eq!(calls(&rows, "claim.pr_timeline"), 1, "{rows:?}");
    assert_eq!(op_calls(&operations, "timeline.read"), 1, "{operations:?}");
    assert_eq!(op_calls(&operations, forge_call_stats::UNKNOWN_OPERATION), 1, "{operations:?}");
}

/// The merge-sequence hold reads were raw spawns hidden from the ratchet
/// (`Command::new(bin)`): each is now one counted, inventoried row, and the
/// comments walk pages at 100.
#[test]
#[serial_test::serial]
fn sequence_hold_reads_are_counted_and_page_at_100() {
    use crate::merge_pr::sequence::{fetch_predecessor, fetch_trusted_bodies};
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("argv.log");
    let gh = logging_stub(tmp.path(), &log, "[]");
    let bin = gh.to_string_lossy().to_string();
    let (rows, operations) = report_after(|| {
        assert_eq!(fetch_trusted_bodies(&bin, tmp.path(), "o/r", 7), Some(vec![]));
        let _ = fetch_predecessor(&bin, tmp.path(), "o/r", 6);
    });
    assert_eq!(calls(&rows, "sequence.trusted_bodies"), 1, "{rows:?}");
    assert_eq!(calls(&rows, "sequence.predecessor"), 1, "{rows:?}");
    assert_eq!(op_calls(&operations, "comment.list"), 1, "{operations:?}");
    assert_eq!(op_calls(&operations, "pr.view-state"), 1, "{operations:?}");
    let argv = std::fs::read_to_string(&log).unwrap();
    assert!(
        argv.contains("api repos/o/r/issues/7/comments?per_page=100 --paginate"),
        "{argv}"
    );
}

#[test]
#[serial_test::serial]
fn gh_json_is_one_counted_read_under_its_operation() {
    use crate::cmd_out::{gh_json, Query};
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(tmp.path(), "gh-json", r#"echo '{"number":3}'"#);
    let mut got = None;
    let (rows, _) = report_after(|| {
        got = Some(gh_json::<serde_json::Value, _>(
            "jev.tier_issue_view",
            &gh,
            &["issue", "view", "3", "--json", "number"],
            tmp.path(),
            std::time::Duration::from_secs(10),
            |_| false,
        ));
    });
    assert!(matches!(got, Some(Query::Populated(_))), "{got:?}");
    assert_eq!(calls(&rows, "jev.tier_issue_view"), 1, "{rows:?}");
}

/// The write-scope probe runs under the credential it is testing: the
/// explicit `GH_CONFIG_DIR` reaches the child, and each leg is counted.
#[test]
#[serial_test::serial]
fn write_scope_probe_is_counted_under_its_explicit_credential() {
    use crate::write_scope::probe::{GhProbe, Permission, PermissionProbe};
    let tmp = tempfile::tempdir().unwrap();
    let cfg = tmp.path().join("cfg");
    let gh = stub(
        tmp.path(),
        "gh-perm",
        r#"[ "$GH_CONFIG_DIR" = "$EXPECT_CFG" ] || { echo wrong-credential >&2; exit 1; }
echo '{"push":true}'"#,
    );
    std::env::set_var("EXPECT_CFG", &cfg);
    let probe = GhProbe::new(gh, Some(cfg));
    let mut got = None;
    let (rows, _) = report_after(|| got = Some(probe.permission("o/r")));
    std::env::remove_var("EXPECT_CFG");
    assert!(matches!(got, Some(Permission::Write)), "{got:?}");
    assert_eq!(calls(&rows, "write_scope.probe"), 1, "{rows:?}");
}

/// #10089: a PR watch and the verdict base-ref read book `pr.view-state`; an
/// issue watch (no inventoried issue-view op) stays an honest `unknown`.
#[test]
fn pr_watch_and_base_ref_book_pr_view_state_and_issue_watch_stays_unknown() {
    use crate::watch_registry::{GhWatchProbe, WatchKind, WatchProbe, WatchSpec};
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(
        tmp.path(),
        "gh-json",
        r#"echo '{"state":"OPEN","labels":[],"baseRefName":"main"}'"#,
    );
    let spec = |kind, number| WatchSpec {
        id: format!("watch-{number}"),
        kind,
        number,
        repo: None,
        workspace_root: Some(tmp.path().to_string_lossy().into_owned()),
        note: None,
        registered_at: chrono::Utc::now(),
    };
    let (_rows, operations) = report_after(|| {
        let probe = GhWatchProbe::new().with_gh_bin(gh.clone());
        let _ = probe.probe(&spec(WatchKind::Pr, 3));
        let _ = probe.probe(&spec(WatchKind::Issue, 4));
        assert_eq!(
            crate::verdict_equivalence::pr_base_ref(&gh, Some(tmp.path()), 3).as_deref(),
            Some("main")
        );
    });
    assert_eq!(op_calls(&operations, "pr.view-state"), 2, "{operations:?}");
    assert_eq!(op_calls(&operations, forge_call_stats::UNKNOWN_OPERATION), 1, "{operations:?}");
}

/// #10089: the one comment POST (now also the merge tree-checks comment)
/// books the inventoried `comment.create`, not `unknown`.
#[test]
#[serial_test::serial]
fn comment_post_books_comment_create() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(tmp.path(), "gh-post", "echo '{}'");
    let (rows, operations) = report_after(|| {
        let posted = crate::forge_comment::post_comment(&gh, None, "o/r", 3, true, "hi");
        assert!(posted.is_ok(), "{posted:?}");
    });
    assert_eq!(calls(&rows, "comment.post"), 1, "{rows:?}");
    assert_eq!(op_calls(&operations, "comment.create"), 1, "{operations:?}");
    assert_eq!(op_calls(&operations, forge_call_stats::UNKNOWN_OPERATION), 0, "{operations:?}");
}
