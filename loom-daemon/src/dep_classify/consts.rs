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

/// The label that parks a proposal for an operator.
pub const OPERATOR_ONLY_LABEL: &str = "loom:operator-only";

/// The #5671 sub-kind label. Must never outlive the base label it accompanies,
/// but a pre-#5679 escalation never carried one — so its absence is normal.
pub const OPERATOR_BLOCKED_LABEL: &str = "loom:operator-blocked";

/// The #7650 sub-kind label a **fact**-checkable escalation carries instead of
/// [`OPERATOR_BLOCKED_LABEL`]. `champion-issue-promo.md` selects it whenever a
/// recurring finding is not a pure dependency citation.
pub const OPERATOR_DECISION_LABEL: &str = "loom:operator-decision";

/// Prefix of the marker stamped into the issue **body** by a fact
/// un-escalation's `## Revision` section. Its presence is what stops a retry
/// after a failed label removal from appending the section twice.
pub const FACT_REVISION_MARKER_PREFIX: &str = "<!-- curator:fact-revision:";
