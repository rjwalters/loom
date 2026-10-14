//! Marker and label constants (epic #7810, PR 3).
//!
//! These strings are **persisted** — they live in issue comments and labels
//! written by past runs, on live repositories. Changing one does not just alter
//! future behaviour; it makes every existing marker unrecognisable, so a
//! proposal already released gets released again, and one already parked gets
//! re-parked. They are reproduced verbatim from
//! `classify-dependency-block.sh` and should be treated as a wire format.

/// Marks the comment Champion writes when it escalates a proposal.
pub const ESCALATE_MARKER: &str = "<!-- champion:proposal-escalated -->";

/// Prefix of the marker written when a dependency **cycle** was the reason.
///
/// Its presence is what makes an escalation ineligible for un-escalation:
/// waiting never resolves a cycle.
pub const CYCLE_MARKER_PREFIX: &str = "<!-- champion:dep-cycle:";

/// Prefix of the idempotency marker for a normal un-escalation.
pub const UNESCALATE_MARKER_PREFIX: &str = "<!-- champion:proposal-unescalated:";

/// Prefix of the idempotency marker for a **fact**-based un-escalation.
///
/// Deliberately distinct from [`UNESCALATE_MARKER_PREFIX`]: the two mechanisms
/// release on different evidence, and one must not suppress the other.
pub const FACT_UNESCALATE_MARKER_PREFIX: &str = "<!-- champion:proposal-unescalated-facts:";

/// Text identifying a Champion rejection comment, whose **last** occurrence is
/// the findings source for `--check-defer`.
pub const REJECT_NEEDLE: &str = "Champion Review: NEEDS REVISION";

/// The label that parks a proposal for an operator (#11795). Before the fold
/// it was `loom:operator-only` plus a sub-kind (`loom:operator-blocked` for a
/// dependency-timing escalation, `loom:operator-decision` for a fact-checkable
/// one); an unmigrated item may still carry those, which [`held_label`] and
/// [`other_hold_names`] read.
pub const OPERATOR_LABEL: &str = crate::work_finder::OPERATOR_HOLD_LABEL;

/// The operator hold `labels` carry: [`OPERATOR_LABEL`] when present, else the
/// first retired name for it present (an unmigrated escalation), else
/// [`OPERATOR_LABEL`] (absent, so the deciders answer `not-operator-only`).
#[must_use]
pub fn held_label(labels: &[String]) -> &'static str {
    crate::work_finder::OPERATOR_HOLD_NAMES
        .iter()
        .copied()
        .find(|n| labels.iter().any(|l| l == n))
        .unwrap_or(OPERATOR_LABEL)
}

/// The hold's other names `labels` carry beside `held` (retired names and
/// sub-kinds), which an un-escalation removes best-effort. Present ones only:
/// removing a label a repo never had can fail.
#[must_use]
pub fn other_hold_names(labels: &[String], held: &str) -> Vec<&'static str> {
    crate::work_finder::OPERATOR_HOLD_NAMES
        .iter()
        .copied()
        .filter(|n| *n != held && labels.iter().any(|l| l == n))
        .collect()
}

/// Prefix of the marker stamped into the issue **body** by a fact
/// un-escalation's `## Revision` section. Its presence is what stops a retry
/// after a failed label removal from appending the section twice.
pub const FACT_REVISION_MARKER_PREFIX: &str = "<!-- curator:fact-revision:";
