//! The stale-verdict audit comment: its body, and the idempotence test that
//! stops the fleet re-posting it (Issue #9124).
//!
//! # Why this exists
//!
//! [`super::forge::invalidate_verdict`] posts its audit comment **before** it
//! swaps the labels, deliberately: if the label write then fails, the PR keeps
//! a verdict that is at least explained rather than getting silently
//! re-queued. That ordering is right, but it was not *idempotent* — and the
//! fleet is many daemons against one repo, so the failing-label-write case is
//! not hypothetical.
//!
//! Measured on `rjwalters/loom`, 150 most recent PRs, 2026-09-29: **106
//! `loom:verdict-stale` comments carried only 79 distinct `(from, to)`
//! transitions**. 26 of the 106 (25%) re-stated a transition the PR had
//! already recorded — one `(from, to)` pair on #9348 was written **five**
//! times in 26 minutes by three different identities. Every repeat came from
//! a host whose comment write succeeded and whose *label* write did not: one
//! token in the pool authored 38 invalidation comments across the sample and
//! **zero** label events on any of those PRs. It re-posted on every tick,
//! forever, because nothing told it the notice was already there.
//!
//! # What the dedup does and does not change
//!
//! It changes **one** thing: whether a second identical notice is written. It
//! does not touch [`super::decide_verdict`], so *which* verdicts are stale is
//! bit-for-bit what it was; and the label swap still runs on every pass, so a
//! host that failed its label write still retries it. The comment-first
//! safety property survives intact — the invariant is "an invalidated verdict
//! is explained on the PR", and a comment already present satisfies it exactly
//! as well as a duplicate would.
//!
//! One deliberate exception, see [`should_post`]: if this pass had to disarm
//! an armed auto-merge (#8900), that is a **new** action nobody has recorded
//! yet, so the comment goes out even when the transition is a repeat.
//!
//! # What it is NOT
//!
//! Not, on its own, a change to invalidation *rates* — this dedup only
//! reduces how many times a repeat invalidation is announced, not how many
//! invalidations happen. **That is a distinct question the same #9124
//! investigation answered separately, with a different result**: a real
//! avoidable category — a head move whose tree is byte-identical — turned out
//! to be more than half the sample, and IS carved out. See
//! `super::forge::tree_unchanged` and `verdict_dedup_tests.rs`'s module doc
//! for that measurement and fix; the two changes are complementary, not the
//! same one.

/// The machine-readable marker [`body`] stamps into every stale-verdict
/// comment, recording WHICH transition the notice covers.
///
/// ```text
/// <!-- loom:verdict-stale from=<marker-sha> to=<head-sha> -->
/// ```
///
/// Distinct from [`super::VERDICT_MARKER_PREFIX`] (`loom:verdict-sha`, which
/// records which tree a *verdict* describes). This one records which
/// invalidation has already been announced.
pub(super) const VERDICT_STALE_MARKER_PREFIX: &str = "<!-- loom:verdict-stale from=";

/// Has this exact `marker_sha -> head_sha` invalidation already been announced
/// on the PR?
///
/// Matches on the full marker line the way [`body`] writes it, so a comment
/// that merely *mentions* the SHAs in prose cannot suppress a real notice, and
/// a notice for a different transition on the same PR cannot either — a PR
/// that is invalidated at A->B, re-approved, and invalidated again at B->C
/// gets both notices.
///
/// `bodies` is the listing [`super::forge::fetch_comment_bodies`] already
/// fetched for the marker scan, so this test costs **no extra API call**.
pub(super) fn already_recorded(bodies: &[String], marker_sha: &str, head_sha: &str) -> bool {
    if marker_sha.is_empty() || head_sha.is_empty() {
        return false;
    }
    let needle = format!("{VERDICT_STALE_MARKER_PREFIX}{marker_sha} to={head_sha} -->");
    bodies.iter().any(|b| b.contains(&needle))
}

/// Should [`super::forge::invalidate_verdict`] write the audit comment?
///
/// `false` only when the PR already carries this transition's notice **and**
/// this pass performed no auto-merge disarm. The disarm carve-out is not
/// cosmetic: `disarm_before_invalidation` (#8900) is a state change to the
/// PR, and an unannounced one would leave "auto-merge was armed and got turned
/// off" nowhere in the record.
pub(super) fn should_post(already_recorded: bool, disarmed: bool) -> bool {
    !already_recorded || disarmed
}

