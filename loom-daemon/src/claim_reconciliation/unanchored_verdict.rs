//! Everything [`super::forge::reconcile_pr_verdicts`] does once
//! [`super::decide_verdict`] has answered `Keep(Unverifiable)`: count the
//! unmarked verdict, then act on [`super::decide_anchor`]'s answer.
//!
//! # Why a sibling file
//!
//! `claim_reconciliation.rs` is frozen by the file-size ratchet
//! (`.loom/docs/file-size-policy.md`). #9258 adds a new branch to this arm, so
//! the arm and the `anchor_verdict` write it already made move here with it,
//! leaving one dispatch call behind, the same treatment
//! [`super::verdict_invalidation`] got for the `Invalidate` arm.
//!
//! # The two answers (#6319, #9258)
//!
//! - **Anchor** (`loom:changes-requested` only, since #9258): post a marker
//!   recording the current head so the verdict becomes invalidatable. It
//!   writes no labels. A rejection cannot merge anything, so pinning it to the
//!   head it is standing on costs nothing.
//! - **Re-queue** (`loom:pr`): an approval with no trusted marker is not merge
//!   eligible and is never anchored. Since #6382 every sanctioned verdict path
//!   (`post-verdict.sh`) appends a marker, so a markerless approval means
//!   something bypassed the mechanism, as in the #9258 incident: an approval
//!   whose body was the literal `@-`. Anchoring it would have stamped a tree
//!   nobody reviewed as approved. This pass removes `loom:pr`, adds
//!   `loom:review-requested`, and says why on the PR.

use anyhow::{anyhow, Result};
use std::path::Path;

use super::{decide_anchor, gh_call, AnchorAction, VerdictKind, VerdictPr, VerdictReconcileStats};

/// The marker the re-queue comment carries. The shell guard
/// (`verdict-staleness-guard.sh --clear`) writes the same one for the same
/// transition, so the two paths' comments read alike.
pub(super) const UNANCHORED_APPROVAL_MARKER_PREFIX: &str = "<!-- loom:unanchored-approval head=";

/// Handle one `Keep(Unverifiable)` verdict. `anchoring` is the
/// `LOOM_VERDICT_ANCHOR` switch, read once per pass by the caller. It gates
/// only the anchor write. The approval re-queue is a safety action, gated (like
/// every write in this pass) by `LOOM_VERDICT_STALENESS_RECONCILE` alone.
pub(super) fn handle_unverifiable(
    gh_bin: &Path,
    root: &Path,
    pr: &VerdictPr,
    anchoring: bool,
    stats: &mut VerdictReconcileStats,
) {
    // Count only what we POSITIVELY know is unanchored. A held PR's comments
    // are never fetched and a failed fetch looks identical to "no marker".
    // Folding either into the counter (or re-queueing on it) would turn an
    // API outage into a fake integrity alarm.
    if !pr.marker_scan_ok {
        return;
    }
    stats.unverifiable += 1;
    log::warn!(
        "claim_reconciliation: PR #{} in {} carries {} with NO verdict-sha marker — the verdict \
         is UNVERIFIABLE and would survive a force-push undetected (#6319)",
        pr.number,
        root.display(),
        pr.kind.label(),
    );
    match decide_anchor(pr) {
        AnchorAction::RequeueApproval => match requeue_unanchored_approval(gh_bin, root, pr) {
            Ok(()) => {
                stats.unanchored_approvals_requeued += 1;
                log::warn!(
                    "claim_reconciliation: re-queued PR #{} in {}: its loom:pr approval carried \
                     no verdict-sha marker, so it approves no known tree and is never anchored \
                     (#9258)",
                    pr.number,
                    root.display(),
                );
            }
            Err(e) => log::warn!(
                "claim_reconciliation: failed to re-queue PR #{}'s unmarked approval in {}: {e} \
                 — retried next tick",
                pr.number,
                root.display()
            ),
        },
        AnchorAction::Anchor { head_sha } if anchoring => {
            match anchor_verdict(gh_bin, root, pr, &head_sha) {
                Ok(()) => {
                    stats.anchored += 1;
                    log::info!(
                        "claim_reconciliation: anchored PR #{}'s {} verdict to {head_sha} in {} \
                         — it is now invalidatable by the ordinary staleness pass (#6319)",
                        pr.number,
                        pr.kind.label(),
                        root.display(),
                    );
                }
                Err(e) => log::warn!(
                    "claim_reconciliation: failed to anchor PR #{}'s {} verdict in {}: {e} — it \
                     stays UNVERIFIABLE until the next tick",
                    pr.number,
                    pr.kind.label(),
                    root.display()
                ),
            }
        }
        AnchorAction::Anchor { .. } | AnchorAction::Skip(_) => {}
    }
}

