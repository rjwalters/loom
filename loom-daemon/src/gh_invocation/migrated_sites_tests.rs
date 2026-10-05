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

#[test]
#[serial(loom_config_env)]
fn tree_compare_of_two_shas_is_spent_once_and_counted() {
    use crate::claim_reconciliation::read_cache::set_test_enabled;
    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    set_test_enabled(true);
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("gh.log");
    let gh = stub(
        tmp.path(),
        "gh-compare",
        &format!(r#"echo "$*" >> {}; echo '{{"status":"ahead","files":[]}}'"#, log.display()),
    );
    let failing = stub(tmp.path(), "gh-fail", "exit 1");
    let mut answers = Vec::new();
    let rows = rows_after(|| {
        for _ in 0..4 {
            answers.push(crate::forge_tree_unchanged::tree_unchanged(&gh, Some(tmp.path()), A, B));
        }
        // A failed compare is not stored: it is retried, never assumed.
        answers.push(crate::forge_tree_unchanged::tree_unchanged(&failing, Some(tmp.path()), A, B));
        answers.push(crate::forge_tree_unchanged::tree_unchanged(&failing, Some(tmp.path()), A, B));
    });
    set_test_enabled(false);
    assert_eq!(&answers[..4], &[Some(true); 4]);
    assert_eq!(&answers[4..], &[None, None]);
    let spawns = std::fs::read_to_string(&log).unwrap().lines().count();
    assert_eq!(spawns, 1, "four unchanged passes cost one compare");
    assert_eq!(calls(&rows, "forge.tree_compare"), 3, "{rows:?}");
}

#[test]
fn snapshot_and_complexity_reads_are_counted() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(tmp.path(), "gh-body", "echo 'no marker here'");
    let mut body = Some("x".to_string());
    let rows = rows_after(|| {
        body = crate::sweep_registry::fetch_issue_complexity(&gh, tmp.path(), 5);
    });
    assert_eq!(body, None);
    assert_eq!(calls(&rows, "model.issue_body"), 1, "{rows:?}");
}

// ---- #10089 increment 3: daemon tick readers + sweep_registry family ----

#[test]
fn watch_probe_is_counted_and_keeps_its_classification() {
    use crate::watch_registry::{GhWatchProbe, WatchKind, WatchOutcome, WatchProbe, WatchSpec};
    let tmp = tempfile::tempdir().unwrap();
    let closed = stub(tmp.path(), "gh-closed", r#"echo '{"state":"CLOSED","labels":[]}'"#);
    let failing = stub(tmp.path(), "gh-fail", "echo boom >&2; exit 1");
    let spec = WatchSpec {
        id: "watch-issue-9".to_string(),
        kind: WatchKind::Issue,
        number: 9,
        repo: None,
        workspace_root: Some(tmp.path().to_string_lossy().into_owned()),
        note: None,
        registered_at: chrono::Utc::now(),
    };
    let (mut ok, mut failed) = (None, None);
    let rows = rows_after(|| {
        ok = Some(
            GhWatchProbe::new()
                .with_gh_bin(closed)
                .probe(&spec)
                .unwrap(),
        );
        failed = Some(
            GhWatchProbe::new()
                .with_gh_bin(failing)
                .probe(&spec)
                .is_err(),
        );
    });
    assert_eq!(ok, Some(Some(WatchOutcome::Closed)));
    assert_eq!(failed, Some(true), "a non-zero exit stays an error");
    assert_eq!(calls(&rows, "watch.view"), 2, "{rows:?}");
}

// ---- #10089 increment 7: worktree_ops family + reclaim warning ----

#[test]
#[serial(loom_config_env)]
fn worktree_ops_probes_are_counted_and_keep_their_classification() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(tmp.path(), "gh-closed", "echo closed");
    std::env::set_var("LOOM_GH_BIN", &gh);
    let (mut state, mut closed_at, mut building) = (None, None, None);
    let rows = rows_after(|| {
        state = Some(crate::worktree_ops::gh::issue_state_rest(tmp.path(), 7));
        closed_at = Some(crate::worktree_ops::gh::issue_closed_at_rest(tmp.path(), 7));
        building = Some(crate::worktree_ops::gh::list_building_issues(tmp.path()).is_err());
    });
    std::env::remove_var("LOOM_GH_BIN");
    assert_eq!(state.as_deref(), Some("CLOSED"));
    assert_eq!(closed_at, Some(Some("closed".to_string())));
    assert_eq!(building, Some(true), "non-JSON stdout is a parse error");
    assert_eq!(calls(&rows, "worktree.issue_state_rest"), 1, "{rows:?}");
    assert_eq!(calls(&rows, "worktree.issue_closed_at"), 1, "{rows:?}");
    assert_eq!(calls(&rows, "worktree.list_building"), 1, "{rows:?}");
}

#[test]
fn reclaim_open_pr_probe_is_counted() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(tmp.path(), "gh-pr", "echo '[{\"number\":12}]'");
    let mut pr = None;
    let rows = rows_after(|| {
        pr = Some(crate::reclaim_pr_warning::open_pr_on_issue_branch(&gh, tmp.path(), 5));
    });
    assert_eq!(pr, Some(Some(12)));
    assert_eq!(calls(&rows, "reclaim.open_pr_probe"), 1, "{rows:?}");
}

#[test]
fn comment_post_is_counted_and_sends_its_body_as_an_input_file() {
    let tmp = tempfile::tempdir().unwrap();
    let body_copy = tmp.path().join("body.json");
    let gh = stub(
        tmp.path(),
        "gh-post",
        &format!(
            "prev=\nfor a in \"$@\"; do [ \"$prev\" = --input ] && cat \"$a\" > {}; prev=\"$a\"; done\necho '{{}}'",
            body_copy.display()
        ),
    );
    let failing = stub(tmp.path(), "gh-post-fail", "echo 'HTTP 502' >&2; exit 1");
    let (mut ok, mut err) = (None, None);
    let rows = rows_after(|| {
        ok = Some(crate::forge_comment::post_comment(&gh, None, "o/r", 3, false, "hi"));
        err = Some(crate::forge_comment::post_comment(&failing, None, "o/r", 3, true, "hi"));
    });
    assert_eq!(ok, Some(Ok("{}\n".to_string())));
    let err = err.unwrap().unwrap_err();
    assert!(err.contains("comment on o/r#3") && err.contains("HTTP 502"), "{err}");
    let sent: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&body_copy).unwrap()).unwrap();
    assert!(sent["body"].as_str().unwrap().starts_with("hi\n\n"), "{sent}");
    assert_eq!(calls(&rows, "comment.post"), 2, "{rows:?}");
}
