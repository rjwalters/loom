//! Issue #9124 coverage that spans the parent module rather than
//! [`super::verdict_stale_comment`] alone: the boundary between "which
//! verdicts are stale" (untouched), "how many times we say so" (deduped), and
//! — the fix that actually moves PL5a — "was the tree even different"
//! (skipped entirely when it was not).
//!
//! # The measurement, so the pins below are not mysterious
//!
//! `rjwalters/loom`, 150 most recent PRs, 2026-09-29 — `gh pr list --limit
//! 150` plus every `issues/<n>/comments` listing in that sample. 110
//! `loom:verdict-stale` comments over 83 distinct `(from, to)` transitions
//! (drift from any earlier same-day count is expected and does not change the
//! conclusion below — re-measure before acting on either number).
//!
//! Of those 83 transitions, **46 (55%) moved to a commit whose tree is
//! byte-identical to the one before it** — confirmed by fetching both
//! commits' `commit.tree.sha` from the GitHub API and finding them equal, not
//! by inference. Every one of the 46 is the same automated commit: the
//! `#8248` required-check-freshness guard's "re-date required checks for PR
//! #N (#8248 guard, automated by #8508)" commit, whose own message says so
//! explicitly:
//!
//! > This commit intentionally changes NOTHING in the tree — it exists only
//! > to give every required check a fresh started_at...
//!
//! The remaining 37 transitions all changed files (verified the same way);
//! no other same-tree pattern was found in this sample.
//!
//! # What this means for #9124's three questions
//!
//! 1. **What moves the head SHA after approval?** Two populations, not one:
//!    genuine content pushes (the majority of *invalidation-worthy* moves),
//!    and the `#8248`/`#8508` re-date commit (the majority of *transitions in
//!    this sample*, because it fires on every merge-queue retry).
//! 2. **Is the re-review substantive?** For the 46 re-date transitions: never
//!    — there is nothing to review, the tree the verdict already describes is
//!    exactly the tree at the new head. For the other 37: yes, by
//!    construction (a real file changed).
//! 3. **Is there a cheap equivalence test?** Yes, and a stronger one than the
//!    diff-vs-base heuristic #9124 originally proposed: `gh api
//!    compare/{marker_sha}...{head_sha}` reporting `files: []` proves the two
//!    trees are bit-for-bit identical, not merely "shaped like a rebase". See
//!    [`super::verdict_invalidation::tree_unchanged`].
//!
//! # What changed as a result
//!
//! [`super::forge::reconcile_pr_verdicts`] now asks that question before
//! clearing a verdict `decide_verdict` marked `Invalidate`. When the answer is
//! "yes, identical", it re-anchors the marker to the new head
//! ([`super::verdict_invalidation::reanchor_tree_unchanged_verdict`]) instead of clearing the
//! label — the verdict never leaves `loom:pr`/`loom:changes-requested`, so
//! **this is a direct reduction in PL5a**
//! (`pr_latency::segments::PrSegments::approval_invalidations`), not merely in
//! comment noise. [`decide_verdict`] itself is unchanged: the carve-out lives
//! strictly downstream of its answer, gated by live evidence, never inside the
//! pure decision function — see [`a_sha_move_always_invalidates_at_the_decide_verdict_layer`].
//!
//! The separate duplicate-comment fix in [`super::verdict_stale_comment`]
//! stays: it is still measurably true (see its own module doc) and is
//! complementary, not a substitute — it reduces comment noise on the
//! transitions that DO still get invalidated, which this carve-out narrows
//! but does not eliminate.
//!
//! Sibling file rather than an inline `mod`: `claim_reconciliation.rs` and its
//! `tests.rs` both sit at the file-size ratchet (`scripts/file-size-baseline.txt`),
//! and keeping test modules out of them is that policy's own preferred remedy.

#![allow(clippy::unwrap_used)]

use super::forge;
use super::verdict_stale_comment;
use crate::claim_reconciliation::{
    decide_verdict, VerdictAction, VerdictKeepReason, VerdictKind, VerdictPr, VerdictReconcileStats,
};

const SHA_A: &str = "1111111111111111111111111111111111111111";
const SHA_B: &str = "2222222222222222222222222222222222222222";

