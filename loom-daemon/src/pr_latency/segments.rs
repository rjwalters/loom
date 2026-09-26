//! Segment derivation and the live queue view (Issue #8923).
//!
//! Two different questions, deliberately answered by two different functions
//! over the same input:
//!
//! - [`segments_for`] is **history**: closed intervals between two observed
//!   events on one PR. A PR still waiting contributes nothing.
//! - [`queue_rows`] is **right now**: how long each open PR has been sitting in
//!   the queue it is in. Dwell since its queue label was applied — never PR
//!   age, which is the conflation this issue was filed to correct.
//!
//! Mixing them is what produced "changes-requested is 90h deep" from a queue
//! whose real dwell was four hours.

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::history::{PrEvent, PrHistory, PrState};
use super::{APPROVED, CHANGES_REQUESTED, REVIEW_REQUESTED, TREATING};

/// Everything one PR contributes to the historical segment tables.
///
/// Every field is an absence when unmeasured. A `Vec` may legitimately hold
/// more than one sample: a PR that went round the loop twice waited in the
/// review queue twice, and averaging those into one would hide the lap.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PrSegments {
    pub pr: u32,

    /// **PL1** — `loom:review-requested` → the next verdict, one sample per
    /// lap. A still-unanswered request yields no sample.
    pub review_wait_secs: Vec<i64>,

    /// **PL2** — first verdict → `loom:pr`. The cost of the repair loop for
    /// PRs that eventually got approved; `Some(0)` when approved first time.
    /// `None` when never approved.
    pub approval_path_secs: Option<i64>,

    /// **PL3** — the `loom:pr` labeling in force at merge → merge. `None` for
    /// an unmerged PR, and also for one merged with no approval labeling at all
    /// (an unmeasured merge, not a fast one).
    pub merge_wait_secs: Option<i64>,

    /// Whether an operator gate was in force during the PL3 window. Meaningful
    /// only when `merge_wait_secs` is `Some`.
    pub merge_operator_gated: bool,

    /// **PL4** — `loom:changes-requested` → the next push, one sample per lap
    /// that got one.
    pub doctor_response_secs: Vec<i64>,

    /// Laps where `loom:changes-requested` was applied and **no** push ever
    /// followed. On a merged PR this means Judge reversed itself without a
    /// code change; on an open one it is the live Doctor backlog.
    pub changes_without_push: usize,

    /// **PL5a** — **approval** invalidations: the PR was in the `loom:pr`
    /// (approved) state and went back to `loom:review-requested`. Each one is a
    /// full extra Judge review of work that had already passed, which is the
    /// only kind of extra lap that is pure waste.
    ///
    /// `claim_reconciliation::invalidate_verdict` writes this signature: it
    /// clears the verdict label and re-applies `loom:review-requested` when the
    /// head SHA has moved past the verdict's recorded marker.
    pub approval_invalidations: usize,

    /// **PL5b** — repair laps: `loom:review-requested` re-applied while the
    /// standing verdict was `loom:changes-requested`. This is the **healthy**
    /// loop (Judge rejected, Doctor fixed, review again) and is counted
    /// separately precisely so it cannot be reported as waste. Conflating the
    /// two is how "56 extra laps" reads as a crisis when most of them are the
    /// pipeline working.
    pub repair_laps: usize,

    /// `false` when the timeline read was incomplete. Such a row's numbers are
    /// reported separately and never folded into a distribution.
    pub complete: bool,
}

