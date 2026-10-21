//! #12041: a Judge-approved `loom:pr` PR that goes `CONFLICTING` is routed to
//! Doctor by this pass instead of waiting for a Champion merge attempt.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

const SHA: &str = "4444444444444444444444444444444444444444";

fn pr(mergeable: Mergeable, labels: &[&str]) -> ConflictPr {
    ConflictPr {
        number: 12041,
        head_sha: Some(SHA.to_string()),
        mergeable,
        labels: labels.iter().map(ToString::to_string).collect(),
        updated_at: None,
    }
}

fn flag_action() -> ConflictAction {
    ConflictAction::Flag {
        head_sha: SHA.to_string(),
    }
}

#[test]
fn conflicting_approved_pr_is_flagged() {
    assert_eq!(decide_review_conflict(&pr(Mergeable::Conflicting, &[APPROVED])), flag_action());
    // Both approval and review-request labels: still one flag.
    assert_eq!(
        decide_review_conflict(&pr(Mergeable::Conflicting, &[APPROVED, REVIEW_REQUESTED])),
        flag_action()
    );
}

#[test]
fn approved_pr_without_head_sha_is_kept() {
    let mut p = pr(Mergeable::Conflicting, &[APPROVED]);
    p.head_sha = None;
    assert_eq!(decide_review_conflict(&p), ConflictAction::Keep(ConflictKeepReason::NoHeadSha));
}

#[test]
fn clean_or_unknown_approved_pr_is_left_alone() {
    assert_eq!(
        decide_review_conflict(&pr(Mergeable::Mergeable, &[APPROVED])),
        ConflictAction::Keep(ConflictKeepReason::NoChange)
    );
    for labels in [
        &[APPROVED][..],
        &[APPROVED, "loom:operator"][..],
        &[APPROVED, "loom:sequenced"][..],
    ] {
        assert_eq!(
            decide_review_conflict(&pr(Mergeable::Unknown, labels)),
            ConflictAction::Keep(ConflictKeepReason::Unknown)
        );
    }
}

#[test]
fn operator_or_sequenced_approved_pr_gets_the_label_only() {
    for extra in ["loom:operator", "loom:operator-only", "loom:sequenced"] {
        assert_eq!(
            decide_review_conflict(&pr(Mergeable::Conflicting, &[APPROVED, extra])),
            ConflictAction::LabelOnly,
            "{extra}"
        );
        // Already labelled: nothing left to do.
        assert_eq!(
            decide_review_conflict(&pr(Mergeable::Conflicting, &[APPROVED, extra, MERGE_CONFLICT])),
            ConflictAction::Keep(ConflictKeepReason::NoChange),
            "{extra}"
        );
    }
}

#[test]
fn blocked_and_frozen_approved_prs_are_never_touched() {
    for hold in ["loom:blocked", "loom:frozen"] {
        for labels in [vec![APPROVED, hold], vec![APPROVED, hold, "loom:operator"]] {
            assert_eq!(
                decide_review_conflict(&pr(Mergeable::Conflicting, &labels)),
                ConflictAction::Keep(ConflictKeepReason::Held),
                "{labels:?}"
            );
        }
    }
}

#[test]
fn operator_held_pr_without_approval_stays_held() {
    // The label-only carve-out is for `loom:pr` PRs; a held review-queue PR
    // keeps the #8922 rule.
    assert_eq!(
        decide_review_conflict(&pr(Mergeable::Conflicting, &[REVIEW_REQUESTED, "loom:operator"])),
        ConflictAction::Keep(ConflictKeepReason::Held)
    );
}

#[test]
fn mergeable_held_or_sequenced_approved_pr_loses_the_label_only() {
    for extra in ["loom:operator", "loom:operator-only", "loom:sequenced"] {
        assert_eq!(
            decide_review_conflict(&pr(Mergeable::Mergeable, &[APPROVED, extra, MERGE_CONFLICT])),
            ConflictAction::LabelClear,
            "{extra}"
        );
        // No label to remove: nothing to do.
        let keep = if extra == "loom:sequenced" {
            ConflictKeepReason::NoChange
        } else {
            ConflictKeepReason::Held
        };
        assert_eq!(
            decide_review_conflict(&pr(Mergeable::Mergeable, &[APPROVED, extra])),
            ConflictAction::Keep(keep),
            "{extra}"
        );
        // No information: leave the label.
        assert_eq!(
            decide_review_conflict(&pr(Mergeable::Unknown, &[APPROVED, extra, MERGE_CONFLICT])),
            ConflictAction::Keep(ConflictKeepReason::Unknown),
            "{extra}"
        );
    }
}

