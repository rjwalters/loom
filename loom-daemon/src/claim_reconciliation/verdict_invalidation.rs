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

use anyhow::{anyhow, Result};
use std::path::Path;

use super::{gh_call, VerdictKind, VerdictPr, VerdictReconcileStats};
use crate::verdict_equivalence::{self, Equivalence, EquivalenceKind};
use crate::verdict_stale_notice::UntrustedVerdictMarker;

// The carve-out's kill switch (`LOOM_VERDICT_TREE_CARVEOUT`, nested here inside
// [`super::VERDICT_STALENESS_ENABLED_ENV`]) moved to the shared module with the
// comparison, so the shell guard's `forge tree-unchanged` call honours the same
// switch this pass does (PR #9581 review). Re-exported under the old path.
pub(super) use crate::forge_tree_unchanged::verdict_tree_carveout_enabled;
#[cfg(test)]
pub(super) use crate::forge_tree_unchanged::VERDICT_TREE_CARVEOUT_ENABLED_ENV;

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
    // #10134: when no equivalence could even be EVALUATED (an unfetchable
    // commit, a `gh` outage, a shallow clone), the clear below still happens —
    // fail closed — but it says so, in the log and in the PR comment, instead
    // of reading exactly like a real content change.
    let mut unavailable_note = None;
    if tree_carveout {
        let assessment =
            verdict_equivalence::assess(gh_bin, Some(root), pr.number, marker_sha, head_sha);
        unavailable_note = assessment.fail_closed_note();
        if let Some(note) = &unavailable_note {
            log::warn!(
                "claim_reconciliation: PR #{} in {}: could not determine whether the head move \
                 {marker_sha} -> {head_sha} kept the reviewed change — failing closed and \
                 clearing {} (#10134). Why: {note}",
                pr.number,
                root.display(),
                pr.kind.label(),
            );
        }
        if let Equivalence::Equivalent(kind) = assessment.equivalence {
            match reanchor_equivalent_verdict(gh_bin, root, pr, marker_sha, head_sha, kind) {
                Ok(()) => {
                    stats.tree_identical_reanchors += 1;
                    log::info!(
                        "claim_reconciliation: PR #{} in {} carries {} with head moved from \
                         {marker_sha} to {head_sha}, but the reviewed change is unchanged \
                         (equivalence kind: {}) — re-anchored instead of invalidating (#9124, \
                         #9416)",
                        pr.number,
                        root.display(),
                        pr.kind.label(),
                        kind.token(),
                    );
                }
                Err(e) => {
                    log::warn!(
                        "claim_reconciliation: failed to re-anchor PR #{}'s {}-equivalent {} \
                         verdict in {}: {e} — it stays stale (recorded for {marker_sha}) until \
                         the next tick re-evaluates it",
                        pr.number,
                        kind.token(),
                        pr.kind.label(),
                        root.display()
                    );
                }
            }
            return;
        }
    }
    let note = unavailable_note.as_deref();
    match invalidate_verdict(gh_bin, root, pr, marker_sha, head_sha, note) {
        Ok((comment_skipped, untrusted)) => {
            stats.invalidated += 1;
            stats.redundant_comments_skipped += usize::from(comment_skipped);
            log::warn!(
                "claim_reconciliation: cleared stale {} from PR #{} in {} (verdict recorded for \
                 {marker_sha}, head is now {head_sha}) — re-queued as loom:review-requested \
                 (#5686){}",
                pr.kind.label(),
                pr.number,
                root.display(),
                crate::verdict_stale_notice::log_note(untrusted.as_ref()),
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

/// Re-anchor a verdict whose head moved but whose reviewed change did not
/// (Issues #9124, #9416): post an updated `<!-- loom:verdict-sha ... -->`
/// marker for `head_sha`, plus a `<!-- loom:verdict-equivalence kind=… -->`
/// marker recording WHICH equivalence carried it, and leave the verdict label
/// exactly as it is.
///
/// **No label is touched, and nothing is disarmed.** Unlike
/// [`invalidate_verdict`], this path runs only once
/// [`crate::verdict_equivalence::detect`] has proven — from git objects and the
/// forge's own compare endpoint, never from a marker — that the change at
/// `head_sha` is the change that was reviewed at `marker_sha`. So there is
/// nothing to re-review, and nothing unsafe about an auto-merge that was
/// already armed going on to merge it. CI still re-runs against `head_sha`
/// either way: this path exempts the review, never a check.
///
/// Idempotent by construction, same as [`super::forge::anchor_verdict`]: the
/// `verdict-sha` marker it posts is exactly what
/// [`super::extract_latest_verdict_sha`] scans for, so the next pass reads the
/// verdict as `Fresh`. The body itself lives in
/// [`crate::verdict_equivalence::reanchor_body`] — with it goes the one rule
/// that matters for that scan: nothing may be added INSIDE the `verdict-sha`
/// marker, because both scanners anchor on its trailing ` -->`.
fn reanchor_equivalent_verdict(
    gh_bin: &Path,
    root: &Path,
    pr: &VerdictPr,
    marker_sha: &str,
    head_sha: &str,
    kind: EquivalenceKind,
) -> Result<()> {
    let label = pr.kind.label();
    let body = verdict_equivalence::reanchor_body(
        label,
        pr.kind.marker_token(),
        kind,
        marker_sha,
        head_sha,
    );

    // #9772: the footer's link needs the slug — `LOOM_REPO` when set, else
    // the root's origin remote. An unresolvable slug posts unlinked rather
    // than linking to nowhere.
    let nwo = std::env::var("LOOM_REPO").ok().or_else(|| {
        crate::worktree_ops::gh::resolve_owner_repo(root).map(|(o, r)| format!("{o}/{r}"))
    });
    let body = crate::forge_comment::footer_or_body(nwo.as_deref(), pr.number, true, &body);
    let n = pr.number.to_string();
    let out = gh_call::output(
        gh_call::write("verdict.reanchor_comment", gh_bin, root)
            .args(["pr", "comment", &n, "--body", &body])
            .args(gh_call::loom_repo_flag()),
    )?;
    if !out.status.success() {
        let (root, err) = (root.display(), gh_call::stderr(&out));
        return Err(anyhow!("gh pr comment (reanchor {label}) failed for #{n} in {root}: {err}"));
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
/// Returns `true` when the comment was skipped as redundant, plus the dropped
/// untrusted marker the posted notice named (#9709; always `None` when skipped).
fn invalidate_verdict(
    gh_bin: &Path,
    root: &Path,
    pr: &VerdictPr,
    marker_sha: &str,
    head_sha: &str,
    unavailable_note: Option<&str>,
) -> Result<(bool, Option<UntrustedVerdictMarker>)> {
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
        .unwrap_or_default()
        + &super::verdict_stale_comment::unavailable_line(unavailable_note);
    let skipped =
        !super::verdict_stale_comment::should_post(pr.invalidation_recorded, disarmed.is_some());
    let untrusted = if skipped {
        log::info!(
            "claim_reconciliation: PR #{} in {} already records the {marker_sha} -> {head_sha} \
             invalidation — not re-posting the notice, only re-applying the labels (#9124)",
            pr.number,
            root.display(),
        );
        None
    } else {
        // #9709: was a NEWER marker for this verdict dropped as untrusted?
        // Then the notice (and the caller's log line) must say so rather
        // than assert a plain head move. Probed only when the notice is
        // actually posted, so a dedup-skipped pass costs no extra read.
        let untrusted = probe_untrusted_marker(root, pr);
        let body = super::verdict_stale_comment::attributed_body(
            label,
            marker_sha,
            head_sha,
            &disarm_line,
            untrusted.as_ref(),
        );
        let n = pr.number.to_string();
        let out = gh_call::output(
            gh_call::write("verdict.stale_comment", gh_bin, root)
                .args(["pr", "comment", &n, "--body", &body])
                .args(gh_call::loom_repo_flag()),
        )?;
        if !out.status.success() {
            let (root, err) = (root.display(), gh_call::stderr(&out));
            return Err(anyhow!("gh pr comment failed for #{n} in {root}: {err}"));
        }
        untrusted
    };

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
    let n = pr.number.to_string();
    let out = gh_call::output(
        gh_call::write("verdict.clear_labels", gh_bin, root)
            .args(["pr", "edit", &n])
            .args(["--remove-label", VerdictKind::Approved.label()])
            .args(["--remove-label", VerdictKind::ChangesRequested.label()])
            .args(["--remove-label", "loom:ci-failure"])
            .args(["--remove-label", "loom:merge-conflict"])
            .args(["--add-label", "loom:review-requested"])
            .args(gh_call::loom_repo_flag()),
    )?;
    if !out.status.success() {
        let (root, err) = (root.display(), gh_call::stderr(&out));
        return Err(anyhow!("gh pr edit (clear {label}) failed for #{n} in {root}: {err}"));
    }
    Ok((skipped, untrusted))
}

/// The newer-than-trusted verdict marker the trust filter dropped on this PR,
/// if any (#9709) — attribution for the notice, never evidence: the decision
/// above was already made from trusted markers only, and nothing here can
/// change it. One extra comment read, on the (rare) posting path only.
/// `None` on any read failure, which falls back to the plain head-moved wording.
///
/// Built through the [`crate::gh_invocation`] choke point (#9985), so the
/// executable comes from its resolver (`LOOM_GH_BIN` → `PATH`), not the
/// caller's `gh_bin`.
fn probe_untrusted_marker(root: &Path, pr: &VerdictPr) -> Option<UntrustedVerdictMarker> {
    use crate::cmd_out::CmdOutcome;
    use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
    let path = format!("repos/{{owner}}/{{repo}}/issues/{}/comments", pr.number);
    let outcome = GhInvocation::new(
        Operation::new("api.rest"),
        AccessIntent::Read,
        GhTarget::None,
        std::time::Duration::from_secs(60),
    )
    .forge_op(crate::forge_call_stats::ops::COMMENT_LIST)
    .args(["api", path.as_str(), "--paginate"])
    .current_dir(root)
    .run();
    let CmdOutcome::Ran(out) = outcome else {
        return None;
    };
    if !out.status.success() {
        return None;
    }
    let items = crate::comment_trust::parse_listing(&out.stdout)?;
    let policy = crate::comment_trust::TrustPolicy::for_root(root);
    crate::verdict_stale_notice::untrusted_newer_marker(&policy, &items, pr.kind)
}
