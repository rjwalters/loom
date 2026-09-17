//! Applying an un-escalation — the Rust port of
//! `classify-dependency-block.sh`'s `_apply_unescalation` (epic #7810, PR 3).
//!
//! # Write ordering is the safety property
//!
//! Two independent forge writes happen here: **remove the label**, then **post
//! the marker comment**. The order is load-bearing, and the shell carries a
//! long comment explaining why — reproduced as behaviour, not just as prose.
//!
//! [`super::unescalate`]'s idempotency guard keys on the presence of the marker
//! **comment**, not on the label state. So:
//!
//! - **Comment first, label removal fails** → every later re-scan finds the
//!   marker, short-circuits on `already-unescalated`, and never retries the
//!   removal. The proposal sits at `loom:operator-only` forever, carrying a
//!   comment claiming it was released. That is exactly the permanence failure
//!   #5664 exists to eliminate.
//! - **Label first, comment fails** → the state change that matters already
//!   landed; the next re-scan stops at `not-operator-only`. Only the audit
//!   trail and the anti-refight marker are missing, which is the soft
//!   direction.
//!
//! So the label goes first, and a failed label removal aborts **before** any
//! comment is posted.
//!
//! The sub-kind label removal between them is best-effort: a sub-label must
//! never outlive the base label it accompanies (#5671), but a pre-#5679
//! escalation never carried one, so "already absent" is the common case rather
//! than an error.

use crate::cmd_out::CmdOutcome;

/// Why an apply did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    /// The issue body could not be revised (fact path only). Nothing else was
    /// attempted, so a later pass finds the body unrevised and retries the
    /// whole sequence from the top.
    BodyEdit(String),
    /// The base label could not be removed. **No comment was posted**, so a
    /// later pass still sees the label, finds no marker, and retries.
    LabelRemoval(String),
    /// The label came off but the marker comment did not post. The state change
    /// landed; only the audit trail is missing.
    CommentPost(String),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::BodyEdit(d) => {
                write!(f, "could not append the revision section ({d}); no label change, no comment, a later pass will retry")
            }
            ApplyError::LabelRemoval(d) => {
                write!(f, "could not remove the operator-only label ({d}); no comment posted, a later pass will retry")
            }
            ApplyError::CommentPost(d) => {
                write!(f, "label removed but the marker comment did not post ({d})")
            }
        }
    }
}

/// The forge writes an apply needs, injected so ordering is testable without a
/// forge — which the shell could only reach through a `gh` stub on `PATH`.
pub trait Writer {
    /// Remove a label. Returns the outcome so the caller can distinguish
    /// "refused" from "could not ask".
    fn remove_label(&mut self, label: &str) -> CmdOutcome;
    /// Post a comment.
    fn post_comment(&mut self, body: &str) -> CmdOutcome;
    /// Replace the issue body. Only the fact path writes one.
    fn edit_body(&mut self, body: &str) -> CmdOutcome;
}

/// Labels this operation manipulates.
pub struct Labels<'a> {
    pub operator_only: &'a str,
    /// The #5671 sub-kind label, removed best-effort.
    pub operator_blocked: &'a str,
}

/// Apply an un-escalation: remove the label, then post the marker comment.
///
/// # Errors
///
/// [`ApplyError::LabelRemoval`] if the base label could not be removed — in
/// which case **nothing else is attempted**. [`ApplyError::CommentPost`] if the
/// label came off but the comment did not post.
pub fn apply_unescalation<W: Writer>(
    writer: &mut W,
    labels: &Labels<'_>,
    body: &str,
) -> Result<(), ApplyError> {
    // 1. The state change that matters. If this fails, stop — posting the
    //    marker now would permanently suppress every future retry.
    let removed = writer.remove_label(labels.operator_only);
    if !removed.succeeded() {
        return Err(ApplyError::LabelRemoval(
            removed.failure_reason("gh issue edit --remove-label"),
        ));
    }

    // 2. Best-effort: the sub-kind label must not outlive its base label
    //    (#5671), but "already absent" is the common case, not an error.
    let _ = writer.remove_label(labels.operator_blocked);

    // 3. The audit trail and the anti-refight marker.
    let posted = writer.post_comment(body);
    if !posted.succeeded() {
        return Err(ApplyError::CommentPost(posted.failure_reason("gh issue comment")));
    }

    Ok(())
}

