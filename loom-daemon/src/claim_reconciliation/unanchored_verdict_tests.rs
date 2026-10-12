//! #9258: an approval with no trusted `loom:verdict-sha` marker is never
//! anchored at the current head and is re-queued instead; a rejection with no
//! marker still anchors (#6319). Pure `decide_anchor` cases plus one
//! end-to-end pass through `forge::reconcile_pr_verdicts` with a fake `gh`, in
//! a sibling file because neither this module nor `tests.rs` has ratchet
//! headroom (scripts/file-size-baseline.txt).

use super::unanchored_verdict::{requeue_body, UNANCHORED_APPROVAL_MARKER_PREFIX};
use super::*;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

const SHA_A: &str = "1111111111111111111111111111111111111111";
const SHA_B: &str = "2222222222222222222222222222222222222222";

fn unmarked(kind: VerdictKind, head: Option<&str>) -> VerdictPr {
    VerdictPr {
        number: 300,
        kind,
        head_sha: head.map(str::to_string),
        marker_sha: None,
        marker_scan_ok: true,
        on_hold: false,
        invalidation_recorded: false,
    }
}

#[test]
fn an_unmarked_approval_is_requeued_never_anchored() {
    assert_eq!(
        decide_anchor(&unmarked(VerdictKind::Approved, Some(SHA_B))),
        AnchorAction::RequeueApproval
    );
    // Even with no resolvable head: there is nothing to anchor to, and the
    // approval still describes no known tree.
    assert_eq!(
        decide_anchor(&unmarked(VerdictKind::Approved, None)),
        AnchorAction::RequeueApproval
    );
    // An empty marker is no marker.
    let mut empty = unmarked(VerdictKind::Approved, Some(SHA_B));
    empty.marker_sha = Some(String::new());
    assert_eq!(decide_anchor(&empty), AnchorAction::RequeueApproval);
}

#[test]
fn an_unmarked_rejection_still_anchors_at_the_head() {
    assert_eq!(
        decide_anchor(&unmarked(VerdictKind::ChangesRequested, Some(SHA_A))),
        AnchorAction::Anchor {
            head_sha: SHA_A.to_string()
        }
    );
}

#[test]
fn an_approval_is_requeued_only_on_positive_evidence() {
    // A held PR is never written to.
    let mut held = unmarked(VerdictKind::Approved, Some(SHA_B));
    held.on_hold = true;
    held.marker_scan_ok = false;
    assert_eq!(decide_anchor(&held), AnchorAction::Skip(AnchorSkipReason::Held));
    // A failed comment read is not "no marker": re-queueing on it would turn
    // a GitHub outage into a mass re-review.
    let mut unread = unmarked(VerdictKind::Approved, Some(SHA_B));
    unread.marker_scan_ok = false;
    assert_eq!(decide_anchor(&unread), AnchorAction::Skip(AnchorSkipReason::MarkerScanFailed));
    // A marked approval belongs to decide_verdict, fresh or stale.
    let mut marked = unmarked(VerdictKind::Approved, Some(SHA_B));
    marked.marker_sha = Some(SHA_A.to_string());
    assert_eq!(decide_anchor(&marked), AnchorAction::Skip(AnchorSkipReason::AlreadyAnchored));
}

#[test]
fn the_requeue_comment_is_never_read_as_a_verdict_marker() {
    // If the explanation carried a verdict-sha marker, it would itself
    // anchor the approval it is withdrawing.
    let body = requeue_body(SHA_B);
    assert!(body.starts_with(&format!("{UNANCHORED_APPROVAL_MARKER_PREFIX}{SHA_B} -->")));
    let bodies = vec![body.clone()];
    assert_eq!(extract_latest_verdict_sha(&bodies, VerdictKind::Approved), None);
    assert_eq!(extract_latest_verdict_sha(&bodies, VerdictKind::ChangesRequested), None);
    assert!(body.contains("re-review is required"), "{body}");
    assert!(body.contains("post-verdict.sh"), "{body}");
}

