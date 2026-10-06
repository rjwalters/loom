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
        updated_at: None,
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

// ---- end to end through a fake `gh` ----

/// Fake `gh` (#10349: REST). The ETag'd open-PR listing returns every open PR
/// — the rows of `rr_json` (the `loom:review-requested` PRs) followed by those
/// of `mc_json` (the `loom:merge-conflict` PRs), written in the old `gh pr
/// list` shape and converted here — each PR's `GET pulls/<n>` answers its
/// `mergeable`, and the comments walk returns `comments`. Any GraphQL `gh pr
/// list` fails, so a regression back to it fails every end-to-end test here.
fn fake_gh(dir: &Path, log: &Path, rr_json: &str, mc_json: &str, comments: &str) -> PathBuf {
    let bin = dir.join("fake-gh-conflict.sh");
    let mut open: Vec<serde_json::Value> = serde_json::from_str(rr_json).unwrap();
    open.extend(serde_json::from_str::<Vec<serde_json::Value>>(mc_json).unwrap());
    let mut mergeable_arms = String::new();
    let rest: Vec<serde_json::Value> = open
        .iter()
        .map(|r| {
            let number = r["number"].as_u64().unwrap();
            let sha = r["headRefOid"].as_str().unwrap_or_default();
            let m = match r["mergeable"].as_str() {
                Some("MERGEABLE") => "true",
                Some("CONFLICTING") => "false",
                _ => "null",
            };
            mergeable_arms.push_str(&format!(
                "case \"$*\" in api*'/pulls/{number}') printf 'HTTP/2.0 200 OK\\r\\n\\r\\n'; \
                 echo '{{\"mergeable\":{m},\"head\":{{\"sha\":\"{sha}\"}}}}'; exit 0 ;; esac\n"
            ));
            serde_json::json!({
                "number": number,
                "labels": r["labels"],
                "head": {"ref": "feature/x", "sha": r["headRefOid"]},
            })
        })
        .collect();
    let pulls = super::open_pr_listing::test_support::pulls_arm_cmd(&format!(
        "echo '{}'",
        serde_json::Value::Array(rest)
    ));
    let script = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  echo 'GraphQL pr list is gone (#10349)' >&2
  exit 97
fi
{pulls}{mergeable_arms}if [ "$1" = "api" ]; then
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
    let ours = r#"[{"user":{"login":"loom-fleet-dispatch[bot]"},"body":"<!-- loom:base-conflict flagged -->\nflagged"}]"#;
    let (stats, calls) = run("[]", &mc, ours);
    assert_eq!(stats.cleared, 1, "{calls}");
    assert!(calls.contains(
        "pr edit 8909 --remove-label loom:merge-conflict --remove-label loom:changes-requested \
         --add-label loom:review-requested"
    ));

    // A Judge's own DIRTY fallback (no flag of ours) is never undone.
    let judges = format!(
        r#"[{{"author_association":"OWNER","body":"rebase please <!-- loom:verdict-sha sha={SHA} verdict=changes-requested -->"}}]"#
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

/// #9548 (H9): the "is this flag ours?" check reads trusted comments only, so
/// a flag marker written by an outsider, or by a user squatting the fleet
/// App's bare name, never clears a Judge's `loom:changes-requested`.
#[test]
#[serial]
fn end_to_end_an_untrusted_flag_marker_is_not_ours() {
    let mc = format!(
        r#"[{{"number":8909,"headRefOid":"{SHA}","mergeable":"MERGEABLE","labels":[{{"name":"loom:merge-conflict"}},{{"name":"loom:changes-requested"}}]}}]"#
    );
    for author in [
        r#""user":{"login":"drive-by","type":"User"},"author_association":"NONE""#,
        r#""user":{"login":"loom-fleet-dispatch","type":"User"},"author_association":"CONTRIBUTOR""#,
        r#""user":{"login":"other-fleet[bot]","type":"Bot"},"author_association":"NONE""#,
    ] {
        let listing = format!(r#"[{{{author},"body":"<!-- loom:base-conflict flagged -->\nx"}}]"#);
        let (stats, calls) = run("[]", &mc, &listing);
        assert_eq!(stats.cleared, 0, "{author}: {calls}");
        assert!(!calls.contains("pr edit"), "{author}: {calls}");
    }
}

// ---- #4429 follow-up: one open-PR listing per workspace, shared ----

/// The pass lists open PRs ONCE, through the REST listing (#10349), and
/// reads `mergeable` per candidate — never a GraphQL `gh pr list`.
#[test]
#[serial]
fn one_unfiltered_listing_per_workspace() {
    let rr = format!(
        r#"[{{"number":8909,"headRefOid":"{SHA}","mergeable":"MERGEABLE","labels":[{{"name":"loom:review-requested"}}]}}]"#
    );
    let (stats, calls) = run(&rr, "[]", "[]");
    assert_eq!(stats.checked, 1, "{calls}");
    let count = |needle: &str| calls.lines().filter(|l| l.contains(needle)).count();
    assert_eq!(count("pulls?state=open"), 1, "exactly one listing: {calls}");
    assert_eq!(count("/pulls/8909"), 1, "one mergeable read: {calls}");
    assert!(calls.lines().all(|l| !l.starts_with("pr list")), "{calls}");
}

/// Client-side filter: only PRs carrying either label are decided on, and a
/// PR carrying both is one candidate. `mergeable` is read only for a
/// candidate whose decision it can change (not a held or in-flight one).
#[test]
fn conflict_candidates_keeps_only_the_two_labels() {
    use super::open_pr_listing::test_support::{listing, row};
    let rows = crate::forge_pull_listing::parse_rest_pulls(&listing(&[
        row(1, &["loom:pr"]),
        row(2, &[REVIEW_REQUESTED]),
        row(3, &[MERGE_CONFLICT, REVIEW_REQUESTED]),
        row(4, &[]),
        row(5, &[REVIEW_REQUESTED, "loom:operator"]),
        row(6, &[MERGE_CONFLICT, "loom:treating"]),
    ]))
    .unwrap();
    let mut read = Vec::new();
    let got = conflict_candidates(&rows, |n| {
        read.push(n);
        (Mergeable::Conflicting, Some(format!("{n:040x}")))
    });
    assert_eq!(got.keys().copied().collect::<Vec<_>>(), vec![2, 3, 5, 6]);
    assert_eq!(read, vec![2, 3], "held / in-flight PRs are never read");
    assert_eq!(got[&2].mergeable, Mergeable::Conflicting);
    assert_eq!(got[&5].mergeable, Mergeable::Unknown);
    assert_eq!(got[&2].head_sha.as_deref(), Some(format!("{:040x}", 2).as_str()));
}

/// #10382: `mergeable` and the head it was computed for come from ONE
/// response. When the listing's head differs (a push between the two reads),
/// the flag names the per-PR head; a per-PR read without a head is Unknown.
#[test]
fn the_flag_names_the_head_the_mergeable_read_was_computed_for() {
    use super::open_pr_listing::test_support::{listing, row};
    let rows = crate::forge_pull_listing::parse_rest_pulls(&listing(&[
        row(1, &[REVIEW_REQUESTED]).sha("listinghead"),
        row(2, &[REVIEW_REQUESTED]).sha("listinghead"),
    ]))
    .unwrap();
    let got = conflict_candidates(&rows, |n| match n {
        1 => (Mergeable::Conflicting, Some("perprhead".to_string())),
        _ => (Mergeable::Conflicting, None),
    });
    assert_eq!(
        decide_review_conflict(&got[&1]),
        ConflictAction::Flag {
            head_sha: "perprhead".to_string()
        }
    );
    assert_eq!(
        decide_review_conflict(&got[&2]),
        ConflictAction::Keep(ConflictKeepReason::Unknown)
    );
}

fn run_sharing(rr_json: &str) -> (ReviewConflictStats, Option<Vec<RestPull>>, String) {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let log = dir.path().join("gh.log");
    std::fs::write(&log, "").unwrap();
    let gh = fake_gh(dir.path(), &log, rr_json, "[]", "[]");
    let prev = std::env::var(REVIEW_CONFLICT_ENABLED_ENV).ok();
    std::env::remove_var(REVIEW_CONFLICT_ENABLED_ENV);
    let (stats, shared) = reconcile_review_conflicts_sharing(&gh, &root);
    if let Some(v) = prev {
        std::env::set_var(REVIEW_CONFLICT_ENABLED_ENV, v);
    }
    (stats, shared, std::fs::read_to_string(&log).unwrap())
}

/// Nothing written → the listing is handed on for the merge-sequence pass.
#[test]
#[serial]
fn listing_is_shared_when_the_pass_wrote_nothing() {
    let rr = format!(
        r#"[{{"number":8909,"headRefOid":"{SHA}","mergeable":"MERGEABLE","labels":[{{"name":"loom:review-requested"}}]}}]"#
    );
    let (_, shared, calls) = run_sharing(&rr);
    let shared = shared.expect("an unwritten listing is shared");
    assert_eq!(shared.len(), 1, "{calls}");
    assert_eq!(shared[0].head_sha.as_deref(), Some(SHA), "{calls}");
}

/// A flag moved labels → the listing is stale and must NOT be handed on.
#[test]
#[serial]
fn listing_is_withheld_after_a_write() {
    let rr = format!(
        r#"[{{"number":8909,"headRefOid":"{SHA}","mergeable":"CONFLICTING","labels":[{{"name":"loom:review-requested"}}]}}]"#
    );
    let (stats, shared, calls) = run_sharing(&rr);
    assert_eq!(stats.flagged, 1, "{calls}");
    assert!(shared.is_none(), "a listing read before a relabel is stale");
}