#[test]
fn mergeable_blocked_or_frozen_approved_pr_is_never_cleared() {
    for hold in ["loom:blocked", "loom:frozen"] {
        for extra in ["loom:operator", "loom:sequenced"] {
            assert_eq!(
                decide_review_conflict(&pr(
                    Mergeable::Mergeable,
                    &[APPROVED, hold, extra, MERGE_CONFLICT]
                )),
                ConflictAction::Keep(ConflictKeepReason::Held),
                "{hold} {extra}"
            );
        }
    }
}

#[test]
fn flag_path_labels_still_go_through_clear_if_ours() {
    assert_eq!(
        decide_review_conflict(&pr(Mergeable::Mergeable, &[CHANGES_REQUESTED, MERGE_CONFLICT])),
        ConflictAction::ClearIfOurs
    );
}

#[test]
fn in_flight_approved_pr_is_left_to_the_agent() {
    for flight in IN_FLIGHT_LABELS {
        assert_eq!(
            decide_review_conflict(&pr(Mergeable::Conflicting, &[APPROVED, flight])),
            ConflictAction::Keep(ConflictKeepReason::InFlight)
        );
    }
}

#[test]
fn approved_flag_comment_says_the_approval_is_set_aside() {
    let body = flag_comment_body_removing(SHA, &[APPROVED]);
    assert!(body.contains(BASE_CONFLICT_MARKER));
    assert!(body.contains("- Removed: `loom:pr`"), "{body}");
    assert!(body.contains("approval is set aside"), "{body}");
    assert!(flag_is_latest(&[body]));
    // The review-queue wording is unchanged.
    assert_eq!(flag_comment_body(SHA), flag_comment_body_removing(SHA, &[REVIEW_REQUESTED]));
}

// ---- end to end through a fake `gh` ----