fn pr(head: Option<&str>, marker: Option<&str>, invalidation_recorded: bool) -> VerdictPr {
    VerdictPr {
        number: 9124,
        kind: VerdictKind::Approved,
        head_sha: head.map(str::to_string),
        marker_sha: marker.map(str::to_string),
        marker_scan_ok: true,
        on_hold: false,
        invalidation_recorded,
    }
}

#[test]
fn the_dedup_flag_never_changes_whether_a_verdict_is_invalidated() {
    // THE safety invariant of #9124's comment dedup. `invalidation_recorded`
    // must be invisible to the staleness decision: it gates one comment, not
    // the verdict. If this ever fails, a PR whose notice happens to be on
    // file would keep an approval for a tree nobody reviewed -- the #5686
    // hazard, reintroduced by a noise-reduction change.
    for recorded in [false, true] {
        assert_eq!(
            decide_verdict(&pr(Some(SHA_B), Some(SHA_A), recorded)),
            VerdictAction::Invalidate {
                marker_sha: SHA_A.to_string(),
                head_sha: SHA_B.to_string(),
            },
            "recorded={recorded}"
        );
        assert_eq!(
            decide_verdict(&pr(Some(SHA_A), Some(SHA_A), recorded)),
            VerdictAction::Keep(VerdictKeepReason::Fresh),
            "recorded={recorded}"
        );
        assert_eq!(
            decide_verdict(&pr(Some(SHA_B), None, recorded)),
            VerdictAction::Keep(VerdictKeepReason::Unverifiable),
            "recorded={recorded}"
        );
    }
}

#[test]
fn a_sha_move_always_invalidates_at_the_decide_verdict_layer() {
    // `decide_verdict` itself stays exactly as #5686 left it: any SHA move is
    // `Invalidate`, full stop, with no force-push-vs-fast-forward detector and
    // no tree comparison of its own. The #9124 tree-identical carve-out is
    // NOT implemented by changing this answer -- it is a strictly downstream
    // check in `forge::reconcile_pr_verdicts` that runs only after this
    // function has already said `Invalidate`, and only proceeds with the
    // clear once a live `gh api compare` call has confirmed the trees
    // actually differ. Keeping the pure decision unchanged means every
    // existing test of it (including the one above) still describes real
    // behavior, and the new evidence-gated step can fail closed (fall
    // through to this answer) on any API hiccup.
    assert_eq!(
        decide_verdict(&pr(Some(SHA_B), Some(SHA_A), false)),
        VerdictAction::Invalidate {
            marker_sha: SHA_A.to_string(),
            head_sha: SHA_B.to_string(),
        }
    );
}

#[test]
fn a_held_pr_is_never_deduped_into_silence() {
    // A held PR's comments are never fetched, so `invalidation_recorded` is
    // always false for one -- but it never reaches `Invalidate` either, so the
    // two facts cannot combine into "held PR whose notice is suppressed".
    let mut held = pr(Some(SHA_B), Some(SHA_A), false);
    held.on_hold = true;
    assert_eq!(decide_verdict(&held), VerdictAction::Keep(VerdictKeepReason::Held));
    assert!(!held.invalidation_recorded);
}

#[test]
fn an_unread_comment_listing_posts_the_notice_rather_than_assuming() {
    // `marker_scan_ok == false` is the API-outage case. Absence of evidence is
    // not evidence that someone else announced it: the flag stays false, so
    // the notice goes out. A duplicate comment is a cheap mistake; a silent
    // invalidation is not.
    let mut unread = pr(Some(SHA_B), Some(SHA_A), false);
    unread.marker_scan_ok = false;
    assert!(!unread.invalidation_recorded);
    assert!(verdict_stale_comment::should_post(unread.invalidation_recorded, false));
}

