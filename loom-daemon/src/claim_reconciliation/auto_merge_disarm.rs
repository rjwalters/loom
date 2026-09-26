//! Disarm GitHub's server-side auto-merge as part of invalidating a verdict
//! (issue #8900) — the daemon-native half of the fix, mirroring
//! `verdict-staleness-guard.sh --clear`'s inline `gh api graphql` disarm.
//!
//! Clearing `loom:pr` for a head move used to be purely a label + comment
//! transition. That is not enough: an auto-merge armed earlier is gated ONLY by
//! the branch ruleset's REQUIRED checks and never re-reads the label, so it
//! fired the moment those checks went green on the new, unreviewed head. The
//! label said "back in the review queue" and the forge merged it anyway —
//! #8694, #8847 and #8843 all merged that way on 2026-09-25.
//!
//! Split into its own file rather than inlined into `claim_reconciliation.rs`
//! per the file-size ratchet (`.loom/docs/file-size-policy.md`), same as
//! [`super::pr_label_info`]. The mutation itself lives one level further out,
//! in [`crate::forge_disable_auto_merge`], so the CLI verb
//! (`loom-daemon forge disable-auto-merge`) and this hook cannot drift apart.

use std::path::Path;

use crate::forge_disable_auto_merge::{disarm_auto_merge, Disarm};

