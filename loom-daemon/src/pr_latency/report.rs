//! The aggregate: per-segment distributions plus the live queue view, built
//! from a set of [`PrHistory`] values (Issue #8923).
//!
//! Pure — no forge access, no printing — so the whole question set can be
//! asserted against fixtures. `cli/pr_latency_cmd.rs` supplies the histories
//! and `cli/pr_latency_render.rs` prints the result.

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::history::{PrHistory, PrState};
use super::segments::{doctor_backlog, queue_rows, segments_for, DoctorBacklogRow, QueueRow};
use super::stats::Distribution;
use super::{APPROVED, CHANGES_REQUESTED, REVIEW_REQUESTED};

/// Answers to the whole question set, for one window.
///
/// Question IDs (`PL1`..`PL6`) are the ones documented in
/// `defaults/docs/pr-latency.md`; the doc and this struct are kept in step by
/// `loom-daemon/tests/pr_latency_artifacts.rs`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LatencyReport {
    /// When the measurement was taken. Every dwell in [`Self::queues`] is
    /// relative to this instant, and the whole document is a snapshot: re-run
    /// it rather than quoting an old one.
    pub measured_at: Option<DateTime<Utc>>,

    /// How many PRs were examined, and how the sample splits by state.
    pub prs_examined: usize,
    pub prs_merged: usize,
    pub prs_open: usize,
    pub prs_closed_unmerged: usize,
    /// PRs whose timeline read was incomplete. Their segments are excluded
    /// from every distribution below — a partial log understates every
    /// interval, so folding one in would silently bias the answer fast.
    pub prs_incomplete: Vec<u32>,

    /// **PL1** `loom:review-requested` → first verdict. One sample per lap.
    pub review_wait: Distribution,

    /// **PL2** first verdict → `loom:pr`, for PRs that reached approval.
    pub approval_path: Distribution,

    /// **PL3a** `loom:pr` → merged, **with** an operator gate in force.
    pub merge_wait_gated: Distribution,
    /// **PL3b** `loom:pr` → merged, with **no** operator gate.
    ///
    /// The split is the point: #8923's hypothesis was that the gate, not the
    /// merge machinery, is the dominant term. Comparing these two answers it.
    pub merge_wait_ungated: Distribution,
    /// Merged PRs carrying no `loom:pr` labeling at all, so PL3 is unmeasured
    /// for them. Counted, never treated as a zero-second merge wait.
    pub merged_without_approval_label: usize,

    /// **PL4** `loom:changes-requested` → next push.
    pub doctor_response: Distribution,

    /// **PL5a** approval invalidations: extra Judge reviews of work that was
    /// already approved. Pure waste — this is the number to act on.
    pub approval_invalidations: usize,
    /// **PL5b** repair laps: review re-requested after a *rejection*. The
    /// healthy loop, reported so PL5a cannot be inflated by it.
    pub repair_laps: usize,
    /// PRs that suffered at least one **approval** invalidation, worst first.
    pub invalidated_prs: Vec<(u32, usize)>,

    /// **PL6** the live queue view — open PRs by dwell, longest first.
    pub queues: Vec<QueueRow>,

    /// Open `loom:changes-requested` PRs with no push since the label (AC3).
    pub doctor_backlog: Vec<DoctorBacklogRow>,
}

impl LatencyReport {
    /// Build the report from every history in `histories`, as of `now`.
    pub fn build(histories: &[PrHistory], now: DateTime<Utc>) -> Self {
        let mut r = Self {
            measured_at: Some(now),
            prs_examined: histories.len(),
            queues: queue_rows(histories, now),
            doctor_backlog: doctor_backlog(histories, now),
            ..Default::default()
        };

        let mut review = Vec::new();
        let mut approval = Vec::new();
        let mut gated = Vec::new();
        let mut ungated = Vec::new();
        let mut doctor = Vec::new();

        for h in histories {
            match h.state {
                PrState::Merged => r.prs_merged += 1,
                PrState::Open => r.prs_open += 1,
                PrState::Closed => r.prs_closed_unmerged += 1,
            }
            if !h.timeline_complete {
                r.prs_incomplete.push(h.number);
                continue;
            }

            let s = segments_for(h);
            review.extend(s.review_wait_secs);
            approval.extend(s.approval_path_secs);
            doctor.extend(s.doctor_response_secs);
            match s.merge_wait_secs {
                Some(secs) if s.merge_operator_gated => gated.push(secs),
                Some(secs) => ungated.push(secs),
                None if h.state == PrState::Merged => r.merged_without_approval_label += 1,
                None => {}
            }
            r.approval_invalidations += s.approval_invalidations;
            r.repair_laps += s.repair_laps;
            if s.approval_invalidations > 0 {
                r.invalidated_prs.push((h.number, s.approval_invalidations));
            }
        }

        r.invalidated_prs
            .sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        r.review_wait = Distribution::of(review);
        r.approval_path = Distribution::of(approval);
        r.merge_wait_gated = Distribution::of(gated);
        r.merge_wait_ungated = Distribution::of(ungated);
        r.doctor_response = Distribution::of(doctor);
        r
    }