#[test]
fn the_skipped_comment_counter_merges_across_workspaces() {
    let mut a = VerdictReconcileStats {
        checked: 3,
        invalidated: 2,
        unverifiable: 1,
        anchored: 1,
        redundant_comments_skipped: 2,
        ..VerdictReconcileStats::default()
    };
    a.merge(VerdictReconcileStats {
        checked: 1,
        invalidated: 1,
        unverifiable: 0,
        anchored: 0,
        redundant_comments_skipped: 1,
        ..VerdictReconcileStats::default()
    });
    assert_eq!(a.invalidated, 3);
    assert_eq!(a.redundant_comments_skipped, 3);
    // The skip counter is a subset of the invalidations, never an alternative
    // to them: a deduped pass still invalidated.
    assert!(a.redundant_comments_skipped <= a.invalidated);
}

#[test]
fn the_tree_identical_counter_merges_and_is_never_counted_as_invalidated() {
    // The opposite relationship from the comment-skip counter above: a
    // tree-identical re-anchor is NOT an invalidation (the label never moved),
    // so it must never inflate `invalidated` -- that would double-count the
    // same PR pass as both "cleared" and "not cleared".
    let mut a = VerdictReconcileStats {
        checked: 2,
        invalidated: 1,
        tree_identical_reanchors: 1,
        ..VerdictReconcileStats::default()
    };
    a.merge(VerdictReconcileStats {
        checked: 1,
        tree_identical_reanchors: 1,
        ..VerdictReconcileStats::default()
    });
    assert_eq!(a.checked, 3);
    assert_eq!(a.invalidated, 1, "re-anchors must not be counted as invalidations");
    assert_eq!(a.tree_identical_reanchors, 2);
}

#[test]
fn the_default_stats_report_no_skips_or_reanchors() {
    assert_eq!(VerdictReconcileStats::default().redundant_comments_skipped, 0);
    assert_eq!(VerdictReconcileStats::default().tree_identical_reanchors, 0);
}

#[test]
fn the_repeat_shape_measured_on_pr_9348_is_suppressed_end_to_end() {
    // #9348's real sequence: one identity posted `from=712b164a to=5fbd60c2`
    // five times in 26 minutes because its label write never landed. Pass one
    // records the notice; passes two through five find it and stay quiet,
    // while still retrying the label swap.
    let (from, to) = (
        "712b164a56109f8d701674f1a95550406ad7aa06",
        "5fbd60c2608f81b141b8b9fba4ad9589f9bcb5fa",
    );
    let mut bodies: Vec<String> = vec!["Judge: approved.".to_string()];
    assert!(verdict_stale_comment::should_post(
        verdict_stale_comment::already_recorded(&bodies, from, to),
        false
    ));
    bodies.push(verdict_stale_comment::body("loom:pr", from, to, ""));
    for pass in 2..=5 {
        assert!(
            !verdict_stale_comment::should_post(
                verdict_stale_comment::already_recorded(&bodies, from, to),
                false
            ),
            "pass {pass} re-posted a notice already on the PR"
        );
    }
    assert_eq!(bodies.len(), 2, "exactly one notice for one transition");
}

