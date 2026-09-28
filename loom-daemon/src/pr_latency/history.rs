//! One PR's normalized event log — the only input every segment is derived
//! from (Issue #8923).
//!
//! Deliberately free of forge access so the whole derivation is unit-testable:
//! `cli/pr_latency_cmd.rs` does the `gh` reads and builds these values.

use chrono::{DateTime, Utc};

/// Terminal state of a PR, as far as latency is concerned.
///
/// `Closed` is kept apart from `Merged` because an abandoned PR's segments are
/// not slow merges — folding them together is how a closed-unmerged PR
/// silently becomes a 200-hour outlier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    Open,
    Merged,
    Closed,
}

impl PrState {
    /// The `gh pr list --json state` vocabulary (`OPEN`/`MERGED`/`CLOSED`).
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_uppercase().as_str() {
            "MERGED" => Self::Merged,
            "CLOSED" => Self::Closed,
            _ => Self::Open,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Merged => "merged",
            Self::Closed => "closed",
        }
    }
}

/// One thing that happened to a PR, at a known time.
///
/// Only the four kinds any segment needs. Anything else on the timeline is
/// dropped at parse time rather than carried as an `Other` variant nobody
/// reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrEvent {
    /// A label was applied.
    Labeled { label: String, at: DateTime<Utc> },
    /// A label was removed.
    Unlabeled { label: String, at: DateTime<Utc> },
    /// New commits reached the head branch (a `committed` or
    /// `head_ref_force_pushed` timeline event).
    ///
    /// **Caveat, stated once here.** REST's issue timeline has no "pushed"
    /// event; a `committed` entry carries the *commit object's* committer date,
    /// which is when the commit was written, not when it reached the forge. For
    /// an agent that commits and immediately pushes — every Loom role — the two
    /// coincide to within seconds. For a human who commits locally and pushes
    /// the next day it does not, and the derived Doctor-response time will read
    /// low. `head_ref_force_pushed` *is* a true push time and is used as-is.
    Pushed { at: DateTime<Utc> },
    /// The PR was merged.
    Merged { at: DateTime<Utc> },
}

impl PrEvent {
    pub fn at(&self) -> DateTime<Utc> {
        match self {
            Self::Labeled { at, .. }
            | Self::Unlabeled { at, .. }
            | Self::Pushed { at }
            | Self::Merged { at } => *at,
        }
    }

    /// The label this event moved, if it is a label event.
    pub fn label(&self) -> Option<&str> {
        match self {
            Self::Labeled { label, .. } | Self::Unlabeled { label, .. } => Some(label.as_str()),
            _ => None,
        }
    }

    fn is_labeled(&self, label: &str) -> bool {
        matches!(self, Self::Labeled { label: l, .. } if l == label)
    }
}

/// Everything known about one PR, ready for segment derivation.
#[derive(Debug, Clone)]
pub struct PrHistory {
    pub number: u32,
    pub created_at: DateTime<Utc>,
    pub state: PrState,
    /// Present only when [`PrState::Merged`].
    pub merged_at: Option<DateTime<Utc>>,
    /// Labels on the PR **now**. Authoritative for the live queue view; the
    /// event log is authoritative for history.
    pub current_labels: Vec<String>,
    /// Chronologically sorted. [`PrHistory::new`] sorts; nothing downstream
    /// re-sorts, and every traversal below relies on the order.
    pub events: Vec<PrEvent>,
    /// `false` when the timeline read did not fully answer (a page failed, the
    /// request errored). Every segment derived from a partial log is suspect,
    /// so such a PR is reported as *unmeasured*, never as a fast one.
    pub timeline_complete: bool,
}

impl PrHistory {
    pub fn new(
        number: u32,
        created_at: DateTime<Utc>,
        state: PrState,
        merged_at: Option<DateTime<Utc>>,
        current_labels: Vec<String>,
        mut events: Vec<PrEvent>,
        timeline_complete: bool,
    ) -> Self {
        events.sort_by_key(PrEvent::at);
        Self {
            number,
            created_at,
            state,
            merged_at,
            current_labels,
            events,
            timeline_complete,
        }
    }

    /// Time of the first `labeled <label>` event, if any.
    pub fn first_labeled(&self, label: &str) -> Option<DateTime<Utc>> {
        self.events
            .iter()
            .find(|e| e.is_labeled(label))
            .map(PrEvent::at)
    }

    /// Time of the last `labeled <label>` event at or before `bound`.
    pub fn last_labeled_before(&self, label: &str, bound: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.events
            .iter()
            .filter(|e| e.is_labeled(label) && e.at() <= bound)
            .map(PrEvent::at)
            .next_back()
    }

    /// Every time `label` was applied, in order.
    pub fn labelings(&self, label: &str) -> Vec<DateTime<Utc>> {
        self.events
            .iter()
            .filter(|e| e.is_labeled(label))
            .map(PrEvent::at)
            .collect()
    }

    /// Time of the first verdict of any kind ([`super::VERDICT_LABELS`]).
    ///
    /// A verdict *removal* is not a verdict: only `labeled` events count, which
    /// is what makes an invalidation (`unlabeled loom:pr` →
    /// `labeled loom:review-requested`) legible as a second lap rather than a
    /// second verdict.
    pub fn first_verdict(&self) -> Option<DateTime<Utc>> {
        self.events
            .iter()
            .find(|e| super::VERDICT_LABELS.iter().any(|l| e.is_labeled(l)))
            .map(PrEvent::at)
    }

