//! In-sweep **rework events**, reconstructed from the PR's forge label
//! timeline (Issue #9444).
//!
//! # Why the label timeline again
//!
//! The same three reasons #8222 gave for sourcing `doctor_cycles` /
//! `judge_verdicts` here rather than from the sampled checkpoint history apply
//! verbatim: the events are durable forge state, they are written by the
//! Judge/Champion/Doctor themselves rather than by an agent-authored stats
//! file, and a cycle that opens and closes between two reaper ticks is still
//! visible. The decisive practical reason is cheaper still — **this costs no
//! forge call at all.** The terminal transition already fetches the timeline
//! once for those two fields; this module is a second fold over the events it
//! already has in hand.
//!
//! # The four shapes, and why two of them are not the same thing
//!
//! | label sequence | event | class |
//! |---|---|---|
//! | `loom:changes-requested` → `loom:review-requested` | [`ReworkKind::Rejudge`] | substantive |
//! | `loom:pr` → `loom:review-requested`, no change request between | [`ReworkKind::StaleBaseRejudge`] | environmental |
//! | `loom:merge-conflict` | [`ReworkKind::MergeConflict`] | environmental |
//! | `loom:ci-failure` | [`ReworkKind::CiRerun`] | environmental |
//!
//! The first two rows are the split this whole issue exists for. Both are "the
//! Judge looked again", and a naive count of `loom:review-requested` arrivals
//! merges them — but one means *a reviewer faulted the work* and the other
//! means *an approval was invalidated by something nobody faulted* (the base
//! moved, or its required checks went stale under #8248). Charging the second
//! to the issue's size is precisely how a churny week reads as a hard one.
//!
//! # Agreement with `doctor_cycles` is a contract, not a coincidence
//!
//! A [`ReworkKind::Rejudge`] event is emitted under **exactly** the condition
//! #8222 counts a completed Doctor cycle: a `loom:changes-requested` arrival
//! that a later `loom:review-requested` arrival closed the loop on. A terminal
//! rejection nobody handed back is a verdict, not a cycle, and likewise not a
//! rework event. So `rework_events` filtered to `rejudge` always has the same
//! length as `doctor_cycles`, and a consumer can never see the two disagree —
//! [`tests::rejudge_events_always_agree_with_the_doctor_cycle_count`] pins it.
//!
//! # Openness is reported, not smoothed
//!
//! A conflict or CI failure still unresolved when the timeline ends is emitted
//! with **no** `duration_sec`: the rework happened (the label is the proof)
//! but its clearing event was never observed. Omitting the duration is the
//! same "unknown != zero" discipline the rest of this record uses — a `0`
//! there would report a conflict that cost nothing.

use super::label_timeline::{
    LabelEvent, APPROVED_LABEL, CHANGES_REQUESTED_LABEL, REVIEW_REQUESTED_LABEL,
};
use super::*;
use crate::telemetry::{ReworkEvent, ReworkKind, MAX_REWORK_EVENTS};

/// The label Champion/Doctor apply when a PR cannot merge because it conflicts
/// with its base.
pub(crate) const MERGE_CONFLICT_LABEL: &str = "loom:merge-conflict";

/// The label applied when CI failed on a PR.
pub(crate) const CI_FAILURE_LABEL: &str = "loom:ci-failure";

/// `reason` for a rework opened by a bare label arrival: the label itself.
/// Bounded vocabulary by construction — these are the only strings this module
/// ever writes into a `reason`, so nothing agent-authored reaches the wire.
const REASON_APPROVAL_SUPERSEDED: &str = "approval-superseded";

/// One rework still waiting for the event that closes it.
#[derive(Debug, Clone, Copy)]
struct Pending {
    kind: ReworkKind,
    opened_at: DateTime<Utc>,
}