    /// Live queue rows for one queue label.
    pub fn queue(&self, label: &str) -> Vec<&QueueRow> {
        self.queues.iter().filter(|r| r.queue == label).collect()
    }

    /// Open, operator-gated PRs whose dwell exceeds `threshold_secs`, in **any**
    /// queue — the population the Phase-2 advisory exists to name.
    ///
    /// Queue-agnostic on purpose. [`crate::work_finder::SKIP_LABELS`] includes
    /// [`crate::work_finder::OPERATOR_HOLD_LABEL`], so the engine has stopped on
    /// these PRs whatever queue label they happen to carry: a gated
    /// `loom:review-requested` PR is **not** awaiting a Judge verdict in any
    /// actionable sense, and reporting it as such blames a role that is
    /// correctly refusing to act. Live data on 2026-09-26 contained exactly
    /// that case — #8893 and #8613 sat at 11.1h under `loom:review-requested` +
    /// `loom:operator`.
    ///
    /// A PR with an **unknown** dwell is deliberately excluded: an advisory
    /// that fires on "we could not tell" is an advisory that gets ignored.
    pub fn stalled_operator_holds(&self, threshold_secs: i64) -> Vec<&QueueRow> {
        self.queues
            .iter()
            .filter(|r| r.operator_gated)
            .filter(|r| r.dwell_secs.is_some_and(|d| d >= threshold_secs))
            .collect()
    }

    /// Open, approved, **ungated** PRs whose dwell exceeds `threshold_secs`.
    ///
    /// Reported separately because it is the strictly worse finding: nothing is
    /// waiting on a person, so the only explanation is that the merge lane
    /// itself stalled.
    pub fn stalled_plain_approvals(&self, threshold_secs: i64) -> Vec<&QueueRow> {
        self.queue(APPROVED)
            .into_iter()
            .filter(|r| !r.operator_gated && !r.parked)
            .filter(|r| r.dwell_secs.is_some_and(|d| d >= threshold_secs))
            .collect()
    }

    /// Open PRs genuinely awaiting a Judge verdict beyond `threshold_secs`.
    ///
    /// Operator-gated rows are excluded and reported by
    /// [`Self::stalled_operator_holds`] instead — they are waiting on a person,
    /// not on Judge, and counting them here would attribute a human's hold to a
    /// role that is correctly refusing to act on it.
    pub fn stalled_reviews(&self, threshold_secs: i64) -> Vec<&QueueRow> {
        self.queue(REVIEW_REQUESTED)
            .into_iter()
            .filter(|r| !r.parked && !r.operator_gated)
            .filter(|r| r.dwell_secs.is_some_and(|d| d >= threshold_secs))
            .collect()
    }

    /// The Doctor backlog that is genuinely queued — not being treated, not
    /// parked, and not operator-gated — beyond `threshold_secs`.
    pub fn stalled_doctor_backlog(&self, threshold_secs: i64) -> Vec<&DoctorBacklogRow> {
        self.doctor_backlog
            .iter()
            .filter(|r| r.treating == super::segments::Treating::Queued)
            .filter(|r| !r.operator_gated)
            .filter(|r| r.dwell_secs.is_some_and(|d| d >= threshold_secs))
            .collect()
    }

    /// True when nothing crossed any threshold — the advisory's clear answer.
    ///
    /// The four populations are disjoint by construction (gated / approved
    /// ungated / awaiting-verdict ungated / Doctor-queued ungated), so a PR is
    /// named at most once and the advisory's total is a count of PRs, not of
    /// findings.
    pub fn advisory_is_clear(&self, threshold_secs: i64) -> bool {
        self.stalled_operator_holds(threshold_secs).is_empty()
            && self.stalled_plain_approvals(threshold_secs).is_empty()
            && self.stalled_reviews(threshold_secs).is_empty()
            && self.stalled_doctor_backlog(threshold_secs).is_empty()
    }

    /// Dwell distribution for one live queue, for the queue-depth summary.
    /// Unknown dwells contribute nothing (absent, not zero).
    pub fn queue_dwell(&self, label: &str) -> Distribution {
        Distribution::of(
            self.queue(label)
                .into_iter()
                .filter_map(|r| r.dwell_secs)
                .collect(),
        )
    }

