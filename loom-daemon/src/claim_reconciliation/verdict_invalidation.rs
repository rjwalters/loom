//! Everything [`super::forge::reconcile_pr_verdicts`] does once
//! [`super::decide_verdict`] has answered `Invalidate` — the tree-identical
//! re-anchor carve-out (Issue #9124) first, then the ordinary stale-verdict
//! clear (#5686) it falls through to.
//!
//! # Why a sibling file
//!
//! Per the file-size ratchet (`.loom/docs/file-size-policy.md`,
//! `scripts/file-size-baseline.txt`), `claim_reconciliation.rs` is frozen at
//! its current size: it may shrink, not grow. The #9124 carve-out is new
//! production logic, so it goes in a new module with a thin dispatch left
//! behind — the same treatment [`super::pr_label_info`],
//! [`super::auto_merge_disarm`] and [`super::verdict_stale_comment`] already
//! got. `invalidate_verdict` moves here with it rather than staying behind in
//! [`super::forge`], because the carve-out and the clear are the two branches
//! of one decision and splitting them across files would make the arm harder
//! to read, not easier.
//!
//! **This is a relocation, not a behavior change.** The decision logic, the
//! comment bodies, the label writes, the counters and the log lines are
//! byte-for-byte what they were; only their module path moved.
//!
//! # What the carve-out is
//!
//! [`super::decide_verdict`] is a pure function and stays that way: any head
//! move off the marker SHA is `Invalidate`, no exceptions, no guessing from
//! event shape (the heuristic #5686 rejected). The #9124 carve-out sits
//! strictly *downstream* of that answer and is gated on live evidence —
//! GitHub's own `compare/{base}...{head}` reporting `files: []`, which proves
//! the two trees are bit-for-bit identical. Measured on `rjwalters/loom`, that
//! is the majority of observed transitions, and every one of them is the
//! `#8248` required-check-freshness guard's automated re-date commit (#8508),
//! whose own message says it changes nothing in the tree. See
//! `verdict_dedup_tests.rs`'s module doc for the measurement.
//!
//! **The comparison itself no longer lives here** (#9576). It moved to
//! [`crate::forge_tree_unchanged`], which this module calls in-process and
//! `defaults/scripts/verdict-staleness-guard.sh` reaches through the
//! `loom-daemon forge tree-unchanged` verb. The agent-side guard had no tree
//! comparison at all while this one did, so the two paths disagreed and PRs
//! #9541/#9483 lost verdicts here that the daemon pass would have kept. One
//! implementation, two callers.

use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::process::{Command, Stdio};

use super::{VerdictKind, VerdictPr, VerdictReconcileStats};
use crate::forge_tree_unchanged::tree_unchanged;

/// Env kill switch for the tree-identical re-anchor carve-out (Issue #9124),
/// nested inside [`super::VERDICT_STALENESS_ENABLED_ENV`]. Defaults to ON for
/// the same shape of argument as [`super::VERDICT_ANCHOR_ENABLED_ENV`]: it can
/// only ever *reduce* exposure relative to the pre-#9124 behavior, because it
/// fires only once GitHub's own compare API has proven the two trees are
/// byte-identical, and it fails open into the ordinary invalidation whenever
/// that proof is unavailable (a `gh api compare` failure, a malformed
/// response, an abbreviated marker SHA the compare endpoint rejects) — see
/// [`tree_unchanged`]. `0`/`false`/`no`/`off` disables it, restoring the
/// pre-#9124 behavior of invalidating on every SHA move regardless of tree
/// content.
pub(super) const VERDICT_TREE_CARVEOUT_ENABLED_ENV: &str = "LOOM_VERDICT_TREE_CARVEOUT";

/// Is the tree-identical re-anchor carve-out enabled? See
/// [`VERDICT_TREE_CARVEOUT_ENABLED_ENV`].
#[must_use]
pub(super) fn verdict_tree_carveout_enabled() -> bool {
    match std::env::var(VERDICT_TREE_CARVEOUT_ENABLED_ENV) {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
        Err(_) => true,
    }
}