/// The comment posted when an unmarked approval is re-queued (#9258).
#[must_use]
pub(super) fn requeue_body(head_sha: &str) -> String {
    format!(
        "{UNANCHORED_APPROVAL_MARKER_PREFIX}{head_sha} -->\n\
         **Approval re-queued: it carried no verdict-sha marker, so re-review is required**\n\n\
         This PR carried `loom:pr`, but no trusted verdict-sha marker records which tree that \
         approval described. `post-verdict.sh` always writes one, so a markerless approval means the verdict mechanism was bypassed (for \
         example a `gh pr comment --body @-` that posted the literal `@-`).\n\n\
         An approval of no known tree is not merge-eligible, and it is deliberately **not** \
         anchored to the current head `{head_sha}`: that would mark a tree nobody reviewed as \
         approved. `loom:pr` was removed and the PR returned to `loom:review-requested`.\n\n\
         ---\n\
         *Automated by loom-daemon claim reconciliation (#9258)*"
    )
}

/// Re-queue one unmarked approval: disarm any armed auto-merge (#8900), swap
/// the verdict labels for `loom:review-requested`, then post the explanation.
///
/// Labels go before the comment, like the shell guard's `--clear` (#10601):
/// the comment claims a state change, so it is posted only once the change
/// holds. A failed label write posts nothing and the next tick retries; once
/// it succeeds the PR no longer carries `loom:pr`, so it is never listed again
/// and the comment is never repeated.
fn requeue_unanchored_approval(gh_bin: &Path, root: &Path, pr: &VerdictPr) -> Result<()> {
    let disarmed = super::auto_merge_disarm::disarm_before_invalidation(gh_bin, root, pr.number);
    let n = pr.number.to_string();
    let out = gh_call::output(
        gh_call::write("verdict.requeue_unanchored", gh_bin, root)
            .args(["pr", "edit", &n])
            .args(["--remove-label", VerdictKind::Approved.label()])
            .args(["--remove-label", VerdictKind::ChangesRequested.label()])
            .args(["--add-label", "loom:review-requested"])
            .args(gh_call::loom_repo_flag()),
    )?;
    if !out.status.success() {
        let (root, err) = (root.display(), gh_call::stderr(&out));
        return Err(anyhow!(
            "gh pr edit (re-queue unmarked approval) failed for #{n} in {root}: {err}"
        ));
    }
    let head = pr.head_sha.as_deref().unwrap_or("unknown");
    let body = requeue_body(head) + &disarmed.map(|l| format!("\n{l}")).unwrap_or_default();
    let out = gh_call::output(
        gh_call::write("verdict.requeue_unanchored_comment", gh_bin, root)
            .args(["pr", "comment", &n, "--body", &body])
            .args(gh_call::loom_repo_flag()),
    )?;
    if !out.status.success() {
        // The label state is the source of truth and it already moved.
        log::warn!(
            "claim_reconciliation: re-queued PR #{n} in {}, but its explanation comment failed: {}",
            root.display(),
            gh_call::stderr(&out)
        );
    }
    Ok(())
}

/// Anchor one unmarked `loom:changes-requested` verdict to the PR's current
/// head (Issue #6319): post a comment carrying the `<!-- loom:verdict-sha ...
/// -->` marker judge.md was supposed to write, so the verdict becomes
/// invalidatable by the ordinary staleness path from here on.
///
/// **No label is touched.** The verdict label was already there and stays
/// exactly as it was, so anchoring cannot reject or un-park anything. Since
/// #9258 it never runs for an approval ([`super::decide_anchor`]).
///
/// Idempotent by construction — the marker it posts is precisely what
/// [`super::extract_latest_verdict_sha`] scans for, so the next pass reads the
/// verdict as `Fresh` and never anchors it twice.
fn anchor_verdict(gh_bin: &Path, root: &Path, pr: &VerdictPr, head_sha: &str) -> Result<()> {
    let label = pr.kind.label();
    let token = pr.kind.marker_token();
    let body = format!(
        "<!-- loom:verdict-sha sha={head_sha} verdict={token} -->\n\
         **Verdict anchored to the current head — no marker had been recorded**\n\n\
         This PR carries `{label}`, but no verdict-SHA marker was ever written for that \
         verdict, so it was **unverifiable**: nothing could tell whether it still described \
         the tree in front of it, and it would have survived a force-push undetected — the \
         exact pre-#5686 hazard.\n\n\
         This comment records the head SHA as of now, `{head_sha}`. It is **not** a review \
         and implies no judgment about this tree: the `{label}` label is unchanged. From \
         here on the verdict is invalidatable — if the head moves off `{head_sha}`, the \
         stale-verdict pass clears `{label}` and returns the PR to `loom:review-requested`.\n\n\
         Anchoring bounds future exposure; it cannot reconstruct which tree was actually \
         reviewed. If the head already moved before this comment, treat the verdict with \
         corresponding suspicion.\n\n\
         ---\n\
         *Automated by loom-daemon claim reconciliation (#6319)*"
    );

    let n = pr.number.to_string();
    let out = gh_call::output(
        gh_call::write("verdict.anchor_comment", gh_bin, root)
            .args(["pr", "comment", &n, "--body", &body])
            .args(gh_call::loom_repo_flag()),
    )?;
    if !out.status.success() {
        let (root, err) = (root.display(), gh_call::stderr(&out));
        return Err(anyhow!("gh pr comment (anchor {label}) failed for #{n} in {root}: {err}"));
    }
    Ok(())
}