/// Derive every historical segment for one PR.
pub fn segments_for(h: &PrHistory) -> PrSegments {
    let mut s = PrSegments {
        pr: h.number,
        complete: h.timeline_complete,
        ..Default::default()
    };

    // PL1: each review request, to the verdict that answered it.
    for req in h.labelings(REVIEW_REQUESTED) {
        if let Some(verdict) = h.next_verdict_after(req) {
            s.review_wait_secs.push(secs_between(req, verdict));
        }
    }

    // PL2: first verdict to approval. Zero when the first verdict *was* the
    // approval — a real measurement, distinct from the `None` below.
    let first_verdict = h.first_verdict();
    if let (Some(v0), Some(approved)) = (first_verdict, h.first_labeled(APPROVED)) {
        s.approval_path_secs = Some(secs_between(v0, approved));
    }

    // PL3: the approval in force at merge, to the merge.
    if let Some(merged_at) = h.merged_at {
        if let Some(approved_at) = h.last_labeled_before(APPROVED, merged_at) {
            s.merge_wait_secs = Some(secs_between(approved_at, merged_at));
            s.merge_operator_gated = h.operator_gated_between(approved_at, merged_at);
        }
    }

    // PL4: each changes-requested verdict, to the push that answered it.
    for rejected in h.labelings(CHANGES_REQUESTED) {
        match h.next_push_after(rejected) {
            Some(push) => s.doctor_response_secs.push(secs_between(rejected, push)),
            None => s.changes_without_push += 1,
        }
    }

    // PL5: classify each extra review request by the verdict that was standing
    // when it arrived. Walking the log is required rather than counting
    // labelings after `first_verdict`, because "the PR was already approved" and
    // "the PR was rejected and has been fixed" are the two cases that must not
    // be one number.
    let mut standing: Option<&str> = None;
    for e in &h.events {
        match e {
            PrEvent::Labeled { label, .. } if label == APPROVED => standing = Some(APPROVED),
            PrEvent::Labeled { label, .. } if label == CHANGES_REQUESTED => {
                standing = Some(CHANGES_REQUESTED);
            }
            PrEvent::Labeled { label, .. } if label == REVIEW_REQUESTED => {
                if standing == Some(APPROVED) {
                    s.approval_invalidations += 1;
                } else if standing.is_some() {
                    s.repair_laps += 1;
                }
                // Consumed: a second request with no verdict in between is the
                // same lap re-announced, not another one. A `None` standing is
                // the first request on a fresh PR, which is not a lap at all.
                standing = None;
            }
            _ => {}
        }
    }

    s
}

/// Whether Doctor has picked a rejected PR up (`loom:treating`), which is the
/// difference between a backlog and work in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Treating {
    /// `loom:treating` is on the PR: Doctor is working it now.
    InProgress,
    /// No `loom:treating`, no park: genuinely queued for Doctor.
    Queued,
    /// Parked ([`super::is_parked`]) — out of the automation queue entirely, so
    /// it is not a Doctor-throughput observation at all.
    Parked,
}

/// One open PR in a verdict queue, as of `now`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueueRow {
    pub pr: u32,
    /// The queue label the PR currently carries.
    pub queue: String,
    /// Seconds since that label was applied. **This is dwell, not age.**
    /// `None` when the label is present but its `labeled` event is not in the
    /// (possibly truncated) timeline — unknown, never 0.
    pub dwell_secs: Option<i64>,
    /// Seconds since the PR was opened. Kept beside dwell precisely so the two
    /// can never be confused again; they differ by an order of magnitude here.
    pub age_secs: i64,
    pub operator_gated: bool,
    pub parked: bool,
    /// The gate/park/sub-kind labels present, for the reason column.
    pub holds: Vec<String>,
    pub treating: Treating,
    /// Seconds since the last push, `None` if none is on the timeline.
    pub since_push_secs: Option<i64>,
}

/// The live queue view: one row per open PR that carries a verdict or
/// review-request label, ordered longest-dwelling first.
///
/// A PR carrying none of the three (e.g. a draft, or one held with its verdict
/// stripped) is deliberately absent: it is in no queue this measures, and
/// inventing a bucket for it is how the original data grew a "held/other" row
/// that meant nothing.
pub fn queue_rows(histories: &[PrHistory], now: DateTime<Utc>) -> Vec<QueueRow> {
    let mut rows: Vec<QueueRow> = histories
        .iter()
        .filter(|h| h.state == PrState::Open)
        .filter_map(|h| queue_row(h, now))
        .collect();
    // Longest dwell first; unknown dwell sorts last (it is not "zero wait").
    rows.sort_by(|a, b| {
        b.dwell_secs
            .unwrap_or(i64::MIN)
            .cmp(&a.dwell_secs.unwrap_or(i64::MIN))
            .then(a.pr.cmp(&b.pr))
    });
    rows
}