/// Carry out the `Invalidate` action for one PR: the #9124 carve-out if the
/// trees turn out to be identical, otherwise the ordinary #5686 clear.
///
/// `tree_carveout` is read once per pass by the caller (not once per PR), so a
/// mid-pass env flip cannot make one root behave two ways.
///
/// Counters are folded into `stats` here rather than returned, so the arm in
/// [`super::forge::reconcile_pr_verdicts`] stays a single call.
pub(super) fn handle_invalidate(
    gh_bin: &Path,
    root: &Path,
    pr: &VerdictPr,
    marker_sha: &str,
    head_sha: &str,
    tree_carveout: bool,
    stats: &mut VerdictReconcileStats,
) {
    // #9124: before clearing the verdict, ask GitHub whether the two trees
    // actually differ at all. A `Some(true)` here means the SHA move carries
    // no content change (measured cause on this repo: the #8248 guard's
    // re-date commit, #8508) — invalidating would buy nothing but a wasted
    // re-review, so this pass re-anchors instead. `Some(false)` or `None`
    // (comparison unavailable) fall straight through to the ordinary clear,
    // unchanged from before #9124.
    if tree_carveout {
        if let Some(true) = tree_unchanged(gh_bin, Some(root), marker_sha, head_sha) {
            match reanchor_tree_unchanged_verdict(gh_bin, root, pr, marker_sha, head_sha) {
                Ok(()) => {
                    stats.tree_identical_reanchors += 1;
                    log::info!(
                        "claim_reconciliation: PR #{} in {} carries {} with head moved from \
                         {marker_sha} to {head_sha}, but the trees are identical — re-anchored \
                         instead of invalidating (#9124)",
                        pr.number,
                        root.display(),
                        pr.kind.label(),
                    );
                }
                Err(e) => {
                    log::warn!(
                        "claim_reconciliation: failed to re-anchor PR #{}'s tree-identical {} \
                         verdict in {}: {e} — it stays stale (recorded for {marker_sha}) until \
                         the next tick re-evaluates it",
                        pr.number,
                        pr.kind.label(),
                        root.display()
                    );
                }
            }
            return;
        }
    }
    match invalidate_verdict(gh_bin, root, pr, marker_sha, head_sha) {
        Ok(comment_skipped) => {
            stats.invalidated += 1;
            stats.redundant_comments_skipped += usize::from(comment_skipped);
            log::warn!(
                "claim_reconciliation: cleared stale {} from PR #{} in {} (verdict recorded for \
                 {marker_sha}, head is now {head_sha}) — re-queued as loom:review-requested \
                 (#5686)",
                pr.kind.label(),
                pr.number,
                root.display(),
            );
        }
        Err(e) => {
            log::warn!(
                "claim_reconciliation: failed to clear stale {} from PR #{} in {}: {e}",
                pr.kind.label(),
                pr.number,
                root.display()
            );
        }
    }
}