/// The marker comment body for a blockers-cleared release.
#[must_use]
pub fn cleared_body(
    operator_only: &str,
    operator_blocked: &str,
    cleared: &str,
    marker: &str,
) -> String {
    format!(
        "**Champion: Un-escalating — the recorded blocker has closed**\n\
         \n\
         This proposal was routed to `{operator_only}` for a **timing** finding, not a\n\
         merits one: its only recurring finding was an open dependency. That dependency has\n\
         since closed, so the stated reason for the escalation no longer holds.\n\
         \n\
         **Cleared blockers**: {cleared}\n\
         \n\
         Removed `{operator_only}` (and its `{operator_blocked}` sub-kind\n\
         label, if present); this proposal returns to normal evaluation. Nothing\n\
         here overrides a human decision — if this proposal genuinely needs one, re-add\n\
         the label (or state the merits finding) and it will not be un-escalated again.\n\
         \n\
         ---\n\
         *Automated by Champion role (classify-dependency-block.sh, #5664)*\n\
         {marker}\n"
    )
}

/// The marker comment body for a startable-subset carve-out.
#[must_use]
pub fn subset_body(
    operator_only: &str,
    operator_blocked: &str,
    still_open: &str,
    subset: &str,
    marker: &str,
) -> String {
    format!(
        "**Champion: Un-escalating — a startable subset was never actually blocked**\n\
         \n\
         This proposal was routed to `{operator_only}` for a **timing** finding, not a\n\
         merits one: its only recurring finding was an open dependency. That dependency\n\
         ({still_open}) is still open, but this issue declares a **startable subset**\n\
         independent of it:\n\
         \n\
         {subset}\n\
         \n\
         Parking the whole issue on a blocker that only covers part of its work was the\n\
         mistake, not the wait itself — un-escalating so the subset above can be\n\
         promoted and built now.\n\
         \n\
         Removed `{operator_only}` (and its `{operator_blocked}` sub-kind\n\
         label, if present); this proposal returns to normal evaluation, scoped to the\n\
         startable subset until {still_open} closes. Nothing here overrides a human\n\
         decision — if this proposal genuinely needs one, re-add the label (or state\n\
         the merits finding) and it will not be un-escalated again.\n\
         \n\
         ---\n\
         *Automated by Champion role (classify-dependency-block.sh, #5664)*\n\
         {marker}\n"
    )
}

// ===========================================================================
// The fact path (#7650)
// ===========================================================================

/// Labels the **fact** un-escalation manipulates.
///
/// Deliberately not [`Labels`]: the two mechanisms drop a *different* sub-kind.
/// A dependency-timing escalation carries `loom:operator-blocked`; a
/// fact-checkable one carries `loom:operator-decision`, which is what
/// `champion-issue-promo.md` selects whenever a recurring finding is not a pure
/// dependency citation. Sharing one struct would invite passing the wrong one,
/// which fails silently — removing an absent label is best-effort and succeeds.
pub struct FactLabels<'a> {
    pub operator_only: &'a str,
    /// The #7650 sub-kind label, removed best-effort.
    pub operator_decision: &'a str,
}

