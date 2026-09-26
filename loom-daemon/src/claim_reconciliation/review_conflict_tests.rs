//! Tests for the #8922 review-queue base-conflict pass.

use super::*;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

const SHA: &str = "3333333333333333333333333333333333333333";

fn pr(mergeable: Mergeable, labels: &[&str]) -> ConflictPr {
    ConflictPr {
        number: 8909,
        head_sha: Some(SHA.to_string()),
        mergeable,
        labels: labels.iter().map(ToString::to_string).collect(),
    }
}

// ---- AC1: a conflicting review-queue PR is flagged ----

#[test]
fn conflicting_review_requested_pr_is_flagged() {
    assert_eq!(
        decide_review_conflict(&pr(Mergeable::Conflicting, &[REVIEW_REQUESTED])),
        ConflictAction::Flag {
            head_sha: SHA.to_string()
        }
    );
}

#[test]
fn conflicting_pr_without_head_sha_is_kept() {
    let mut p = pr(Mergeable::Conflicting, &[REVIEW_REQUESTED]);
    p.head_sha = None;
    assert_eq!(decide_review_conflict(&p), ConflictAction::Keep(ConflictKeepReason::NoHeadSha));
}

#[test]
fn clean_review_requested_pr_is_left_alone() {
    assert_eq!(
        decide_review_conflict(&pr(Mergeable::Mergeable, &[REVIEW_REQUESTED])),
        ConflictAction::Keep(ConflictKeepReason::NoChange)
    );
}

// ---- AC2: mergeable again -> clear (only if ours) ----

#[test]
fn mergeable_flagged_pr_is_a_clear_candidate() {
    assert_eq!(
        decide_review_conflict(&pr(Mergeable::Mergeable, &[CHANGES_REQUESTED, MERGE_CONFLICT])),
        ConflictAction::ClearIfOurs
    );
}

#[test]
fn still_conflicting_flagged_pr_is_left_alone() {
    assert_eq!(
        decide_review_conflict(&pr(Mergeable::Conflicting, &[CHANGES_REQUESTED, MERGE_CONFLICT])),
        ConflictAction::Keep(ConflictKeepReason::NoChange)
    );
}

#[test]
fn flag_is_latest_only_when_our_flag_is_the_newest_state_marker() {
    let flag = flag_comment_body(SHA);
    let judge = format!("Needs work.\n{VERDICT_MARKER_PREFIX}{SHA} verdict=changes-requested -->");
    let cleared = format!("{BASE_CONFLICT_CLEARED_MARKER}\nback to review");
    let chatter = "just a comment".to_string();

    assert!(flag_is_latest(std::slice::from_ref(&flag)));
    assert!(flag_is_latest(&[judge.clone(), flag.clone(), chatter.clone()]));
    // A Judge verdict after our flag is not ours to undo.
    assert!(!flag_is_latest(&[flag.clone(), judge.clone()]));
    // Judge's own DIRTY fallback with no flag of ours.
    assert!(!flag_is_latest(&[judge]));
    // Already cleared.
    assert!(!flag_is_latest(&[flag, cleared]));
    assert!(!flag_is_latest(&[chatter]));
    assert!(!flag_is_latest(&[]));
}

#[test]
fn flag_comment_carries_a_fresh_changes_requested_verdict_marker() {
    // The stale-verdict pass must read our label as Fresh, not anchor it.
    let body = flag_comment_body(SHA);
    assert_eq!(
        crate::claim_reconciliation::extract_latest_verdict_sha(
            &[body],
            crate::claim_reconciliation::VerdictKind::ChangesRequested
        ),
        Some(SHA.to_string())
    );
}

// ---- AC3: UNKNOWN never changes a label ----

#[test]
fn unknown_mergeability_never_changes_labels_in_either_direction() {
    for labels in [
        &[REVIEW_REQUESTED][..],
        &[CHANGES_REQUESTED, MERGE_CONFLICT][..],
    ] {
        assert_eq!(
            decide_review_conflict(&pr(Mergeable::Unknown, labels)),
            ConflictAction::Keep(ConflictKeepReason::Unknown)
        );
    }
}

#[test]
fn mergeable_parse_treats_anything_unexpected_as_unknown() {
    assert_eq!(Mergeable::parse(Some("MERGEABLE")), Mergeable::Mergeable);
    assert_eq!(Mergeable::parse(Some("CONFLICTING")), Mergeable::Conflicting);
    assert_eq!(Mergeable::parse(Some("UNKNOWN")), Mergeable::Unknown);
    assert_eq!(Mergeable::parse(Some("")), Mergeable::Unknown);
    assert_eq!(Mergeable::parse(None), Mergeable::Unknown);
}

// ---- AC4: held PRs are untouched; in-flight agents too ----

#[test]
fn held_prs_are_never_touched() {
    for hold in VERDICT_HOLD_LABELS {
        assert_eq!(
            decide_review_conflict(&pr(Mergeable::Conflicting, &[REVIEW_REQUESTED, hold])),
            ConflictAction::Keep(ConflictKeepReason::Held)
        );
        assert_eq!(
            decide_review_conflict(&pr(
                Mergeable::Mergeable,
                &[CHANGES_REQUESTED, MERGE_CONFLICT, hold]
            )),
            ConflictAction::Keep(ConflictKeepReason::Held)
        );
    }
}

#[test]
fn prs_with_an_agent_in_flight_are_left_to_it() {
    for claim in IN_FLIGHT_LABELS {
        assert_eq!(
            decide_review_conflict(&pr(Mergeable::Conflicting, &[REVIEW_REQUESTED, claim])),
            ConflictAction::Keep(ConflictKeepReason::InFlight)
        );
    }
}