/// Reconstruct this PR's in-sweep rework from its `labeled` events.
///
/// Returned in **opening order** — the order the rework started, not the order
/// it finished — so a reader sees the lifecycle as it unfolded. Capped at
/// [`MAX_REWORK_EVENTS`], like the verdict list beside it.
#[must_use]
pub(crate) fn rework_from_events(events: &[LabelEvent]) -> Vec<ReworkEvent> {
    // Each slot holds at most one open rework of its kind; a repeated arrival
    // of the same label while one is already open is the same rework (a label
    // removed and re-applied), not a second one.
    let mut open_change_request: Option<Pending> = None;
    let mut open_approval: Option<Pending> = None;
    let mut open_conflict: Option<Pending> = None;
    let mut open_ci: Option<Pending> = None;
    let mut closed: Vec<(DateTime<Utc>, ReworkEvent)> = Vec::new();

    for event in events {
        match event.label.as_str() {
            REVIEW_REQUESTED_LABEL => {
                // A hand-back settles at most ONE of the two re-judge shapes,
                // and the change request wins: if a reviewer faulted the work,
                // that is why the Judge is looking again, whatever else moved.
                let reopened = open_change_request.take().or_else(|| open_approval.take());
                if let Some(pending) = reopened {
                    close(&mut closed, pending, event.at, reason_for(pending.kind));
                }
                // An approval can only be superseded once; a hand-back also
                // invalidates any approval still standing.
                open_approval = None;
                // Going back to the Judge is how a conflict/CI rework ends in
                // practice — the fix was pushed and re-review requested.
                settle(&mut closed, &mut open_conflict, event.at);
                settle(&mut closed, &mut open_ci, event.at);
            }
            APPROVED_LABEL => {
                // Arm the stale-base shape: if this approval is later followed
                // by another `loom:review-requested` with no change request in
                // between, it was invalidated by something nobody faulted.
                open_approval = Some(Pending {
                    kind: ReworkKind::StaleBaseRejudge,
                    opened_at: event.at,
                });
                settle(&mut closed, &mut open_conflict, event.at);
                settle(&mut closed, &mut open_ci, event.at);
            }
            CHANGES_REQUESTED_LABEL => {
                // A change request supersedes any standing approval outright:
                // a hand-back after this one is substantive, not stale-base.
                open_approval = None;
                open_change_request.get_or_insert(Pending {
                    kind: ReworkKind::Rejudge,
                    opened_at: event.at,
                });
            }
            MERGE_CONFLICT_LABEL => {
                open_conflict.get_or_insert(Pending {
                    kind: ReworkKind::MergeConflict,
                    opened_at: event.at,
                });
            }
            CI_FAILURE_LABEL => {
                open_ci.get_or_insert(Pending {
                    kind: ReworkKind::CiRerun,
                    opened_at: event.at,
                });
            }
            _ => {}
        }
    }

    // A conflict or CI failure still open when the timeline ends really
    // happened — the label is the proof — but its cost was never bounded. It
    // is emitted with no duration rather than dropped or zeroed.
    //
    // An unclosed change request is deliberately NOT emitted: #8222 defines a
    // rejection nobody handed back as a verdict and not a Doctor cycle, and
    // this list must never disagree with `doctor_cycles`. An unclosed approval
    // is not rework at all — it is a PR sitting approved.
    for pending in [open_conflict, open_ci].into_iter().flatten() {
        closed.push((
            pending.opened_at,
            ReworkEvent::new(pending.kind, Some(reason_for(pending.kind)), None),
        ));
    }

    closed.sort_by_key(|(opened_at, _)| *opened_at);
    closed.truncate(MAX_REWORK_EVENTS);
    closed.into_iter().map(|(_, event)| event).collect()
}

/// The bounded `reason` string for a rework opened by `kind`.
fn reason_for(kind: ReworkKind) -> String {
    match kind {
        ReworkKind::Rejudge => CHANGES_REQUESTED_LABEL.to_string(),
        ReworkKind::StaleBaseRejudge => REASON_APPROVAL_SUPERSEDED.to_string(),
        ReworkKind::MergeConflict => MERGE_CONFLICT_LABEL.to_string(),
        ReworkKind::CiRerun => CI_FAILURE_LABEL.to_string(),
        // Unreachable from this module — no label sequence opens a rebase (see
        // `ReworkKind::Rebase`'s doc) — but a total match keeps the day that
        // writer lands a one-line change here rather than a panic.
        ReworkKind::Rebase => ReworkKind::Rebase.as_str().to_string(),
    }
}

/// Close `slot` at `at`, if it holds an open rework.
fn settle(
    closed: &mut Vec<(DateTime<Utc>, ReworkEvent)>,
    slot: &mut Option<Pending>,
    at: DateTime<Utc>,
) {
    if let Some(pending) = slot.take() {
        close(closed, pending, at, reason_for(pending.kind));
    }
}

