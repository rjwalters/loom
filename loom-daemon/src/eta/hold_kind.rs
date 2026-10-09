//! The one definition of *which* hold a PR is under at an instant, and of
//! *who* released a hold (#10958, Slice 1).
//!
//! [`super::labels::FLAG_OP_HOLD`] collapses every operator hold into one
//! bit. This module splits it, point-in-time, from two logs the daemon
//! already keeps on disk: the raw event cache's PR label rows
//! ([`super::fleet_events`]) and the Champion marker log
//! ([`super::hold_marker_log`]). It is pure: no clock, no forge.
//!
//! # Spells
//!
//! A PR's label rows strictly before `cutoff` are replayed in canonical order
//! from an empty set; the hold labels ([`is_hold_label`]) in force are
//! resolved once per instant. A **spell** opens when that set becomes
//! non-empty and ends when it empties (a release) or the PR merges or closes
//! while held. A label added while in force opens nothing.
//!
//! # Kind
//!
//! The labels give a [`HoldKind`] on their own ([`label_kind`]). The latest
//! trusted Champion marker posted in the spell (from [`MARKER_SLACK_SEC`]
//! before its start, since Champion posts its notice and its label in either
//! order) refines it: a hold marker (`merge-risk-hold`, `critical-file-hold`,
//! `ac-hold`) names the kind; a release marker followed by a label still in
//! force means a human put the hold back, so the label kind stands. A re-arm
//! at a new head is a newer hold marker, and the spell takes its head.
//! When the marker log does not cover the spell, the kind is the label kind
//! and `marker_known` is false: an uncovered span is unknown, never "no
//! marker".
//!
//! # Release
//!
//! A spell that ended by its labels emptying at `r` was released by
//! **Champion** when a `-cleared` marker was posted within
//! [`CLOSER_WINDOW_SEC`] of `r`, else by a **human** (the inference
//! `champion-critical-file-hold.md` relies on: an open episode whose label is
//! gone can only mean a human removed it). The verdict is knowable only once
//! the window has passed with coverage through it; before that it is
//! [`ReleaseBy::Pending`], so a closer posted after `cutoff` can never change
//! what a row at `cutoff` saw. No forge `actor` is read: on a host whose
//! writes use a person's token, fleet and operator writes share one login.
//!
//! # Knowable-at
//!
//! A label row is usable iff `event_time < cutoff`, a marker iff
//! `created_at < cutoff`. Training passes `cutoff = t − LAG`, serving
//! `cutoff = as_of`, as [`super::star`] does.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;

use super::fleet_events::{EventKind, ItemKind, RawEvent};
use super::hold_marker_log::{self, Coverage, HoldMarker, MarkerCursor, MarkerKind};
use crate::observability::queue_blocked::BLOCKED_LABEL;

/// How long before a spell's first label a marker may be posted and still
/// belong to it.
pub const MARKER_SLACK_SEC: i64 = 30 * 60;

/// How close to a release a `-cleared` marker must be for Champion to have
/// released it.
pub const CLOSER_WINDOW_SEC: i64 = 15 * 60;

/// `loom:operator`.
pub const OPERATOR: &str = "loom:operator";
/// `loom:operator-only`.
pub const OPERATOR_ONLY: &str = "loom:operator-only";
/// `loom:operator-decision`.
pub const OPERATOR_DECISION: &str = "loom:operator-decision";
/// `loom:operator-mechanical`: an `operator-only` sub-kind.
pub const OPERATOR_MECHANICAL: &str = "loom:operator-mechanical";

/// The kind of a hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldKind {
    /// `loom:operator`, no Champion hold marker.
    Operator,
    /// `loom:operator-only` (or `-mechanical`).
    OperatorOnly,
    /// `loom:operator-decision`.
    OperatorDecision,
    /// Champion's merge-risk hold (criterion #2).
    MergeRisk,
    /// Champion's critical-file hold (criterion #3).
    CriticalFile,
    /// Champion's acceptance-criteria hold.
    AcHold,
    /// `loom:blocked` alone.
    Blocked,
    /// A hold label set none of the above names.
    Other,
}

impl HoldKind {
    /// Every kind, in one-hot order.
    pub const ALL: [HoldKind; 8] = [
        HoldKind::Operator,
        HoldKind::OperatorOnly,
        HoldKind::OperatorDecision,
        HoldKind::MergeRisk,
        HoldKind::CriticalFile,
        HoldKind::AcHold,
        HoldKind::Blocked,
        HoldKind::Other,
    ];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            HoldKind::Operator => "operator",
            HoldKind::OperatorOnly => "operator_only",
            HoldKind::OperatorDecision => "operator_decision",
            HoldKind::MergeRisk => "merge_risk",
            HoldKind::CriticalFile => "critical_file",
            HoldKind::AcHold => "ac_hold",
            HoldKind::Blocked => "blocked",
            HoldKind::Other => "other",
        }
    }

    /// The kind a hold marker names.
    #[must_use]
    pub fn of_marker(kind: MarkerKind) -> Option<Self> {
        match kind {
            MarkerKind::MergeRiskHold => Some(HoldKind::MergeRisk),
            MarkerKind::CriticalFileHold => Some(HoldKind::CriticalFile),
            MarkerKind::AcHold => Some(HoldKind::AcHold),
            _ => None,
        }
    }
}