#[test]
fn parse_pr_list_reads_mergeable_and_labels() {
    let json = format!(
        r#"[{{"number":8909,"headRefOid":"{SHA}","mergeable":"CONFLICTING","labels":[{{"name":"loom:review-requested"}}]}},
            {{"number":8910,"labels":[]}}]"#
    );
    let prs = parse_pr_list(json.as_bytes()).unwrap();
    assert_eq!(prs[0], pr(Mergeable::Conflicting, &[REVIEW_REQUESTED]));
    assert_eq!(prs[1].mergeable, Mergeable::Unknown);
    assert_eq!(prs[1].head_sha, None);
}

// ---- end to end through a fake `gh` ----

/// Fake `gh`: `pr list --label loom:review-requested` returns `rr_json`,
/// `--label loom:merge-conflict` returns `mc_json`, `api` returns `comments`.
fn fake_gh(dir: &Path, log: &Path, rr_json: &str, mc_json: &str, comments: &str) -> PathBuf {
    let bin = dir.join("fake-gh-conflict.sh");
    let script = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  case "$*" in
    *"--label loom:review-requested"*) echo '{rr_json}' ;;
    *"--label loom:merge-conflict"*) echo '{mc_json}' ;;
    *) echo '[]' ;;
  esac
  exit 0
fi
if [ "$1" = "api" ]; then
  echo '{comments}'
  exit 0
fi
exit 0
"#,
        log = log.display(),
    );
    std::fs::write(&bin, script).unwrap();
    let mut perms = std::fs::metadata(&bin).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&bin, perms).unwrap();
    bin
}

use std::path::PathBuf;

fn run(rr_json: &str, mc_json: &str, comments: &str) -> (ReviewConflictStats, String) {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let log = dir.path().join("gh.log");
    std::fs::write(&log, "").unwrap();
    let gh = fake_gh(dir.path(), &log, rr_json, mc_json, comments);
    let prev = std::env::var(REVIEW_CONFLICT_ENABLED_ENV).ok();
    std::env::remove_var(REVIEW_CONFLICT_ENABLED_ENV);
    let stats = reconcile_review_conflicts(&gh, &root);
    if let Some(v) = prev {
        std::env::set_var(REVIEW_CONFLICT_ENABLED_ENV, v);
    }
    (stats, std::fs::read_to_string(&log).unwrap())
}

#[test]
#[serial]
fn end_to_end_conflicting_review_pr_is_relabeled_after_its_comment() {
    let rr = format!(
        r#"[{{"number":8909,"headRefOid":"{SHA}","mergeable":"CONFLICTING","labels":[{{"name":"loom:review-requested"}}]}}]"#
    );
    let (stats, calls) = run(&rr, "[]", "[]");
    assert_eq!(stats.flagged, 1, "{calls}");
    let comment = calls.find("pr comment 8909").expect("flag comment");
    let edit = calls.find("pr edit 8909").expect("relabel");
    assert!(comment < edit, "comment must precede the relabel: {calls}");
    assert!(calls.contains(
        "pr edit 8909 --remove-label loom:review-requested --add-label loom:changes-requested \
         --add-label loom:merge-conflict"
    ));
}

#[test]
#[serial]
fn end_to_end_unknown_writes_nothing() {
    let rr = format!(
        r#"[{{"number":8909,"headRefOid":"{SHA}","mergeable":"UNKNOWN","labels":[{{"name":"loom:review-requested"}}]}}]"#
    );
    let mc = format!(
        r#"[{{"number":8911,"headRefOid":"{SHA}","mergeable":"UNKNOWN","labels":[{{"name":"loom:merge-conflict"}},{{"name":"loom:changes-requested"}}]}}]"#
    );
    let (stats, calls) = run(&rr, &mc, "[]");
    assert_eq!((stats.flagged, stats.cleared), (0, 0));
    assert!(!calls.contains("pr edit") && !calls.contains("pr comment"), "{calls}");
}

#[test]
#[serial]
fn end_to_end_mergeable_again_is_cleared_only_when_we_flagged_it() {
    let mc = format!(
        r#"[{{"number":8909,"headRefOid":"{SHA}","mergeable":"MERGEABLE","labels":[{{"name":"loom:merge-conflict"}},{{"name":"loom:changes-requested"}}]}}]"#
    );
    let ours = r#"[{"body":"<!-- loom:base-conflict flagged -->\nflagged"}]"#;
    let (stats, calls) = run("[]", &mc, ours);
    assert_eq!(stats.cleared, 1, "{calls}");
    assert!(calls.contains(
        "pr edit 8909 --remove-label loom:merge-conflict --remove-label loom:changes-requested \
         --add-label loom:review-requested"
    ));

    // A Judge's own DIRTY fallback (no flag of ours) is never undone.
    let judges = format!(
        r#"[{{"body":"rebase please <!-- loom:verdict-sha sha={SHA} verdict=changes-requested -->"}}]"#
    );
    let (stats, calls) = run("[]", &mc, &judges);
    assert_eq!(stats.cleared, 0);
    assert!(!calls.contains("pr edit"), "{calls}");
}

#[test]
#[serial]
fn end_to_end_held_pr_is_untouched() {
    let rr = format!(
        r#"[{{"number":8909,"headRefOid":"{SHA}","mergeable":"CONFLICTING","labels":[{{"name":"loom:review-requested"}},{{"name":"loom:operator"}}]}}]"#
    );
    let (stats, calls) = run(&rr, "[]", "[]");
    assert_eq!(stats.flagged, 0);
    assert!(!calls.contains("pr edit") && !calls.contains("pr comment"), "{calls}");
}