/// The audit comment [`super::forge::invalidate_verdict`] posts.
///
/// `disarm_line` is pre-formatted (leading newline included) or empty — an
/// empty one contributes no line at all, so the comment never claims a disarm
/// that did not happen.
pub(super) fn body(label: &str, marker_sha: &str, head_sha: &str, disarm_line: &str) -> String {
    format!(
        "{VERDICT_STALE_MARKER_PREFIX}{marker_sha} to={head_sha} -->\n\
         **Stale review verdict cleared — head SHA moved**\n\n\
         This PR's `{label}` verdict was rendered against `{marker_sha}`, but the current \
         head is `{head_sha}`. A review verdict is a statement about a specific tree, so it \
         does not survive a rebase, a force-push, or new commits.\n\n\
         - Verdict cleared: `{label}` (recorded for `{marker_sha}`)\n\
         - Returned to the review queue: `loom:review-requested` (current head `{head_sha}`)\
         {disarm_line}\n\n\
         Judge will re-evaluate the tree that is actually here now. No judgment about the new \
         tree is implied either way — the old verdict simply no longer describes it.\n\n\
         ---\n\
         *Automated by loom-daemon claim reconciliation (#5686)*"
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const SHA_A: &str = "1111111111111111111111111111111111111111";
    const SHA_B: &str = "2222222222222222222222222222222222222222";
    const SHA_C: &str = "3333333333333333333333333333333333333333";

    fn notice(from: &str, to: &str) -> String {
        body("loom:pr", from, to, "")
    }

    #[test]
    fn a_notice_this_module_wrote_is_recognised_as_recorded() {
        // The round trip is the whole contract: if `body` and
        // `already_recorded` ever drift apart, the dedup silently stops
        // working and nothing fails.
        let bodies = vec![notice(SHA_A, SHA_B)];
        assert!(already_recorded(&bodies, SHA_A, SHA_B));
    }

    #[test]
    fn a_notice_for_a_different_transition_does_not_suppress_this_one() {
        // #9348's real shape: invalidated A->B, re-approved, then invalidated
        // B->C. The second notice must still go out.
        let bodies = vec![notice(SHA_A, SHA_B)];
        assert!(!already_recorded(&bodies, SHA_B, SHA_C));
        assert!(!already_recorded(&bodies, SHA_A, SHA_C));
        // ...and not in reverse, either: B->A is a distinct transition.
        assert!(!already_recorded(&bodies, SHA_B, SHA_A));
    }

    #[test]
    fn prose_mentioning_both_shas_does_not_count_as_a_recorded_notice() {
        // Only the marker line suppresses. A human (or a Judge) writing "this
        // went from 1111... to 2222..." must never silence the audit trail.
        let bodies = vec![format!("Rebased {SHA_A} onto main, now {SHA_B}.")];
        assert!(!already_recorded(&bodies, SHA_A, SHA_B));
    }

    #[test]
    fn a_verdict_sha_marker_is_not_a_stale_notice() {
        // The two markers share a prefix up to `loom:verdict-`; confusing them
        // would suppress the first notice on every marked PR.
        let bodies = vec![format!(
            "<!-- loom:verdict-sha sha={SHA_A} verdict=approved -->"
        )];
        assert!(!already_recorded(&bodies, SHA_A, SHA_B));
    }

    #[test]
    fn empty_shas_never_match() {
        // Belt and braces: `decide_verdict` already refuses to invalidate on an
        // empty SHA, but an empty needle here would match a truncated body and
        // suppress a real notice.
        let bodies = vec![notice(SHA_A, SHA_B)];
        assert!(!already_recorded(&bodies, "", SHA_B));
        assert!(!already_recorded(&bodies, SHA_A, ""));
        assert!(!already_recorded(&[], SHA_A, SHA_B));
    }

    #[test]
    fn the_notice_is_found_among_many_unrelated_comments() {
        // The real listing is dozens of comments deep and the notice is rarely
        // last (a Judge, Doctor, or Champion usually comments after it).
        let bodies = vec![
            "Builder: opened.".to_string(),
            notice(SHA_A, SHA_B),
            "Doctor: pushed a fix.".to_string(),
        ];
        assert!(already_recorded(&bodies, SHA_A, SHA_B));
    }

    #[test]
    fn should_post_skips_only_a_pure_repeat() {
        assert!(should_post(false, false), "first notice always goes out");
        assert!(!should_post(true, false), "pure repeat is suppressed");
    }

    #[test]
    fn a_disarm_is_announced_even_on_a_repeat() {
        // #8900: turning off an armed auto-merge is a state change this pass
        // made. Suppressing its record to save a duplicate comment would trade
        // a real audit trail for noise reduction — the wrong way round.
        assert!(should_post(true, true));
    }

    #[test]
    fn the_body_still_carries_every_field_the_audit_trail_needs() {
        let b = body("loom:changes-requested", SHA_A, SHA_B, "\n- Disarmed.");
        assert!(b.starts_with(VERDICT_STALE_MARKER_PREFIX));
        assert!(b.contains(&format!("from={SHA_A} to={SHA_B} -->")));
        assert!(b.contains("loom:changes-requested"));
        assert!(b.contains("loom:review-requested"));
        assert!(b.contains("- Disarmed."));
        assert!(b.contains("#5686"));
    }

    #[test]
    fn an_absent_disarm_contributes_no_line() {
        assert!(!body("loom:pr", SHA_A, SHA_B, "").contains("Disarmed"));
    }
}