fn run(open_json: &str, comments: &str) -> (ReviewConflictStats, String) {
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let log = dir.path().join("gh.log");
    std::fs::write(&log, "").unwrap();
    let rows: Vec<serde_json::Value> = serde_json::from_str(open_json).unwrap();
    let mut arms = String::new();
    let rest: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let number = r["number"].as_u64().unwrap();
            let m = match r["mergeable"].as_str() {
                Some("MERGEABLE") => "true",
                Some("CONFLICTING") => "false",
                _ => "null",
            };
            arms.push_str(&format!(
                "case \"$*\" in api*'/pulls/{number}') printf 'HTTP/2.0 200 OK\\r\\n\\r\\n'; \
                 echo '{{\"mergeable\":{m},\"head\":{{\"sha\":\"{SHA}\"}}}}'; exit 0 ;; esac\n"
            ));
            serde_json::json!({
                "number": number,
                "labels": r["labels"],
                "head": {"ref": "feature/x", "sha": SHA},
            })
        })
        .collect();
    let pulls = super::open_pr_listing::test_support::pulls_arm_cmd(&format!(
        "echo '{}'",
        serde_json::Value::Array(rest)
    ));
    let gh = dir.path().join("fake-gh.sh");
    std::fs::write(
        &gh,
        format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\n{pulls}{arms}\
             if [ \"$1\" = \"api\" ]; then echo '{comments}'; exit 0; fi\nexit 0\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let prev = std::env::var(REVIEW_CONFLICT_ENABLED_ENV).ok();
    std::env::remove_var(REVIEW_CONFLICT_ENABLED_ENV);
    let stats = reconcile_review_conflicts(&gh, &root);
    if let Some(v) = prev {
        std::env::set_var(REVIEW_CONFLICT_ENABLED_ENV, v);
    }
    (stats, std::fs::read_to_string(&log).unwrap())
}

fn open(number: u32, mergeable: &str, labels: &[&str]) -> String {
    let labels: Vec<String> = labels
        .iter()
        .map(|l| format!(r#"{{"name":"{l}"}}"#))
        .collect();
    format!(
        r#"[{{"number":{number},"mergeable":"{mergeable}","labels":[{}]}}]"#,
        labels.join(",")
    )
}

#[test]
#[serial]
fn end_to_end_conflicting_approved_pr_is_flagged_once_and_routed_to_doctor() {
    let (stats, calls) = run(&open(12041, "CONFLICTING", &["loom:pr"]), "[]");
    assert_eq!(stats.flagged, 1, "{calls}");
    assert_eq!(calls.matches("pr comment 12041").count(), 1, "{calls}");
    assert!(
        calls.find("pr comment 12041").unwrap() < calls.find("pr edit 12041").unwrap(),
        "comment must precede the relabel: {calls}"
    );
    assert!(
        calls.contains(
            "pr edit 12041 --remove-label loom:pr --add-label loom:changes-requested \
             --add-label loom:merge-conflict"
        ),
        "{calls}"
    );
}

#[test]
#[serial]
fn end_to_end_pr_with_both_approval_labels_loses_both() {
    let (stats, calls) =
        run(&open(12041, "CONFLICTING", &["loom:pr", "loom:review-requested"]), "[]");
    assert_eq!(stats.flagged, 1, "{calls}");
    assert!(
        calls.contains(
            "pr edit 12041 --remove-label loom:review-requested --remove-label loom:pr \
             --add-label loom:changes-requested --add-label loom:merge-conflict"
        ),
        "{calls}"
    );
}

#[test]
#[serial]
fn end_to_end_operator_and_sequenced_approved_prs_get_the_label_only() {
    for extra in ["loom:operator", "loom:sequenced"] {
        let (stats, calls) = run(&open(12041, "CONFLICTING", &["loom:pr", extra]), "[]");
        assert_eq!((stats.flagged, stats.labeled), (0, 1), "{extra}: {calls}");
        assert!(!calls.contains("pr comment"), "{extra}: {calls}");
        assert!(calls.contains("pr edit 12041 --add-label loom:merge-conflict"), "{calls}");
        assert!(!calls.contains("--remove-label"), "{extra}: {calls}");
    }
}

#[test]
#[serial]
fn end_to_end_frozen_unknown_and_in_flight_approved_prs_write_nothing() {
    for (m, label) in [
        ("CONFLICTING", "loom:frozen"),
        ("CONFLICTING", "loom:blocked"),
        ("CONFLICTING", "loom:reviewing"),
        ("CONFLICTING", "loom:treating"),
        ("UNKNOWN", "loom:merge-conflict"),
    ] {
        let (stats, calls) = run(&open(12041, m, &["loom:pr", label]), "[]");
        assert_eq!((stats.flagged, stats.labeled, stats.cleared), (0, 0, 0), "{label}");
        assert!(!calls.contains("pr edit") && !calls.contains("pr comment"), "{label}: {calls}");
    }
}

#[test]
#[serial]
fn end_to_end_flagged_approved_pr_returns_to_review_when_mergeable() {
    // After a flag the PR carries changes-requested + merge-conflict; once
    // MERGEABLE the existing clear path returns it to review, not to loom:pr.
    let ours = r#"[{"user":{"login":"loom-fleet-dispatch[bot]"},"body":"<!-- loom:base-conflict flagged -->\nflagged"}]"#;
    let (stats, calls) = run(
        &open(12041, "MERGEABLE", &["loom:changes-requested", "loom:merge-conflict"]),
        ours,
    );
    assert_eq!(stats.cleared, 1, "{calls}");
    assert!(calls.contains("--add-label loom:review-requested"), "{calls}");
    assert!(!calls.contains("--add-label loom:pr"), "{calls}");

    // A PR flagged by Judge/Doctor (no flag of ours) is not touched.
    let judge = format!(
        r#"[{{"author_association":"OWNER","body":"rebase <!-- loom:verdict-sha sha={SHA} verdict=changes-requested -->"}}]"#
    );
    let (stats, calls) = run(
        &open(12041, "MERGEABLE", &["loom:changes-requested", "loom:merge-conflict"]),
        &judge,
    );
    assert_eq!(stats.cleared, 0);
    assert!(!calls.contains("pr edit"), "{calls}");
}

#[test]
#[serial]
fn end_to_end_already_flagged_approved_pr_is_not_reflagged() {
    // loom:pr + loom:merge-conflict already present, still conflicting: the
    // sequenced/operator forms are NoChange; a plain one is re-flagged only
    // because loom:pr is present — which the flag itself removes, so a flagged
    // PR never carries it. Guard the label-only duplicate case here.
    let (stats, calls) = run(
        &open(12041, "CONFLICTING", &["loom:pr", "loom:operator", "loom:merge-conflict"]),
        "[]",
    );
    assert_eq!((stats.flagged, stats.labeled), (0, 0));
    assert!(!calls.contains("pr edit"), "{calls}");
}

#[test]
#[serial]
fn end_to_end_mergeable_held_and_sequenced_prs_lose_only_the_conflict_label() {
    for extra in ["loom:operator", "loom:sequenced"] {
        let (stats, calls) =
            run(&open(12041, "MERGEABLE", &["loom:pr", extra, "loom:merge-conflict"]), "[]");
        assert_eq!((stats.label_cleared, stats.cleared, stats.flagged), (1, 0, 0), "{extra}");
        assert!(calls.contains("pr edit 12041 --remove-label loom:merge-conflict"), "{calls}");
        assert!(!calls.contains("--add-label"), "{extra}: {calls}");
        assert!(!calls.contains("pr comment"), "{extra}: {calls}");
        assert!(!calls.contains("/comments"), "{extra}: {calls}");
    }
}

#[test]
#[serial]
fn end_to_end_unknown_mergeable_held_pr_keeps_the_label() {
    let (stats, calls) = run(
        &open(12041, "UNKNOWN", &["loom:pr", "loom:operator", "loom:merge-conflict"]),
        "[]",
    );
    assert_eq!(stats.label_cleared, 0);
    assert!(!calls.contains("pr edit"), "{calls}");
}