/// Disarm any armed auto-merge on `pr_number` **before** its verdict is
/// invalidated, and return the audit line to append to the stale-verdict
/// comment (`None` when there is nothing to say).
///
/// # Ordering
///
/// This runs before the comment, which runs before the label flip. That is the
/// safest of the three possible orders: the disarm can only *prevent* a merge,
/// so doing it first shrinks the window in which the queued merge could still
/// fire, and a later comment/label failure leaves the PR disarmed with its
/// verdict intact — strictly safer than the pre-#8900 behavior. Doing it last
/// would leave that window open for the duration of two more `gh` calls, which
/// is exactly how long #8694 needed (it merged three minutes after the push).
///
/// # No-op discipline
///
/// A PR with nothing armed returns `None` and sends no mutation, so an ordinary
/// stale-verdict clear costs one extra read and never claims a disarm that did
/// not happen. A disarm that *failed* returns a line saying so — the comment
/// must not imply the queue was stood down when it may still be armed.
pub(super) fn disarm_before_invalidation(
    gh_bin: &Path,
    root: &Path,
    pr_number: u32,
) -> Option<String> {
    match disarm_auto_merge(gh_bin, Some(root), pr_number) {
        Disarm::NotArmed => None,
        Disarm::Disarmed => {
            log::warn!(
                "claim_reconciliation: disarmed GitHub auto-merge on PR #{pr_number} in {} while \
                 invalidating its stale verdict — an armed queue ignores the label flip and \
                 merges the unreviewed head as soon as required checks pass (#8900)",
                root.display(),
            );
            Some(
                "- **GitHub auto-merge disarmed** (`disablePullRequestAutoMerge`): a queued \
                 server-side merge was armed on this PR. It is gated only by the ruleset's \
                 required checks and would have merged this unreviewed head regardless of the \
                 label flip above, so it was stood down (#8900)."
                    .to_string(),
            )
        }
        Disarm::Failed(reason) => {
            log::warn!(
                "claim_reconciliation: could not determine or disarm GitHub auto-merge on PR \
                 #{pr_number} in {}: {reason} — the verdict is still being invalidated, but a \
                 queued merge may remain armed (#8900)",
                root.display(),
            );
            Some(format!(
                "- ⚠️ **Auto-merge state could not be confirmed**: `{reason}`. If a server-side \
                 auto-merge is armed on this PR it may still merge this unreviewed head once \
                 required checks pass — disarm it by hand (`loom-daemon forge \
                 disable-auto-merge {pr_number}`) or apply `loom:operator` (#8900)."
            ))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::super::{forge, VERDICT_STALENESS_ENABLED_ENV};
    use serial_test::serial;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    const SHA_A: &str = "1111111111111111111111111111111111111111";
    const SHA_B: &str = "2222222222222222222222222222222222222222";

    /// A fake `gh` reproducing #8694's exact shape for
    /// `forge::reconcile_pr_verdicts`: PR #8694 carries `loom:pr`, its only
    /// marker is an `approved` one recorded for `SHA_A`, the reported head is
    /// `SHA_B` (a Doctor rebase force-push moved it), and `pr view --json
    /// id,autoMergeRequest` reports auto-merge ARMED (auto-squash, armed
    /// 2026-09-22 by the fleet app).
    ///
    /// `armed` toggles the arm state; `graphql_rc` makes the disable mutation
    /// fail. Every invocation is logged so the test can assert on exactly which
    /// calls were made, in particular that no mutation fires when nothing is
    /// armed.
    fn fake_gh(
        dir: &std::path::Path,
        log: &std::path::Path,
        armed: bool,
        graphql_rc: i32,
    ) -> std::path::PathBuf {
        let auto_merge = if armed {
            r#"{"mergeMethod":"SQUASH","enabledAt":"2026-09-22T22:09:18Z"}"#
        } else {
            "null"
        };
        let bin = dir.join("fake-gh-disarm.sh");
        let script = format!(
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  echo '[{{"number":8694,"headRefOid":"{sha_b}","labels":[{{"name":"loom:pr"}}]}}]'
  exit 0
fi
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then
  echo '{{"id":"PR_kwDOQAPbH88AAAABEp4dZw","autoMergeRequest":{auto_merge}}}'
  exit 0
fi
if [ "$1" = "api" ] && [ "$2" = "graphql" ]; then
  if [ "{graphql_rc}" != "0" ]; then
    echo 'gh: GraphQL: Resource not accessible (disablePullRequestAutoMerge)' 1>&2
    exit {graphql_rc}
  fi
  echo '{{"data":{{"disablePullRequestAutoMerge":{{"pullRequest":{{"number":8694}}}}}}}}'
  exit 0
fi
if [ "$1" = "api" ]; then
  echo '[{{"created_at":"2026-09-22T22:00:00Z","body":"LGTM.\n\n<!-- loom:verdict-sha sha={sha_a} verdict=approved -->"}}]'
  exit 0
fi
exit 0
"#,
            log = log.display(),
            sha_a = SHA_A,
            sha_b = SHA_B,
        );
        std::fs::write(&bin, script).unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();
        bin
    }

    fn with_staleness_on<T>(f: impl FnOnce() -> T) -> T {
        let prev = std::env::var(VERDICT_STALENESS_ENABLED_ENV).ok();
        std::env::set_var(VERDICT_STALENESS_ENABLED_ENV, "1");
        let out = f();
        match prev {
            Some(v) => std::env::set_var(VERDICT_STALENESS_ENABLED_ENV, v),
            None => std::env::remove_var(VERDICT_STALENESS_ENABLED_ENV),
        }
        out
    }

    /// THE #8694 REGRESSION, end to end through the daemon backstop: a stale
    /// `loom:pr` on a PR with auto-merge armed must have the armed queue
    /// disarmed, not merely the label flipped. Before #8900 the pass made only
    /// the comment + label writes and the queued merge fired anyway — #8694
    /// merged as `528f2971` three minutes after the force-push, still labeled
    /// `loom:review-requested` and with no approval at the merged head.
    #[test]
    #[serial]
    fn invalidating_a_stale_verdict_disarms_an_armed_auto_merge() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let log = dir.path().join("gh.log");
        let gh = fake_gh(dir.path(), &log, true, 0);

        let stats = with_staleness_on(|| forge::reconcile_pr_verdicts(&gh, &repo_root));
        assert_eq!(stats.invalidated, 1, "the stale approval must be invalidated");

        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(
            calls.contains("pr view 8694 --json id,autoMergeRequest"),
            "the arm state must be read: {calls}"
        );
        assert!(
            calls.contains("disablePullRequestAutoMerge"),
            "the armed auto-merge must be disarmed (#8900): {calls}"
        );
        assert!(
            !calls.contains("enablePullRequestAutoMerge"),
            "it must be the DISABLE mutation, never the arm: {calls}"
        );
        assert!(
            calls.contains("pullRequestId=PR_kwDOQAPbH88AAAABEp4dZw"),
            "the mutation must address the PR by the node id read alongside the arm state: {calls}"
        );

        // The comment body is multi-line, so the fake `gh`'s per-invocation log
        // entry spans several lines; assert against the whole log. "auto-merge
        // disarmed" can only come from the comment body — the mutation call is
        // logged as a single `api graphql …` line that does not contain it.
        assert!(
            calls.lines().any(|l| l.starts_with("pr comment 8694")),
            "no stale-verdict comment recorded in:\n{calls}"
        );
        assert!(
            calls.contains("auto-merge disarmed"),
            "the comment must record the disarm: {calls}"
        );

        // Disarm FIRST, then comment, then labels — see the module doc.
        let order: Vec<&str> = calls
            .lines()
            .filter(|l| {
                l.contains("disablePullRequestAutoMerge")
                    || l.starts_with("pr comment 8694")
                    || l.starts_with("pr edit 8694")
            })
            .collect();
        assert!(
            order
                .first()
                .is_some_and(|l| l.contains("disablePullRequestAutoMerge")),
            "the disarm must precede the comment and the label flip: {order:?}"
        );
    }

    /// The common case: nothing armed. No mutation is sent (no wasted API call
    /// on every ordinary clear) and the comment does not claim a disarm.
    #[test]
    #[serial]
    fn an_unarmed_pr_sends_no_mutation_and_claims_no_disarm() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let log = dir.path().join("gh.log");
        let gh = fake_gh(dir.path(), &log, false, 0);

        let stats = with_staleness_on(|| forge::reconcile_pr_verdicts(&gh, &repo_root));
        assert_eq!(stats.invalidated, 1, "the clear itself is unaffected");

        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(!calls.contains("graphql"), "an unarmed PR must not trigger a mutation: {calls}");
        assert!(
            calls.lines().any(|l| l.starts_with("pr comment 8694")),
            "no stale-verdict comment recorded in:\n{calls}"
        );
        assert!(
            !calls.contains("auto-merge disarmed"),
            "the comment must not claim a disarm that never happened: {calls}"
        );
        assert!(
            !calls.contains("could not be confirmed"),
            "nor emit a spurious warning: {calls}"
        );
        assert!(
            calls.lines().any(|l| l.starts_with("pr edit 8694")),
            "the label flip must still happen: {calls}"
        );
    }

    /// A FAILED disarm must not abort the invalidation (a stale verdict is
    /// still stale) and must not be reported as a disarm — the comment says the
    /// queue may still fire, so an operator knows there is something left to do.
    #[test]
    #[serial]
    fn a_failed_disarm_warns_in_the_comment_and_still_clears_the_verdict() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        let log = dir.path().join("gh.log");
        let gh = fake_gh(dir.path(), &log, true, 1);

        let stats = with_staleness_on(|| forge::reconcile_pr_verdicts(&gh, &repo_root));
        assert_eq!(stats.invalidated, 1, "a failed disarm must not block clearing a stale verdict");

        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(
            calls.lines().any(|l| l.starts_with("pr comment 8694")),
            "no stale-verdict comment recorded in:\n{calls}"
        );
        assert!(
            calls.contains("could not be confirmed"),
            "the comment must warn rather than claim a disarm: {calls}"
        );
        assert!(
            !calls.contains("auto-merge disarmed"),
            "a failed disarm is never reported as disarmed: {calls}"
        );
        assert!(
            calls.lines().any(|l| l.starts_with("pr edit 8694")),
            "the label flip must still happen: {calls}"
        );
    }
}