fn queue_row(h: &PrHistory, now: DateTime<Utc>) -> Option<QueueRow> {
    // Precedence when a PR somehow carries more than one: the verdict wins
    // over the request, because that is the state a reader must act on. The
    // combination is itself a contradiction `merge-pr.sh` refuses (#8112).
    let queue = [APPROVED, CHANGES_REQUESTED, REVIEW_REQUESTED]
        .into_iter()
        .find(|l| h.current_labels.iter().any(|c| c == l))?;

    let parked = super::is_parked(&h.current_labels);
    let treating = if parked {
        Treating::Parked
    } else if h.current_labels.iter().any(|l| l == TREATING) {
        Treating::InProgress
    } else {
        Treating::Queued
    };

    Some(QueueRow {
        pr: h.number,
        queue: queue.to_string(),
        dwell_secs: h
            .last_labeled_before(queue, now)
            .map(|at| secs_between(at, now)),
        age_secs: secs_between(h.created_at, now),
        operator_gated: super::is_operator_gated(&h.current_labels),
        parked,
        holds: super::hold_labels(&h.current_labels),
        treating,
        since_push_secs: h
            .events
            .iter()
            .filter(|e| matches!(e, PrEvent::Pushed { .. }))
            .map(PrEvent::at)
            .next_back()
            .map(|at| secs_between(at, now)),
    })
}

/// One rejected PR that Doctor has not answered with a push.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DoctorBacklogRow {
    pub pr: u32,
    /// Dwell since `loom:changes-requested` was applied — the number AC3 asks
    /// for, and not the PR's age.
    pub dwell_secs: Option<i64>,
    pub treating: Treating,
    pub holds: Vec<String>,
}