/// Record `pending` as closed at `at`, measuring its duration.
///
/// Saturating at zero: the forge stamps whole seconds, so two events inside
/// one second can arrive in an order that makes the delta negative, and a
/// negative rework duration is never a fact worth publishing.
fn close(
    closed: &mut Vec<(DateTime<Utc>, ReworkEvent)>,
    pending: Pending,
    at: DateTime<Utc>,
    reason: String,
) {
    let duration_sec = (at - pending.opened_at).num_seconds().max(0);
    closed.push((
        pending.opened_at,
        ReworkEvent::new(pending.kind, Some(reason), Some(duration_sec)),
    ));
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::super::label_timeline::{parse_label_events, signals_from_events};
    use super::*;
    use crate::telemetry::ReworkClass;

    fn events(rows: &[(&str, &str)]) -> Vec<LabelEvent> {
        let joined: String = rows
            .iter()
            .map(|(at, label)| format!("{at}\t{label}\n"))
            .collect();
        parse_label_events(joined.as_bytes())
    }

    /// AC2: a Judge `changes-requested` → Doctor hand-back is one
    /// **substantive** rework event, with the real wall-clock cost of the loop.
    #[test]
    fn a_change_request_closed_by_a_handback_is_substantive_rejudge() {
        let rework = rework_from_events(&events(&[
            ("2026-09-18T12:00:00Z", REVIEW_REQUESTED_LABEL),
            ("2026-09-18T12:30:00Z", CHANGES_REQUESTED_LABEL),
            ("2026-09-18T13:00:00Z", REVIEW_REQUESTED_LABEL),
            ("2026-09-18T13:30:00Z", APPROVED_LABEL),
        ]));
        assert_eq!(rework.len(), 1);
        assert_eq!(rework[0].kind, ReworkKind::Rejudge);
        assert_eq!(rework[0].classification, ReworkClass::Substantive);
        assert_eq!(rework[0].reason.as_deref(), Some(CHANGES_REQUESTED_LABEL));
        assert_eq!(rework[0].duration_sec, Some(1_800));
    }

    /// The other re-judge shape: an approval invalidated with no change
    /// request against it — the base moved, or #8248's freshness guard fired.
    /// It is **environmental**, and telling it apart from the row above is the
    /// whole point of this module.
    #[test]
    fn an_approval_superseded_with_no_change_request_is_environmental() {
        let rework = rework_from_events(&events(&[
            ("2026-09-18T12:00:00Z", REVIEW_REQUESTED_LABEL),
            ("2026-09-18T12:30:00Z", APPROVED_LABEL),
            ("2026-09-18T14:30:00Z", REVIEW_REQUESTED_LABEL),
            ("2026-09-18T15:00:00Z", APPROVED_LABEL),
        ]));
        assert_eq!(rework.len(), 1);
        assert_eq!(rework[0].kind, ReworkKind::StaleBaseRejudge);
        assert_eq!(rework[0].classification, ReworkClass::Environmental);
        assert_eq!(rework[0].reason.as_deref(), Some(REASON_APPROVAL_SUPERSEDED));
        assert_eq!(rework[0].duration_sec, Some(7_200));
    }

    /// A change request arriving after an approval supersedes it, so the
    /// hand-back that follows is charged to the work, not to the environment.
    #[test]
    fn a_change_request_after_an_approval_wins_over_the_stale_base_shape() {
        let rework = rework_from_events(&events(&[
            ("2026-09-18T12:00:00Z", REVIEW_REQUESTED_LABEL),
            ("2026-09-18T12:30:00Z", APPROVED_LABEL),
            ("2026-09-18T12:40:00Z", CHANGES_REQUESTED_LABEL),
            ("2026-09-18T13:40:00Z", REVIEW_REQUESTED_LABEL),
        ]));
        assert_eq!(rework.len(), 1);
        assert_eq!(rework[0].kind, ReworkKind::Rejudge);
        assert_eq!(rework[0].duration_sec, Some(3_600));
    }

    /// AC1 (event half): a PR that conflicted with its moved base carries a
    /// `merge_conflict` rework event, classified `environmental`, measured
    /// from the label's arrival to the re-review that cleared it.
    #[test]
    fn a_merge_conflict_is_an_environmental_event_measured_to_its_clearing() {
        let rework = rework_from_events(&events(&[
            ("2026-09-18T12:00:00Z", REVIEW_REQUESTED_LABEL),
            ("2026-09-18T12:30:00Z", MERGE_CONFLICT_LABEL),
            ("2026-09-18T13:00:00Z", REVIEW_REQUESTED_LABEL),
        ]));
        assert_eq!(rework.len(), 1);
        assert_eq!(rework[0].kind, ReworkKind::MergeConflict);
        assert_eq!(rework[0].classification, ReworkClass::Environmental);
        assert_eq!(rework[0].duration_sec, Some(1_800));
    }

    /// A CI failure is the same shape, and an approval clears it just as a
    /// re-review does.
    #[test]
    fn a_ci_failure_is_environmental_and_an_approval_closes_it() {
        let rework = rework_from_events(&events(&[
            ("2026-09-18T12:00:00Z", REVIEW_REQUESTED_LABEL),
            ("2026-09-18T12:10:00Z", "loom:ci-failure"),
            ("2026-09-18T12:25:00Z", APPROVED_LABEL),
        ]));
        assert_eq!(rework.len(), 1);
        assert_eq!(rework[0].kind, ReworkKind::CiRerun);
        assert_eq!(rework[0].classification, ReworkClass::Environmental);
        assert_eq!(rework[0].duration_sec, Some(900));
    }

    /// Still conflicted when the sweep ended: the rework is reported, its
    /// duration is not invented.
    #[test]
    fn an_unresolved_conflict_is_reported_without_a_duration() {
        let rework = rework_from_events(&events(&[
            ("2026-09-18T12:00:00Z", REVIEW_REQUESTED_LABEL),
            ("2026-09-18T12:30:00Z", MERGE_CONFLICT_LABEL),
        ]));
        assert_eq!(rework.len(), 1);
        assert_eq!(rework[0].kind, ReworkKind::MergeConflict);
        assert_eq!(rework[0].duration_sec, None, "never a fabricated 0");
    }

    /// A label removed and re-applied while the rework is still open is one
    /// rework, not two — the same dedupe the verdict list applies.
    #[test]
    fn a_reapplied_label_is_one_rework() {
        let rework = rework_from_events(&events(&[
            ("2026-09-18T12:00:00Z", MERGE_CONFLICT_LABEL),
            ("2026-09-18T12:05:00Z", MERGE_CONFLICT_LABEL),
            ("2026-09-18T12:30:00Z", REVIEW_REQUESTED_LABEL),
        ]));
        assert_eq!(rework.len(), 1);
        assert_eq!(rework[0].duration_sec, Some(1_800), "measured from the FIRST arrival");
    }

    /// A rejection nobody handed back is a verdict, not rework — the #8222
    /// Doctor-cycle rule, mirrored so the two fields can never disagree.
    #[test]
    fn a_terminal_rejection_is_not_a_rework_event() {
        let rework = rework_from_events(&events(&[
            ("2026-09-18T12:00:00Z", REVIEW_REQUESTED_LABEL),
            ("2026-09-18T12:30:00Z", CHANGES_REQUESTED_LABEL),
        ]));
        assert!(rework.is_empty());
    }

    /// The contract stated in the module doc, over a set of timelines that
    /// exercises every arm: `rejudge` events and `doctor_cycles` are the same
    /// count, always.
    #[test]
    fn rejudge_events_always_agree_with_the_doctor_cycle_count() {
        let timelines: Vec<Vec<(&str, &str)>> = vec![
            vec![],
            vec![("2026-09-18T12:00:00Z", REVIEW_REQUESTED_LABEL)],
            vec![
                ("2026-09-18T12:00:00Z", REVIEW_REQUESTED_LABEL),
                ("2026-09-18T12:30:00Z", APPROVED_LABEL),
            ],
            vec![
                ("2026-09-18T12:00:00Z", REVIEW_REQUESTED_LABEL),
                ("2026-09-18T12:30:00Z", CHANGES_REQUESTED_LABEL),
            ],
            vec![
                ("2026-09-18T12:00:00Z", REVIEW_REQUESTED_LABEL),
                ("2026-09-18T12:30:00Z", CHANGES_REQUESTED_LABEL),
                ("2026-09-18T13:00:00Z", REVIEW_REQUESTED_LABEL),
                ("2026-09-18T13:30:00Z", CHANGES_REQUESTED_LABEL),
                ("2026-09-18T14:00:00Z", REVIEW_REQUESTED_LABEL),
                ("2026-09-18T14:30:00Z", APPROVED_LABEL),
            ],
            vec![
                ("2026-09-18T12:00:00Z", REVIEW_REQUESTED_LABEL),
                ("2026-09-18T12:10:00Z", MERGE_CONFLICT_LABEL),
                ("2026-09-18T12:20:00Z", "loom:ci-failure"),
                ("2026-09-18T12:30:00Z", CHANGES_REQUESTED_LABEL),
                ("2026-09-18T13:00:00Z", REVIEW_REQUESTED_LABEL),
                ("2026-09-18T13:30:00Z", APPROVED_LABEL),
                ("2026-09-18T15:30:00Z", REVIEW_REQUESTED_LABEL),
            ],
        ];
        for rows in timelines {
            let parsed = events(&rows);
            let rejudges = rework_from_events(&parsed)
                .into_iter()
                .filter(|event| event.kind == ReworkKind::Rejudge)
                .count();
            let cycles = signals_from_events(&parsed).doctor_cycles as usize;
            assert_eq!(rejudges, cycles, "{rows:?}");
        }
    }

    /// Everything at once, in opening order, with both classes present — the
    /// shape a per-issue effort query splits.
    #[test]
    fn a_mixed_timeline_reports_every_rework_in_opening_order() {
        let rework = rework_from_events(&events(&[
            ("2026-09-18T12:00:00Z", REVIEW_REQUESTED_LABEL),
            ("2026-09-18T12:10:00Z", "loom:ci-failure"),
            ("2026-09-18T12:20:00Z", MERGE_CONFLICT_LABEL),
            ("2026-09-18T12:30:00Z", CHANGES_REQUESTED_LABEL),
            ("2026-09-18T13:00:00Z", REVIEW_REQUESTED_LABEL),
            ("2026-09-18T13:30:00Z", APPROVED_LABEL),
            ("2026-09-18T15:30:00Z", REVIEW_REQUESTED_LABEL),
        ]));
        assert_eq!(
            rework.iter().map(|e| e.kind).collect::<Vec<_>>(),
            vec![
                ReworkKind::CiRerun,
                ReworkKind::MergeConflict,
                ReworkKind::Rejudge,
                ReworkKind::StaleBaseRejudge,
            ]
        );
        assert_eq!(
            rework
                .iter()
                .filter(|e| e.classification == ReworkClass::Environmental)
                .count(),
            3
        );
        assert_eq!(
            rework
                .iter()
                .filter(|e| e.classification == ReworkClass::Substantive)
                .count(),
            1
        );
    }

    /// Unrelated labels never open a rework.
    #[test]
    fn unrelated_labels_are_ignored() {
        assert!(rework_from_events(&events(&[
            ("2026-09-18T12:00:00Z", "loom:treating"),
            ("2026-09-18T12:01:00Z", "tier:goal-supporting"),
            ("2026-09-18T12:02:00Z", "loom:building"),
        ]))
        .is_empty());
    }

    /// A pathologically flapping timeline cannot grow the record without
    /// bound, exactly as the verdict list is capped.
    #[test]
    fn rework_events_are_capped() {
        let base = DateTime::parse_from_rfc3339("2026-09-18T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut flapping = Vec::new();
        for i in 0..(MAX_REWORK_EVENTS as i64 + 10) {
            for (offset, label) in [(0, MERGE_CONFLICT_LABEL), (1, REVIEW_REQUESTED_LABEL)] {
                flapping.push(LabelEvent {
                    at: base + chrono::Duration::minutes(i * 2 + offset),
                    label: label.to_string(),
                });
            }
        }
        assert_eq!(rework_from_events(&flapping).len(), MAX_REWORK_EVENTS);
    }

    /// Whole-second forge stamps can put two events inside one second; a
    /// negative duration is never published.
    #[test]
    fn durations_never_go_negative() {
        let at = DateTime::parse_from_rfc3339("2026-09-18T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let rework = rework_from_events(&[
            LabelEvent {
                at,
                label: MERGE_CONFLICT_LABEL.to_string(),
            },
            LabelEvent {
                at,
                label: REVIEW_REQUESTED_LABEL.to_string(),
            },
        ]);
        assert_eq!(rework[0].duration_sec, Some(0));
    }
}