/// Whether `label` holds a PR for a human: a `merge_hold` label, its
/// companions, or `loom:blocked`.
#[must_use]
pub fn is_hold_label(label: &str) -> bool {
    use super::labels::{MERGE_HOLD_COMPANION_LABELS, MERGE_HOLD_LABELS};
    label == BLOCKED_LABEL
        || MERGE_HOLD_LABELS.contains(&label)
        || MERGE_HOLD_COMPANION_LABELS.contains(&label)
}

/// The kind the hold labels in `set` name on their own, or `None` when the
/// set holds nothing.
#[must_use]
pub fn label_kind(set: &BTreeSet<String>) -> Option<HoldKind> {
    let has = |l: &str| set.contains(l);
    if set.is_empty() {
        None
    } else if has(OPERATOR_DECISION) {
        Some(HoldKind::OperatorDecision)
    } else if has(OPERATOR_ONLY) || has(OPERATOR_MECHANICAL) {
        Some(HoldKind::OperatorOnly)
    } else if has(OPERATOR) {
        Some(HoldKind::Operator)
    } else if has(BLOCKED_LABEL) {
        Some(HoldKind::Blocked)
    } else {
        Some(HoldKind::Other)
    }
}

/// Who released a hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseBy {
    /// A `-cleared` marker within [`CLOSER_WINDOW_SEC`] of the release.
    Champion,
    /// No such marker, the window passed with the marker log covering it.
    Human,
    /// Not yet decidable at `cutoff` (window open, or not covered).
    Pending,
}

/// How a spell ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "end")]
pub enum SpellEnd {
    /// Its hold labels were removed.
    Released { at: DateTime<Utc>, by: ReleaseBy },
    /// The PR merged while held.
    Merged { at: DateTime<Utc> },
    /// The PR closed unmerged while held.
    Closed { at: DateTime<Utc> },
}

/// One hold spell as known at a cutoff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HoldSpell {
    /// When its first hold label was added.
    pub start: DateTime<Utc>,
    /// The kind its labels name (in force at its end, or at the cutoff).
    pub label_kind: HoldKind,
    /// The kind, refined by the markers when they cover the spell.
    pub kind: HoldKind,
    /// The marker log covers the spell, so `kind` used it.
    pub marker_known: bool,
    /// The head the latest hold marker names.
    pub head: Option<String>,
    /// `None` while the spell is open at the cutoff.
    pub end: Option<SpellEnd>,
}

/// The inputs for one repo: its PR rows from the raw event cache, its marker
/// rows and the marker log's coverage.
#[derive(Debug, Clone, Default)]
pub struct RepoHolds {
    /// PR label, merge and close rows, in canonical order.
    pub events: Vec<RawEvent>,
    /// Marker rows, in `(created_at, comment_id, kind)` order.
    pub markers: Vec<HoldMarker>,
    pub coverage: Coverage,
}

impl RepoHolds {
    /// Build from a repo's raw events (any order, other kinds dropped), its
    /// marker rows and coverage.
    #[must_use]
    pub fn new(events: &[RawEvent], markers: &[HoldMarker], coverage: Coverage) -> Self {
        let mut events: Vec<RawEvent> = events
            .iter()
            .filter(|e| {
                e.item_kind == ItemKind::Pr
                    && matches!(
                        e.kind,
                        EventKind::LabelAdded
                            | EventKind::LabelRemoved
                            | EventKind::Merged
                            | EventKind::Closed
                            | EventKind::Reopened
                    )
            })
            .cloned()
            .collect();
        events.sort_by(|a, b| a.canonical_key().cmp(&b.canonical_key()));
        let mut markers = markers.to_vec();
        markers.sort_by(|a, b| {
            (a.created_at, a.comment_id, a.kind).cmp(&(b.created_at, b.comment_id, b.kind))
        });
        Self {
            events,
            markers,
            coverage,
        }
    }

    /// Read `repo` from disk under `root`: its events file, its rows of the
    /// marker log and its cursor's coverage. Addressed by slug only.
    #[must_use]
    pub fn load(root: &Path, repo: &str) -> Self {
        use super::fleet_events::{events_path, load_events};
        let events = load_events(&events_path(root, repo));
        let markers = hold_marker_log::for_repo(&hold_marker_log::load(root), repo);
        Self::new(&events, &markers, MarkerCursor::read(root).coverage(repo))
    }