// ============================================================================
// End-to-end coverage of the tree-identical carve-out through the public
// `forge::reconcile_pr_verdicts` entry point, mirroring the fake-`gh` harness
// `auto_merge_disarm.rs` already uses for the same function.
// ============================================================================

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tree_carveout_e2e {
    use super::forge;
    use super::{SHA_A, SHA_B};
    use crate::claim_reconciliation::verdict_invalidation::VERDICT_TREE_CARVEOUT_ENABLED_ENV;
    use crate::claim_reconciliation::VERDICT_STALENESS_ENABLED_ENV;
    use serial_test::serial;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    /// A fake `gh` for PR #9124: carries `loom:pr` approved at `SHA_A`, head
    /// is now `SHA_B` (the `#8248`/`#8508` re-date shape), and `pr view`
    /// reports no auto-merge armed. `compare_body` stands in for GitHub's
    /// `compare/{base}...{head}` response -- `{"files": []}` for a
    /// tree-identical move, a non-empty `files` array for a real one.
    fn fake_gh(
        dir: &std::path::Path,
        log: &std::path::Path,
        compare_body: &str,
    ) -> std::path::PathBuf {
        let bin = dir.join("fake-gh-tree-carveout.sh");
        let script = format!(
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  echo '[{{"number":9124,"headRefOid":"{sha_b}","labels":[{{"name":"loom:pr"}}]}}]'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  echo '{{"id":"PR_kwDOQAPbH88AAAABEp4dZw","autoMergeRequest":null}}'
  exit 0
fi
if [ "$1" = "api" ]; then
  case "$2" in
    *compare/*)
      echo '{compare_body}'
      exit 0
      ;;
  esac
  echo '[{{"created_at":"2026-09-22T22:00:00Z","body":"LGTM.\n\n<!-- loom:verdict-sha sha={sha_a} verdict=approved -->"}}]'
  exit 0
fi
exit 0
"#,
            log = log.display(),
            sha_a = SHA_A,
            sha_b = SHA_B,
            compare_body = compare_body,
        );
        std::fs::write(&bin, script).unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();
        bin
    }

    fn with_env<T>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
        let prev: Vec<_> = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var(k).ok()))
            .collect();
        for (k, v) in vars {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        let out = f();
        for (k, v) in prev {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        out
    }

    /// THE #9124 fix, end to end: a tree-identical head move must re-anchor,
    /// not invalidate. No label edit is ever sent -- the whole safety argument
    /// for skipping Judge is that the approved label never needed to move.
    #[test]
    #[serial]
    fn a_tree_identical_head_move_is_reanchored_not_invalidated() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let log = dir.path().join("gh.log");
        let gh = fake_gh(dir.path(), &log, r#"{"files": []}"#);

        let stats = with_env(
            &[
                (VERDICT_STALENESS_ENABLED_ENV, Some("1")),
                (VERDICT_TREE_CARVEOUT_ENABLED_ENV, Some("1")),
            ],
            || forge::reconcile_pr_verdicts(&gh, &repo_root),
        );

        assert_eq!(
            stats.tree_identical_reanchors, 1,
            "the identical-tree move must be re-anchored"
        );
        assert_eq!(stats.invalidated, 0, "it must NOT also count as an invalidation");

        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(calls.contains("compare/"), "the compare API must have been consulted: {calls}");
        assert!(
            !calls.lines().any(|l| l.starts_with("pr edit 9124")),
            "no label may be touched by a re-anchor: {calls}"
        );
        assert!(
            calls.lines().any(|l| l.starts_with("pr comment 9124")),
            "the re-anchor marker comment must still be posted: {calls}"
        );
    }

    /// The control case: when the trees genuinely differ, the carve-out must
    /// not swallow the invalidation -- #9124's whole safety argument depends
    /// on this staying a hard fallback, not a default.
    #[test]
    #[serial]
    fn a_genuinely_different_tree_is_still_invalidated() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let log = dir.path().join("gh.log");
        let gh = fake_gh(dir.path(), &log, r#"{"files": [{"filename": "src/lib.rs"}]}"#);

        let stats = with_env(
            &[
                (VERDICT_STALENESS_ENABLED_ENV, Some("1")),
                (VERDICT_TREE_CARVEOUT_ENABLED_ENV, Some("1")),
            ],
            || forge::reconcile_pr_verdicts(&gh, &repo_root),
        );

        assert_eq!(stats.invalidated, 1, "a real content change must still invalidate");
        assert_eq!(stats.tree_identical_reanchors, 0);

        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(
            calls.lines().any(|l| l.starts_with("pr edit 9124")),
            "the label flip must still happen for a real change: {calls}"
        );
    }

    /// The kill switch: disabling the carve-out restores the pre-#9124
    /// behavior even when the trees are identical.
    #[test]
    #[serial]
    fn the_kill_switch_restores_the_pre_9124_behavior() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let log = dir.path().join("gh.log");
        let gh = fake_gh(dir.path(), &log, r#"{"files": []}"#);

        let stats = with_env(
            &[
                (VERDICT_STALENESS_ENABLED_ENV, Some("1")),
                (VERDICT_TREE_CARVEOUT_ENABLED_ENV, Some("0")),
            ],
            || forge::reconcile_pr_verdicts(&gh, &repo_root),
        );

        assert_eq!(
            stats.invalidated, 1,
            "with the switch off, an identical tree still invalidates"
        );
        assert_eq!(stats.tree_identical_reanchors, 0);
    }
}