    /// First verdict labeling strictly after `after`.
    pub fn next_verdict_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.events
            .iter()
            .filter(|e| e.at() > after)
            .find(|e| super::VERDICT_LABELS.iter().any(|l| e.is_labeled(l)))
            .map(PrEvent::at)
    }

    /// First push strictly after `after`.
    pub fn next_push_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.events
            .iter()
            .filter(|e| e.at() > after)
            .find(|e| matches!(e, PrEvent::Pushed { .. }))
            .map(PrEvent::at)
    }

    /// Whether `label` was present at instant `at`, by replaying the log.
    ///
    /// Used instead of "was it ever applied" so a gate that was applied and
    /// then *cleared* before the merge is not counted as gating it.
    pub fn label_present_at(&self, label: &str, at: DateTime<Utc>) -> bool {
        let mut present = false;
        for e in &self.events {
            if e.at() > at {
                break;
            }
            match e {
                PrEvent::Labeled { label: l, .. } if l == label => present = true,
                PrEvent::Unlabeled { label: l, .. } if l == label => present = false,
                _ => {}
            }
        }
        present
    }

    /// Whether any operator gate was in force at any point in
    /// `[from, to]` — applied inside the window, or already present when it
    /// opened.
    pub fn operator_gated_between(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> bool {
        super::OPERATOR_GATE_LABELS.iter().any(|gate| {
            self.label_present_at(gate, from)
                || self
                    .events
                    .iter()
                    .any(|e| e.is_labeled(gate) && e.at() >= from && e.at() <= to)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod fixtures {
    use super::*;

    /// `t` seconds after a fixed epoch, so test expectations are exact.
    pub fn t(secs: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + chrono::Duration::seconds(secs)
    }

    pub fn labeled(label: &str, secs: i64) -> PrEvent {
        PrEvent::Labeled {
            label: label.into(),
            at: t(secs),
        }
    }

    pub fn unlabeled(label: &str, secs: i64) -> PrEvent {
        PrEvent::Unlabeled {
            label: label.into(),
            at: t(secs),
        }
    }

    pub fn pushed(secs: i64) -> PrEvent {
        PrEvent::Pushed { at: t(secs) }
    }

    /// A merged PR with the given event log.
    pub fn merged(number: u32, merged_secs: i64, events: Vec<PrEvent>) -> PrHistory {
        let mut events = events;
        events.push(PrEvent::Merged { at: t(merged_secs) });
        PrHistory::new(
            number,
            t(0),
            PrState::Merged,
            Some(t(merged_secs)),
            Vec::new(),
            events,
            true,
        )
    }

    /// An open PR with the given current labels and event log.
    pub fn open(number: u32, labels: &[&str], events: Vec<PrEvent>) -> PrHistory {
        PrHistory::new(
            number,
            t(0),
            PrState::Open,
            None,
            labels.iter().map(|s| (*s).to_string()).collect(),
            events,
            true,
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::fixtures::*;
    use crate::pr_latency::{APPROVED, CHANGES_REQUESTED, REVIEW_REQUESTED};

    #[test]
    fn events_are_sorted_on_construction() {
        let h = open(1, &[], vec![labeled(APPROVED, 500), labeled(REVIEW_REQUESTED, 100)]);
        assert_eq!(h.events[0].at(), t(100));
        assert_eq!(h.events[1].at(), t(500));
    }

    #[test]
    fn label_present_at_replays_add_and_remove() {
        let h = open(
            1,
            &[],
            vec![
                labeled("loom:operator", 100),
                unlabeled("loom:operator", 300),
            ],
        );
        assert!(!h.label_present_at("loom:operator", t(50)));
        assert!(h.label_present_at("loom:operator", t(200)));
        assert!(!h.label_present_at("loom:operator", t(400)));
    }

    #[test]
    fn a_gate_cleared_before_the_window_does_not_gate_it() {
        let h = merged(
            1,
            900,
            vec![
                labeled("loom:operator", 100),
                unlabeled("loom:operator", 200),
                labeled(APPROVED, 300),
            ],
        );
        assert!(!h.operator_gated_between(t(300), t(900)));
        // Still present when the window opens => gated.
        let h2 = merged(2, 900, vec![labeled("loom:operator", 100), labeled(APPROVED, 300)]);
        assert!(h2.operator_gated_between(t(300), t(900)));
        // Applied inside the window => gated.
        let h3 = merged(3, 900, vec![labeled(APPROVED, 300), labeled("loom:operator", 400)]);
        assert!(h3.operator_gated_between(t(300), t(900)));
    }

    #[test]
    fn first_verdict_takes_the_earliest_of_either_label() {
        let h = merged(1, 900, vec![labeled(CHANGES_REQUESTED, 400), labeled(APPROVED, 700)]);
        assert_eq!(h.first_verdict(), Some(t(400)));
    }

    #[test]
    fn last_labeled_before_ignores_later_labelings() {
        let h = merged(1, 900, vec![labeled(APPROVED, 300), labeled(APPROVED, 800)]);
        assert_eq!(h.last_labeled_before(APPROVED, t(500)), Some(t(300)));
        assert_eq!(h.last_labeled_before(APPROVED, t(900)), Some(t(800)));
    }
}