    /// Every spell of `pr` that began before `cutoff`, oldest first.
    #[must_use]
    pub fn spells(&self, pr: u32, cutoff: DateTime<Utc>) -> Vec<HoldSpell> {
        let rows: Vec<&RawEvent> = self
            .events
            .iter()
            .filter(|e| e.item == pr && e.event_time < cutoff)
            .collect();
        let mut out = Vec::new();
        let mut set: BTreeSet<String> = BTreeSet::new();
        let mut open: Option<(DateTime<Utc>, HoldKind)> = None;
        let mut ended = false;
        let mut i = 0;
        while i < rows.len() {
            let at = rows[i].event_time;
            let mut terminal: Option<SpellEnd> = None;
            while i < rows.len() && rows[i].event_time == at {
                let e = rows[i];
                let label = e.label.as_deref().filter(|l| is_hold_label(l));
                match (e.kind, label) {
                    (EventKind::LabelAdded, Some(l)) => {
                        set.insert(l.to_string());
                    }
                    (EventKind::LabelRemoved, Some(l)) => {
                        set.remove(l);
                    }
                    (EventKind::Merged, _) => terminal = Some(SpellEnd::Merged { at }),
                    (EventKind::Closed, _) if terminal.is_none() => {
                        terminal = Some(SpellEnd::Closed { at });
                    }
                    (EventKind::Reopened, _) => ended = false,
                    _ => {}
                }
                i += 1;
            }
            if terminal.is_some() {
                ended = true;
            }
            let now_kind = if ended { None } else { label_kind(&set) };
            match (open, now_kind, terminal) {
                (Some((start, kind)), _, Some(end)) => {
                    out.push(self.spell(pr, start, kind, Some(end), cutoff));
                    open = None;
                }
                (Some((start, kind)), None, None) => {
                    let by = self.release_by(pr, at, cutoff);
                    out.push(self.spell(
                        pr,
                        start,
                        kind,
                        Some(SpellEnd::Released { at, by }),
                        cutoff,
                    ));
                    open = None;
                }
                (Some((start, _)), Some(k), None) => open = Some((start, k)),
                (None, Some(k), _) => open = Some((at, k)),
                (None, None, _) => {}
            }
        }
        if let Some((start, kind)) = open {
            out.push(self.spell(pr, start, kind, None, cutoff));
        }
        out
    }

    /// The spell `pr` is in at `cutoff`, or `None` when it is not held.
    #[must_use]
    pub fn hold_at(&self, pr: u32, cutoff: DateTime<Utc>) -> Option<HoldSpell> {
        self.spells(pr, cutoff).pop().filter(|s| s.end.is_none())
    }

    /// The kind of hold `pr` is under at `cutoff`, or `None` when it is not
    /// held.
    #[must_use]
    pub fn hold_kind_at(&self, pr: u32, cutoff: DateTime<Utc>) -> Option<HoldKind> {
        self.hold_at(pr, cutoff).map(|s| s.kind)
    }

    fn spell(
        &self,
        pr: u32,
        start: DateTime<Utc>,
        label_kind: HoldKind,
        end: Option<SpellEnd>,
        cutoff: DateTime<Utc>,
    ) -> HoldSpell {
        let until = match end {
            Some(
                SpellEnd::Released { at, .. } | SpellEnd::Merged { at } | SpellEnd::Closed { at },
            ) => at.min(cutoff),
            None => cutoff,
        };
        let from = start - Duration::seconds(MARKER_SLACK_SEC);
        let marker_known = self.coverage.covers(from, until);
        let latest = self
            .markers
            .iter()
            .rfind(|m| m.pr == pr && m.created_at >= from && m.created_at < until);
        let held = latest.filter(|m| marker_known && m.kind.is_hold());
        HoldSpell {
            start,
            label_kind,
            kind: held
                .and_then(|m| HoldKind::of_marker(m.kind))
                .unwrap_or(label_kind),
            marker_known,
            head: held.and_then(|m| m.head.clone()),
            end,
        }
    }

    /// Who released `pr`'s hold whose labels emptied at `at`, as known at
    /// `cutoff`.
    fn release_by(&self, pr: u32, at: DateTime<Utc>, cutoff: DateTime<Utc>) -> ReleaseBy {
        let w = Duration::seconds(CLOSER_WINDOW_SEC);
        let champion = self.markers.iter().any(|m| {
            m.pr == pr
                && m.kind.is_champion_close()
                && m.created_at < cutoff
                && m.created_at >= at - w
                && m.created_at <= at + w
        });
        if champion {
            ReleaseBy::Champion
        } else if cutoff > at + w && self.coverage.covers(at - w, at + w) {
            ReleaseBy::Human
        } else {
            ReleaseBy::Pending
        }
    }
}