/// Every open `loom:changes-requested` PR with **no push since** the label.
///
/// Acceptance criterion 3 of #8923. Split by [`Treating`] rather than reported
/// as one count, because the original reading of this queue's depth blamed
/// Doctor throughput for rows that were either being treated at that moment or
/// parked out of Doctor's reach entirely.
pub fn doctor_backlog(histories: &[PrHistory], now: DateTime<Utc>) -> Vec<DoctorBacklogRow> {
    let mut rows: Vec<DoctorBacklogRow> = histories
        .iter()
        .filter(|h| h.state == PrState::Open)
        .filter(|h| h.current_labels.iter().any(|l| l == CHANGES_REQUESTED))
        .filter_map(|h| {
            let applied = h.last_labeled_before(CHANGES_REQUESTED, now);
            // A push after the label means Doctor answered; not backlog.
            if let Some(applied) = applied {
                if h.next_push_after(applied).is_some() {
                    return None;
                }
            }
            Some(DoctorBacklogRow {
                pr: h.number,
                dwell_secs: applied.map(|at| secs_between(at, now)),
                treating: if super::is_parked(&h.current_labels) {
                    Treating::Parked
                } else if h.current_labels.iter().any(|l| l == TREATING) {
                    Treating::InProgress
                } else {
                    Treating::Queued
                },
                holds: super::hold_labels(&h.current_labels),
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        b.dwell_secs
            .unwrap_or(i64::MIN)
            .cmp(&a.dwell_secs.unwrap_or(i64::MIN))
            .then(a.pr.cmp(&b.pr))
    });
    rows
}

/// Non-negative seconds from `from` to `to`.
///
/// Clamped at zero: forge timestamps have one-second resolution and two events
/// in the same API batch can land out of order by a second, which must read as
/// "instant", never as a negative duration poisoning a sum.
fn secs_between(from: DateTime<Utc>, to: DateTime<Utc>) -> i64 {
    (to - from).num_seconds().max(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::history::fixtures::*;
    use super::*;

    const HOUR: i64 = 3600;

    #[test]
    fn happy_path_merged_pr_yields_one_sample_per_segment() {
        // requested at 1h, approved at 2h, merged at 3h.
        let h = merged(
            10,
            3 * HOUR,
            vec![labeled(REVIEW_REQUESTED, HOUR), labeled(APPROVED, 2 * HOUR)],
        );
        let s = segments_for(&h);
        assert_eq!(s.review_wait_secs, vec![HOUR]);
        assert_eq!(s.approval_path_secs, Some(0));
        assert_eq!(s.merge_wait_secs, Some(HOUR));
        assert!(!s.merge_operator_gated);
        assert_eq!(s.doctor_response_secs, Vec::<i64>::new());
        assert_eq!(s.approval_invalidations, 0);
        assert_eq!(s.repair_laps, 0);
    }

    #[test]
    fn a_repair_lap_is_two_review_waits_not_an_average() {
        // requested 1h -> changes 2h -> push 3h -> requested 4h -> approved 6h
        // -> merged 7h.
        let h = merged(
            11,
            7 * HOUR,
            vec![
                labeled(REVIEW_REQUESTED, HOUR),
                labeled(CHANGES_REQUESTED, 2 * HOUR),
                pushed(3 * HOUR),
                labeled(REVIEW_REQUESTED, 4 * HOUR),
                labeled(APPROVED, 6 * HOUR),
            ],
        );
        let s = segments_for(&h);
        assert_eq!(s.review_wait_secs, vec![HOUR, 2 * HOUR]);
        // First verdict was the rejection at 2h; approval landed at 6h.
        assert_eq!(s.approval_path_secs, Some(4 * HOUR));
        assert_eq!(s.doctor_response_secs, vec![HOUR]);
        assert_eq!(s.changes_without_push, 0);
        // The second review request followed a REJECTION: a healthy repair
        // lap, not an invalidated approval.
        assert_eq!(s.repair_laps, 1);
        assert_eq!(s.approval_invalidations, 0);
        assert_eq!(s.merge_wait_secs, Some(HOUR));
    }

    #[test]
    fn merge_wait_uses_the_approval_in_force_at_merge() {
        // Approved at 1h, invalidated, re-approved at 5h, merged at 6h. The
        // answer is 1h, not 5h: the stale approval is not what gated the merge.
        let h = merged(
            12,
            6 * HOUR,
            vec![
                labeled(REVIEW_REQUESTED, 0),
                labeled(APPROVED, HOUR),
                unlabeled(APPROVED, 2 * HOUR),
                labeled(REVIEW_REQUESTED, 2 * HOUR),
                labeled(APPROVED, 5 * HOUR),
            ],
        );
        let s = segments_for(&h);
        assert_eq!(s.merge_wait_secs, Some(HOUR));
        // Approved, then sent back to review: a wasted re-review, not a repair.
        assert_eq!(s.approval_invalidations, 1);
        assert_eq!(s.repair_laps, 0);
    }

    #[test]
    fn operator_gate_during_the_merge_window_is_recorded() {
        let h = merged(
            13,
            10 * HOUR,
            vec![labeled(APPROVED, HOUR), labeled("loom:operator", 2 * HOUR)],
        );
        let s = segments_for(&h);
        assert_eq!(s.merge_wait_secs, Some(9 * HOUR));
        assert!(s.merge_operator_gated);
    }

    #[test]
    fn a_merge_with_no_approval_label_is_unmeasured_not_instant() {
        let h = merged(14, 5 * HOUR, vec![labeled(REVIEW_REQUESTED, HOUR)]);
        let s = segments_for(&h);
        assert_eq!(s.merge_wait_secs, None);
        assert_eq!(s.approval_path_secs, None);
        // The unanswered review request contributes no sample either.
        assert_eq!(s.review_wait_secs, Vec::<i64>::new());
    }

    #[test]
    fn changes_requested_with_no_push_is_counted_not_timed() {
        let h = merged(
            15,
            5 * HOUR,
            vec![
                labeled(CHANGES_REQUESTED, HOUR),
                labeled(APPROVED, 4 * HOUR),
            ],
        );
        let s = segments_for(&h);
        assert_eq!(s.doctor_response_secs, Vec::<i64>::new());
        assert_eq!(s.changes_without_push, 1);
    }

    #[test]
    fn queue_rows_report_dwell_not_age() {
        // Opened at t=0, rejected at t=100h, measured at t=101h. Age is 101h;
        // dwell is 1h. Reporting the former as the latter is this issue's bug.
        let h = open(
            20,
            &[CHANGES_REQUESTED],
            vec![
                labeled(REVIEW_REQUESTED, 0),
                labeled(CHANGES_REQUESTED, 100 * HOUR),
            ],
        );
        let rows = queue_rows(&[h], t(101 * HOUR));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].dwell_secs, Some(HOUR));
        assert_eq!(rows[0].age_secs, 101 * HOUR);
        assert_eq!(rows[0].treating, Treating::Queued);
    }

    #[test]
    fn queue_rows_sort_longest_dwell_first_and_unknown_dwell_last() {
        let short = open(1, &[APPROVED], vec![labeled(APPROVED, 100 * HOUR)]);
        let long = open(2, &[APPROVED], vec![labeled(APPROVED, 10 * HOUR)]);
        // Label present, labeling event absent from the timeline => unknown.
        let unknown = open(3, &[APPROVED], vec![]);
        let rows = queue_rows(&[short, long, unknown], t(101 * HOUR));
        assert_eq!(rows.iter().map(|r| r.pr).collect::<Vec<_>>(), vec![2, 1, 3]);
        assert_eq!(rows[2].dwell_secs, None);
    }

    #[test]
    fn a_verdict_wins_over_a_stale_review_request_label() {
        let h = open(
            4,
            &[REVIEW_REQUESTED, APPROVED],
            vec![labeled(REVIEW_REQUESTED, 0), labeled(APPROVED, HOUR)],
        );
        let rows = queue_rows(&[h], t(2 * HOUR));
        assert_eq!(rows[0].queue, APPROVED);
        assert_eq!(rows[0].dwell_secs, Some(HOUR));
    }

    #[test]
    fn a_pr_in_no_queue_is_absent_rather_than_bucketed() {
        let h = open(5, &["loom:operator"], vec![]);
        assert!(queue_rows(&[h], t(HOUR)).is_empty());
    }

    #[test]
    fn merged_prs_never_appear_in_the_live_queue() {
        let h = merged(6, HOUR, vec![labeled(APPROVED, 0)]);
        assert!(queue_rows(&[h], t(2 * HOUR)).is_empty());
    }

    #[test]
    fn doctor_backlog_excludes_answered_and_classifies_the_rest() {
        let answered = open(
            30,
            &[CHANGES_REQUESTED],
            vec![labeled(CHANGES_REQUESTED, HOUR), pushed(2 * HOUR)],
        );
        let queued = open(31, &[CHANGES_REQUESTED], vec![labeled(CHANGES_REQUESTED, HOUR)]);
        let in_progress =
            open(32, &[CHANGES_REQUESTED, TREATING], vec![labeled(CHANGES_REQUESTED, 2 * HOUR)]);
        let parked = open(
            33,
            &[CHANGES_REQUESTED, "loom:blocked"],
            vec![labeled(CHANGES_REQUESTED, 3 * HOUR)],
        );
        let rows = doctor_backlog(&[answered, queued, in_progress, parked], t(10 * HOUR));
        assert_eq!(rows.iter().map(|r| r.pr).collect::<Vec<_>>(), vec![31, 32, 33]);
        assert_eq!(rows[0].treating, Treating::Queued);
        assert_eq!(rows[0].dwell_secs, Some(9 * HOUR));
        assert_eq!(rows[1].treating, Treating::InProgress);
        assert_eq!(rows[2].treating, Treating::Parked);
        assert_eq!(rows[2].holds, vec!["loom:blocked".to_string()]);
    }

    #[test]
    fn a_push_before_the_label_does_not_answer_it() {
        let h = open(
            34,
            &[CHANGES_REQUESTED],
            vec![pushed(HOUR), labeled(CHANGES_REQUESTED, 2 * HOUR)],
        );
        let rows = doctor_backlog(&[h], t(3 * HOUR));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].dwell_secs, Some(HOUR));
    }

    #[test]
    fn out_of_order_timestamps_clamp_to_zero_rather_than_go_negative() {
        // Same-second batch: the verdict's recorded time precedes the request's.
        let h = merged(40, HOUR, vec![labeled(REVIEW_REQUESTED, 10), labeled(APPROVED, 9)]);
        let s = segments_for(&h);
        // The approval is sorted before the request, so the request has no
        // verdict after it: no sample, and certainly no negative one.
        assert!(s.review_wait_secs.iter().all(|v| *v >= 0));
    }
}