/// Re-anchor a verdict whose head moved but whose tree did not (Issue #9124):
/// post an updated `<!-- loom:verdict-sha ... -->` marker for `head_sha` and
/// leave the verdict label exactly as it is.
///
/// **No label is touched, and nothing is disarmed.** Unlike
/// [`invalidate_verdict`], this path runs only once GitHub's own compare API
/// has confirmed `marker_sha` and `head_sha` share the same tree — the
/// reviewed code is still exactly what is at `head_sha` — so there is nothing
/// to re-review and nothing unsafe about an auto-merge that was already armed
/// going on to merge it.
///
/// Idempotent by construction, same as [`super::forge::anchor_verdict`]: the
/// marker it posts is exactly what [`super::extract_latest_verdict_sha`] scans
/// for, so the next pass reads the verdict as `Fresh`.
fn reanchor_tree_unchanged_verdict(
    gh_bin: &Path,
    root: &Path,
    pr: &VerdictPr,
    marker_sha: &str,
    head_sha: &str,
) -> Result<()> {
    let label = pr.kind.label();
    let token = pr.kind.marker_token();
    let body = format!(
        "<!-- loom:verdict-sha sha={head_sha} verdict={token} -->\n\
         **Verdict re-anchored — head moved, but the tree did not**\n\n\
         This PR's `{label}` verdict was recorded for `{marker_sha}`. The head is now \
         `{head_sha}`, but comparing the two shows **zero file differences** — the code this \
         verdict describes is unchanged; only the commit identity moved (commonly the \
         `#8248` required-check-freshness guard's automated re-date commit, #8508, which \
         exists only to refresh a merge queue's check timestamps).\n\n\
         Since the tree is provably identical, clearing `{label}` and sending this PR back \
         through Judge would buy nothing but another full review of content already \
         reviewed — exactly the waste #9124 measured. The marker is updated to `{head_sha}` \
         so a future GENUINE change is still caught by the ordinary staleness check.\n\n\
         ---\n\
         *Automated by loom-daemon claim reconciliation (#9124)*"
    );

    let mut cmd = Command::new(gh_bin);
    cmd.arg("pr")
        .arg("comment")
        .arg(pr.number.to_string())
        .arg("--body")
        .arg(&body);
    cmd.current_dir(root);
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
    if let Ok(repo) = std::env::var("LOOM_REPO") {
        cmd.arg("--repo").arg(repo);
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    let out = cmd
        .output()
        .with_context(|| format!("failed to invoke {}", gh_bin.display()))?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh pr comment (reanchor {label}) failed for #{} in {}: {}",
            pr.number,
            root.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// Clear one stale verdict: post the auditable old->new SHA comment, then swap
/// the verdict label (plus its per-tree companions) for
/// `loom:review-requested`.
///
/// The comment goes FIRST, deliberately: if the label write then fails, the PR
/// keeps a verdict that is at least explained, rather than getting silently
/// re-queued with no record of why. A failed comment aborts before touching
/// any label, so the transition is never applied without its audit trail.
///
/// Ahead of even the comment, any armed GitHub auto-merge is **disarmed**
/// (issue #8900) — see [`super::auto_merge_disarm`] for why the disarm is
/// first and why it is safe there. Without it the label flip below is
/// cosmetic: the queued server-side merge is gated only by the ruleset's
/// required checks and merges the unreviewed head anyway (#8694, #8847, #8843
/// all merged that way on 2026-09-25).
///
/// # The comment is idempotent (Issue #9124)
///
/// "Comment first" is not "comment again every tick". When the PR already
/// carries this transition's notice ([`VerdictPr::invalidation_recorded`]) and
/// this pass disarmed nothing, the comment is skipped and only the label swap
/// is attempted. That is the whole fix for the measured duplicate-notice loop:
/// 26 of 106 invalidation comments in a 150-PR sample restated a transition
/// already recorded, because a host whose label write keeps failing re-posts
/// on every pass. The label write is deliberately still attempted — retrying
/// it is the *point*.
///
/// Returns `true` when the comment was skipped as redundant.
fn invalidate_verdict(
    gh_bin: &Path,
    root: &Path,
    pr: &VerdictPr,
    marker_sha: &str,
    head_sha: &str,
) -> Result<bool> {
    let label = pr.kind.label();
    // `None` (the common case: nothing was armed) contributes no line at
    // all, so the comment never claims a disarm that did not happen.
    //
    // This runs unconditionally, BEFORE the #9124 dedup decision: an
    // armed auto-merge on a stale verdict is the #8900 hazard whether or
    // not another host already announced the staleness, and a disarm that
    // did happen is exactly what must not go unrecorded.
    let disarmed = super::auto_merge_disarm::disarm_before_invalidation(gh_bin, root, pr.number);
    let disarm_line = disarmed
        .as_ref()
        .map(|line| format!("\n{line}"))
        .unwrap_or_default();
    let skipped =
        !super::verdict_stale_comment::should_post(pr.invalidation_recorded, disarmed.is_some());
    if skipped {
        log::info!(
            "claim_reconciliation: PR #{} in {} already records the {marker_sha} -> {head_sha} \
             invalidation — not re-posting the notice, only re-applying the labels (#9124)",
            pr.number,
            root.display(),
        );
    } else {
        let body = super::verdict_stale_comment::body(label, marker_sha, head_sha, &disarm_line);
        let mut cmd = Command::new(gh_bin);
        cmd.arg("pr")
            .arg("comment")
            .arg(pr.number.to_string())
            .arg("--body")
            .arg(&body);
        cmd.current_dir(root);
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
        if let Ok(repo) = std::env::var("LOOM_REPO") {
            cmd.arg("--repo").arg(repo);
        }
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());
        let out = cmd
            .output()
            .with_context(|| format!("failed to invoke {}", gh_bin.display()))?;
        if !out.status.success() {
            return Err(anyhow!(
                "gh pr comment failed for #{} in {}: {}",
                pr.number,
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }

    // `loom:ci-failure` / `loom:merge-conflict` are findings about the OLD
    // tree too — they ride along with the verdict they were applied
    // beside, so they go with it.
    //
    // Remove BOTH terminal verdict labels here (`loom:pr` AND
    // `loom:changes-requested`), not just `label` — this PR's OWN detected
    // kind (issue #7018). `list_verdict_prs` walks the two verdict kinds
    // independently, so a PR carrying both labels simultaneously (a
    // contradictory state, a manual label edit, or debris left by an
    // earlier partial write) is only ever reasoned about here under ONE
    // kind at a time; a single-label removal would leave the OTHER
    // verdict label standing beside the freshly re-added
    // `loom:review-requested` — the exact mutual-exclusion violation this
    // pass exists to prevent. `gh pr edit --remove-label` on a label that
    // isn't present is a documented no-op, so requesting removal of both
    // unconditionally is always safe.
    let mut cmd = Command::new(gh_bin);
    cmd.arg("pr")
        .arg("edit")
        .arg(pr.number.to_string())
        .arg("--remove-label")
        .arg(VerdictKind::Approved.label())
        .arg("--remove-label")
        .arg(VerdictKind::ChangesRequested.label())
        .arg("--remove-label")
        .arg("loom:ci-failure")
        .arg("--remove-label")
        .arg("loom:merge-conflict")
        .arg("--add-label")
        .arg("loom:review-requested");
    cmd.current_dir(root);
    crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
    if let Ok(repo) = std::env::var("LOOM_REPO") {
        cmd.arg("--repo").arg(repo);
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());
    let out = cmd
        .output()
        .with_context(|| format!("failed to invoke {}", gh_bin.display()))?;
    if !out.status.success() {
        return Err(anyhow!(
            "gh pr edit (clear {label}) failed for #{} in {}: {}",
            pr.number,
            root.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(skipped)
}