/// Fold a resolutions file into prose bullets.
///
/// Mirrors `sed -E 's/^RESOLVED:[[:space:]]*/- /'`: the tag is stripped only at
/// line start, and every other line passes through untouched. By the time this
/// runs every line is `RESOLVED:` — a partial set never reaches the apply path.
#[must_use]
pub fn resolutions_summary(resolutions: &str) -> String {
    resolutions
        .lines()
        .map(|line| match line.strip_prefix("RESOLVED:") {
            Some(rest) => format!("- {}", rest.trim_start_matches([' ', '\t'])),
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The issue body with a `## Revision` section appended.
///
/// Appending changes the body hash, which is this repo's existing contract for
/// "revised — evaluate again". The returned string has **no** trailing newline,
/// matching the shell's `${new_body%$'\n'}`.
#[must_use]
pub fn fact_revised_body(
    body: &str,
    today: &str,
    commit: &str,
    summary: &str,
    revision_marker: &str,
) -> String {
    format!(
        "{body}\n\n## Revision ({today})\n\nCurator re-verified every objection Champion's escalation cited against\n`{commit}` and found all of them resolved:\n\n{summary}\n\n{revision_marker}"
    )
}

/// The confirming comment for a fact un-escalation. No trailing newline,
/// matching the shell.
#[must_use]
pub fn fact_comment_body(
    operator_only: &str,
    operator_decision: &str,
    commit: &str,
    summary: &str,
    marker: &str,
) -> String {
    format!(
        "**Curator: De-escalating — every cited objection has resolved on `main`**\n\nChampion escalated this proposal for repeated rejection without revision.\nEvery objection Champion's escalation cited has since been independently\nre-verified against `{commit}`:\n\n{summary}\n\nAppended a `## Revision` section naming the verifying commit (this changes\nthe body hash, the existing contract for \"revised — evaluate again\") and\nremoved `{operator_only}` (and its `{operator_decision}`\nsub-kind label, if present). This proposal returns to Champion's normal\nevaluation queue. Nothing here overrides a human decision — if this proposal\ngenuinely needs one, re-add the label and it will not be de-escalated again\nfor the same finding set.\n\n---\n*Automated by Curator role (classify-dependency-block.sh --check-fact-unescalate, #7650)*\n{marker}"
    )
}

/// Apply a fact un-escalation: revise the body, remove the label, post the
/// comment.
///
/// # Write order
///
/// The same rule as [`apply_unescalation`], with one extra write in front:
///
/// - **body edit fails** → no label change and no comment; a later re-scan
///   finds the body unrevised and retries from the top.
/// - **label removal fails** → the body already carries the `## Revision`
///   section, but the label is still on. A later re-scan re-reads the
///   already-revised body and computes the *same* fingerprint — it is keyed to
///   the escalation comment and the commit, not the body — so the label and
///   comment steps retry. The `revision_marker` guard below is what keeps that
///   retry from appending the section a second time.
/// - **comment post fails** → both state changes landed; only the audit trail
///   is missing.
///
/// # Errors
///
/// [`ApplyError::BodyEdit`], [`ApplyError::LabelRemoval`] or
/// [`ApplyError::CommentPost`], per the ordering above.
pub fn apply_fact_unescalation<W: Writer>(
    writer: &mut W,
    labels: &FactLabels<'_>,
    current_body: &str,
    revised_body: &str,
    revision_marker: &str,
    comment: &str,
) -> Result<(), ApplyError> {
    // Idempotent: a retry after a failed label removal must not append the
    // `## Revision` section again.
    if !current_body.contains(revision_marker) {
        let edited = writer.edit_body(revised_body);
        if !edited.succeeded() {
            return Err(ApplyError::BodyEdit(edited.failure_reason("gh issue edit --body")));
        }
    }

    let removed = writer.remove_label(labels.operator_only);
    if !removed.succeeded() {
        return Err(ApplyError::LabelRemoval(
            removed.failure_reason("gh issue edit --remove-label"),
        ));
    }

    let _ = writer.remove_label(labels.operator_decision);

    let posted = writer.post_comment(comment);
    if !posted.succeeded() {
        return Err(ApplyError::CommentPost(posted.failure_reason("gh issue comment")));
    }

    Ok(())
}

#[cfg(test)]
mod tests;
