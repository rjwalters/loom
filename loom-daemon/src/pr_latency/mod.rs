//! PR latency decomposition: where the time between "a PR exists" and "it is
//! merged" actually goes (Issue #8923).
//!
//! # Why this is not the cycle-time rollup
//!
//! `defaults/observability/cycle-time-*.sql` (#8665) answers *"what took long
//! to ship?"* from the telemetry stream, and its own question doc says in
//! plain terms what it cannot answer:
//!
//! > **"How long from issue *filed* → curated → built → reviewed → merged?"**
//! > The telemetry stream starts at `sweep.started`; nothing in it carries the
//! > forge's `created_at` for the issue, nor the times a label moved.
//!
//! That is exactly this module's question, so it cannot be a query against
//! that rollup. There is also **no durable label-transition store** anywhere in
//! Loom — `forge_events` is a read-only wake signal, not a history — so every
//! number here is derived **live** from the forge's own timeline, the same way
//! [`crate::claim_reconciliation`] derives verdict staleness live. Building a
//! transitions store would be a materially larger change than "measure first";
//! see `defaults/docs/pr-latency.md`.
//!
//! # The discipline inherited from the cycle-time question set
//!
//! **Absent is never zero.** A segment that could not be measured is `None`,
//! never `0`. A PR that is still waiting has *no* sample for the segment it is
//! waiting in — it appears in the live queue view instead, so "fast" and
//! "not finished yet" can never be read off the same number. This is the
//! single most important property here, because the mistake this issue was
//! filed to correct was precisely a conflation: the original problem statement
//! reported PR **age** and called it queue **dwell**.
//!
//! # Layout
//!
//! - [`history`] — the normalized per-PR event log (pure; no forge access).
//! - [`segments`] — segment derivation and the live queue view.
//! - [`report`] — the aggregate over a set of PRs, and the advisory predicates.
//! - [`stats`] — the percentile summary applied to a set of samples.
//!
//! Forge reads and rendering live in `cli/pr_latency_cmd.rs` /
//! `cli/pr_latency_render.rs`, so everything in here is testable without `gh`.

pub mod history;
pub mod report;
pub mod segments;
pub mod stats;

pub use history::{PrEvent, PrHistory, PrState};
pub use report::LatencyReport;
pub use segments::{
    doctor_backlog, queue_rows, segments_for, DoctorBacklogRow, PrSegments, QueueRow, Treating,
};
pub use stats::Distribution;

/// The Judge queue: a PR awaiting a verdict.
pub const REVIEW_REQUESTED: &str = "loom:review-requested";

/// The approved verdict — a PR that is finished and awaiting merge.
pub const APPROVED: &str = "loom:pr";

/// The changes-requested verdict — a PR routed back to Doctor.
pub const CHANGES_REQUESTED: &str = "loom:changes-requested";

/// Doctor's own in-progress claim on a `loom:changes-requested` PR.
///
/// Reported beside the Doctor backlog because without it "awaiting Doctor" and
/// "Doctor is working it right now" look identical — the conflation that made
/// the first reading of this issue's data blame Doctor throughput.
pub const TREATING: &str = "loom:treating";

/// Both verdict labels, in no significant order.
pub const VERDICT_LABELS: &[&str] = &[APPROVED, CHANGES_REQUESTED];

/// Labels that mean **a human is the only transition out**, and which
/// therefore make an approved-but-unmerged PR's wait *expected* rather than a
/// throughput failure.
///
/// `loom:operator` and `loom:operator-only` are deliberately both here even
/// though they differ for dispatch (the former is re-evaluable, the latter is a
/// park — see [`crate::work_finder::OPERATOR_HOLD_LABEL`]): for the
/// question *"is this PR waiting on a person?"* they are the same answer.
/// `loom:needs-capability` joins them because it hard-skips identically
/// (#5817). The four `loom:operator-*` sub-kinds are **not** listed — they
/// always accompany `loom:operator-only` and so would only double-count; they
/// are surfaced as detail on the row instead.
pub const OPERATOR_GATE_LABELS: &[&str] = &[
    crate::work_finder::OPERATOR_HOLD_LABEL,
    "loom:operator-only",
    "loom:needs-capability",
];

/// True when any label in `labels` is an operator gate.
pub fn is_operator_gated<S: AsRef<str>>(labels: &[S]) -> bool {
    labels
        .iter()
        .any(|l| OPERATOR_GATE_LABELS.contains(&l.as_ref()))
}

/// True when any label in `labels` takes the artifact out of the automation
/// queue entirely ([`crate::work_finder::PARK_LABELS`]).
///
/// Reused rather than re-listed: a park set that drifts from the work finder's
/// own would report a PR as reachable when nothing can reach it.
pub fn is_parked<S: AsRef<str>>(labels: &[S]) -> bool {
    labels
        .iter()
        .any(|l| crate::work_finder::PARK_LABELS.contains(&l.as_ref()))
}

/// Every operator-gate / park label present, for the detail column.
pub fn hold_labels<S: AsRef<str>>(labels: &[S]) -> Vec<String> {
    labels
        .iter()
        .map(|l| l.as_ref())
        .filter(|l| {
            OPERATOR_GATE_LABELS.contains(l)
                || crate::work_finder::PARK_LABELS.contains(l)
                || l.starts_with("loom:operator-")
        })
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_gate_covers_both_hold_flavours_but_not_sub_kinds() {
        assert!(is_operator_gated(&["loom:pr", "loom:operator"]));
        assert!(is_operator_gated(&["loom:operator-only"]));
        assert!(is_operator_gated(&["loom:needs-capability"]));
        // A sub-kind alone is not a gate: it never appears without its base.
        assert!(!is_operator_gated(&["loom:operator-decision"]));
        assert!(!is_operator_gated(&["loom:pr", "loom:blocked"]));
    }

    #[test]
    fn park_set_is_the_work_finders_own() {
        assert!(is_parked(&["loom:blocked"]));
        assert!(is_parked(&["loom:operator-only"]));
        // The re-evaluable hold is NOT a park, matching PARK_LABELS' contract.
        assert!(!is_parked(&["loom:operator"]));
    }

    #[test]
    fn hold_labels_lists_gates_parks_and_sub_kinds_without_duplicates() {
        let held = hold_labels(&[
            "loom:pr",
            "loom:operator-only",
            "loom:operator-decision",
            "loom:blocked",
        ]);
        assert_eq!(
            held,
            vec![
                "loom:operator-only".to_string(),
                "loom:operator-decision".to_string(),
                "loom:blocked".to_string()
            ]
        );
    }
}