    /// Every live queue label, in report order.
    pub fn queue_labels() -> [&'static str; 3] {
        [REVIEW_REQUESTED, APPROVED, CHANGES_REQUESTED]
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::history::fixtures::*;
    use super::super::segments::Treating;
    use super::*;
    use crate::pr_latency::TREATING;

    const HOUR: i64 = 3600;

    #[test]
    fn the_gate_split_separates_two_populations() {
        let gated =
            merged(1, 50 * HOUR, vec![labeled(APPROVED, HOUR), labeled("loom:operator", 2 * HOUR)]);
        let plain = merged(2, 2 * HOUR, vec![labeled(APPROVED, HOUR)]);
        let r = LatencyReport::build(&[gated, plain], t(60 * HOUR));
        assert_eq!(r.merge_wait_gated.n, 1);
        assert_eq!(r.merge_wait_gated.p50_secs, Some(49 * HOUR));
        assert_eq!(r.merge_wait_ungated.n, 1);
        assert_eq!(r.merge_wait_ungated.p50_secs, Some(HOUR));
    }

    #[test]
    fn an_incomplete_timeline_is_excluded_not_averaged_in() {
        let mut partial = merged(1, 50 * HOUR, vec![labeled(APPROVED, 49 * HOUR)]);
        partial.timeline_complete = false;
        let good = merged(2, 2 * HOUR, vec![labeled(APPROVED, HOUR)]);
        let r = LatencyReport::build(&[partial, good], t(60 * HOUR));
        assert_eq!(r.prs_incomplete, vec![1]);
        assert_eq!(r.merge_wait_ungated.n, 1);
        assert_eq!(r.merge_wait_ungated.max_secs, Some(HOUR));
        // Still counted in the state census — it exists, it was just unreadable.
        assert_eq!(r.prs_merged, 2);
        assert_eq!(r.prs_examined, 2);
    }

    #[test]
    fn a_merge_with_no_approval_label_is_counted_separately() {
        let r = LatencyReport::build(&[merged(1, 5 * HOUR, vec![])], t(6 * HOUR));
        assert_eq!(r.merged_without_approval_label, 1);
        assert!(r.merge_wait_gated.is_empty());
        assert!(r.merge_wait_ungated.is_empty());
    }

    #[test]
    fn approval_invalidations_are_summed_and_attributed() {
        let twice = merged(
            7,
            10 * HOUR,
            vec![
                labeled(REVIEW_REQUESTED, 0),
                labeled(APPROVED, HOUR),
                unlabeled(APPROVED, 2 * HOUR),
                labeled(REVIEW_REQUESTED, 2 * HOUR),
                labeled(APPROVED, 3 * HOUR),
                unlabeled(APPROVED, 4 * HOUR),
                labeled(REVIEW_REQUESTED, 5 * HOUR),
                labeled(APPROVED, 9 * HOUR),
            ],
        );
        let clean =
            merged(8, 2 * HOUR, vec![labeled(REVIEW_REQUESTED, 0), labeled(APPROVED, HOUR)]);
        // A rejection-then-fix lap must land in `repair_laps`, never inflate the
        // waste counter.
        let repaired = merged(
            9,
            5 * HOUR,
            vec![
                labeled(REVIEW_REQUESTED, 0),
                labeled(CHANGES_REQUESTED, HOUR),
                pushed(2 * HOUR),
                labeled(REVIEW_REQUESTED, 3 * HOUR),
                labeled(APPROVED, 4 * HOUR),
            ],
        );
        let r = LatencyReport::build(&[twice, clean, repaired], t(20 * HOUR));
        assert_eq!(r.approval_invalidations, 2);
        assert_eq!(r.repair_laps, 1);
        assert_eq!(r.invalidated_prs, vec![(7, 2)]);
    }

    #[test]
    fn the_advisory_fires_on_a_gated_approval_over_threshold() {
        let held = open(
            10,
            &[APPROVED, "loom:operator"],
            vec![labeled(APPROVED, 0), labeled("loom:operator", 0)],
        );
        let r = LatencyReport::build(&[held], t(90 * HOUR));
        assert!(!r.advisory_is_clear(24 * HOUR));
        let stalled = r.stalled_operator_holds(24 * HOUR);
        assert_eq!(stalled.len(), 1);
        assert_eq!(stalled[0].dwell_secs, Some(90 * HOUR));
        assert!(r.stalled_plain_approvals(24 * HOUR).is_empty());
        // Under a higher threshold the same state is clear.
        assert!(r.advisory_is_clear(200 * HOUR));
    }

    #[test]
    fn a_gated_review_request_is_a_human_hold_not_a_judge_stall() {
        // The live case on 2026-09-26: #8893 and #8613 sat at 11.1h under
        // `loom:review-requested` + `loom:operator`. `loom:operator` is in
        // `SKIP_LABELS`, so the engine has stopped — reporting these as
        // "awaiting a Judge verdict" blames a role that correctly refuses to
        // act on them.
        let gated =
            open(8893, &[REVIEW_REQUESTED, "loom:operator"], vec![labeled(REVIEW_REQUESTED, 0)]);
        let genuine = open(9046, &[REVIEW_REQUESTED], vec![labeled(REVIEW_REQUESTED, 0)]);
        let r = LatencyReport::build(&[gated, genuine], t(12 * HOUR));

        let judge = r.stalled_reviews(8 * HOUR);
        assert_eq!(judge.len(), 1, "only the ungated PR is Judge's to answer");
        assert_eq!(judge[0].pr, 9046);

        let people = r.stalled_operator_holds(8 * HOUR);
        assert_eq!(people.len(), 1);
        assert_eq!(people[0].pr, 8893);
        // The hold is reported with the queue it is held in, not as an approval.
        assert_eq!(people[0].queue, REVIEW_REQUESTED);
        // Each PR is named exactly once across the two populations.
        assert!(r.stalled_plain_approvals(8 * HOUR).is_empty());
    }

    #[test]
    fn a_gated_rejection_is_not_counted_against_doctor() {
        let gated =
            open(1, &[CHANGES_REQUESTED, "loom:operator"], vec![labeled(CHANGES_REQUESTED, 0)]);
        let genuine = open(2, &[CHANGES_REQUESTED], vec![labeled(CHANGES_REQUESTED, 0)]);
        let r = LatencyReport::build(&[gated, genuine], t(50 * HOUR));

        let doctor = r.stalled_doctor_backlog(24 * HOUR);
        assert_eq!(doctor.len(), 1);
        assert_eq!(doctor[0].pr, 2);
        assert!(!doctor[0].operator_gated);
        // The gated one is still visible — as a human hold, in its own queue.
        assert_eq!(r.stalled_operator_holds(24 * HOUR).len(), 1);
        // Both remain in the full backlog listing; only the ALARM is filtered.
        assert_eq!(r.doctor_backlog.len(), 2);
    }

    #[test]
    fn a_gated_approval_under_threshold_is_clear() {
        let held = open(11, &[APPROVED, "loom:operator"], vec![labeled(APPROVED, 0)]);
        let r = LatencyReport::build(&[held], t(2 * HOUR));
        assert!(r.advisory_is_clear(24 * HOUR));
    }

    #[test]
    fn an_unknown_dwell_never_fires_the_advisory() {
        // Label present, no labeling event on the (truncated) timeline.
        let unknown = open(12, &[APPROVED, "loom:operator"], vec![]);
        let r = LatencyReport::build(&[unknown], t(500 * HOUR));
        assert!(r.advisory_is_clear(HOUR));
        assert_eq!(r.queues.len(), 1);
        assert_eq!(r.queues[0].dwell_secs, None);
    }

    #[test]
    fn treating_and_parked_rows_are_not_a_doctor_backlog_alarm() {
        let treating =
            open(20, &[CHANGES_REQUESTED, TREATING], vec![labeled(CHANGES_REQUESTED, 0)]);
        let parked =
            open(21, &[CHANGES_REQUESTED, "loom:blocked"], vec![labeled(CHANGES_REQUESTED, 0)]);
        let queued = open(22, &[CHANGES_REQUESTED], vec![labeled(CHANGES_REQUESTED, 0)]);
        let r = LatencyReport::build(&[treating, parked, queued], t(100 * HOUR));
        assert_eq!(r.doctor_backlog.len(), 3);
        let stalled = r.stalled_doctor_backlog(24 * HOUR);
        assert_eq!(stalled.len(), 1);
        assert_eq!(stalled[0].pr, 22);
        assert_eq!(stalled[0].treating, Treating::Queued);
    }

    #[test]
    fn queue_dwell_summarises_only_known_dwells() {
        let a = open(30, &[APPROVED], vec![labeled(APPROVED, 0)]);
        let b = open(31, &[APPROVED], vec![]);
        let r = LatencyReport::build(&[a, b], t(10 * HOUR));
        let d = r.queue_dwell(APPROVED);
        assert_eq!(d.n, 1);
        assert_eq!(d.p50_secs, Some(10 * HOUR));
        assert_eq!(r.queue(APPROVED).len(), 2);
    }

    #[test]
    fn an_empty_window_reports_absence_everywhere() {
        let r = LatencyReport::build(&[], t(0));
        assert_eq!(r.prs_examined, 0);
        assert!(r.review_wait.is_empty());
        assert!(r.merge_wait_gated.is_empty());
        assert!(r.queues.is_empty());
        assert!(r.advisory_is_clear(HOUR));
    }
}