#[test]
fn stats_count_a_requeue_as_remediated() {
    let mut stats = VerdictReconcileStats {
        unverifiable: 3,
        anchored: 1,
        unanchored_approvals_requeued: 1,
        ..VerdictReconcileStats::default()
    };
    assert_eq!(stats.residual_unverifiable(), 1);
    stats.merge(VerdictReconcileStats {
        unanchored_approvals_requeued: 2,
        ..VerdictReconcileStats::default()
    });
    assert_eq!(stats.unanchored_approvals_requeued, 3);
    assert_eq!(stats.residual_unverifiable(), 0, "never underflows");
}

/// One pass over PR #300 carrying `label` at `SHA_B` with a single trusted
/// comment whose body is the literal `@-` (the #9258 incident: no marker).
/// Returns the stats and the fake `gh`'s write log.
fn reconcile_unmarked(label: &str, anchoring: bool) -> (VerdictReconcileStats, String) {
    use super::open_pr_listing::test_support::{pulls_arm, row};
    let dir = tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let listing = dir.path().join("comments.json");
    std::fs::write(
        &listing,
        r#"[{"user":{"login":"maintainer","type":"User"},"author_association":"COLLABORATOR","created_at":"2026-09-29T00:00:00Z","body":"@-"}]"#,
    )
    .unwrap();
    let writes = dir.path().join("writes.log");
    let gh = dir.path().join("fake-gh.sh");
    std::fs::write(
        &gh,
        format!(
            r#"#!/usr/bin/env bash
{pulls}case "$*" in
  "pr edit "*|"pr comment "*) printf '%s\n' "$*" >> "{writes}" ;;
  "api repos/{{owner}}/{{repo}}/issues/300/comments"*) cat "{listing}" ;;
  *) echo '{{}}' ;;
esac
"#,
            listing = listing.display(),
            writes = writes.display(),
            pulls = pulls_arm(&[row(300, &[label]).sha(SHA_B)]),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let prev_gh_bin = std::env::var_os("LOOM_GH_BIN");
    std::env::set_var("LOOM_GH_BIN", &gh);
    std::env::set_var(VERDICT_ANCHOR_ENABLED_ENV, if anchoring { "1" } else { "0" });
    let stats = forge::reconcile_pr_verdicts(&gh, &root);
    std::env::remove_var(VERDICT_ANCHOR_ENABLED_ENV);
    match prev_gh_bin {
        Some(v) => std::env::set_var("LOOM_GH_BIN", v),
        None => std::env::remove_var("LOOM_GH_BIN"),
    }
    (stats, std::fs::read_to_string(&writes).unwrap_or_default())
}

#[test]
#[serial]
fn reconcile_requeues_an_unmarked_approval_and_posts_no_anchor() {
    let (stats, writes) = reconcile_unmarked("loom:pr", true);
    assert_eq!(stats.unverifiable, 1, "{stats:?}");
    assert_eq!(stats.anchored, 0, "an approval is never anchored: {stats:?}");
    assert_eq!(stats.unanchored_approvals_requeued, 1, "{stats:?}");
    assert!(!writes.contains("loom:verdict-sha"), "no anchor marker may be posted: {writes}");
    assert!(
        writes.contains(
            "pr edit 300 --remove-label loom:pr --remove-label loom:changes-requested \
             --add-label loom:review-requested"
        ),
        "{writes}"
    );
    assert!(writes.contains(UNANCHORED_APPROVAL_MARKER_PREFIX), "{writes}");
    // Labels first, then the comment (#10601).
    let edit = writes.find("pr edit 300").unwrap();
    let comment = writes.find("pr comment 300").unwrap();
    assert!(edit < comment, "{writes}");
}

#[test]
#[serial]
fn the_requeue_does_not_depend_on_the_anchor_switch() {
    let (stats, writes) = reconcile_unmarked("loom:pr", false);
    assert_eq!(stats.unanchored_approvals_requeued, 1, "{stats:?}");
    assert!(writes.contains("--add-label loom:review-requested"), "{writes}");
}

#[test]
#[serial]
fn reconcile_still_anchors_an_unmarked_rejection() {
    let (stats, writes) = reconcile_unmarked("loom:changes-requested", true);
    assert_eq!((stats.anchored, stats.unanchored_approvals_requeued), (1, 0), "{stats:?}");
    assert!(
        writes
            .contains(&format!("<!-- loom:verdict-sha sha={SHA_B} verdict=changes-requested -->")),
        "{writes}"
    );
    assert!(!writes.contains("pr edit"), "anchoring writes no labels: {writes}");
}
