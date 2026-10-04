//! `fleet_state(t)`: the whole fleet of one repo at an instant, reconstructed
//! from the raw event cache (#10197).
//!
//! Pure: [`fleet_state`] takes already-loaded [`RawEvent`]s and an instant and
//! touches nothing else, so it is deterministic and host-independent in the
//! same sense as [`super::fleet::FleetSnapshot`] — two hosts holding the same
//! events file produce the same bytes for the same `t`.
//!
//! # Leak-freedom
//!
//! The first thing [`fleet_state`] does is drop every event with
//! `event_time >= t`. Nothing after that line can see them, so a cache
//! fetched today replays any earlier instant without leaking its own future
//! (#10193's hard requirement). Events are then replayed in the canonical
//! `(event_time, source, seq, id)` order, so same-second events resolve the
//! same way on every run.
//!
//! # Stages
//!
//! Stage names follow `.loom/docs/label-state-machine.md` and
//! [`super::labels`]. One addition: an approved PR (`loom:pr`) that also
//! carries an operator label is [`ItemStage::HeldForHuman`], not `merge_wait`
//! — it waits on a person, not a queue, and is a large share of open-PR time
//! (#10218 builds the estimator for it; this only exposes the stage).
//!
//! # What PR 1 cannot see yet
//!
//! `pr_open_skip_lockout` needs each open PR's closing references, which the
//! issue-events listing does not carry; it is `null` until #10197 PR 2 adds
//! that fetcher. Items with no event inside the cache's window are invisible:
//! an item untouched since before the window is not reported as open.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use super::fleet_events::{EventKind, ItemKind, RawEvent};
use super::labels::{check_holds, stage_from_pr_labels, APPROVED, READY_LABEL};
use super::Stage;
use crate::work_finder::BUILDING_LABEL;

/// Schema tag of a [`FleetState`].
pub const STATE_SCHEMA: &str = "eta-fleet-state/v1";

/// Labels that put an item on a human: the operator escalation and its
/// sub-kinds. `loom:operator-priority` is the operator's star, not a hold, and
/// is deliberately absent.
pub const OPERATOR_HOLD_LABELS: &[&str] = &[
    "loom:operator",
    "loom:operator-only",
    "loom:operator-blocked",
    "loom:operator-mechanical",
    "loom:operator-decision",
    "loom:operator-objective",
];

/// Where an open item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStage {
    /// Issue with no Loom stage label yet.
    Untriaged,
    Triage,
    Curating,
    Curated,
    /// `loom:issue`: approved, waiting for a dispatch slot.
    ReadyWait,
    /// `loom:building`: a sweep holds it.
    Building,
    ReviewWait,
    Doctor,
    MergeWait,
    /// Approved PR also carrying an operator label.
    HeldForHuman,
    /// A hold or park label (including the operator labels on an issue).
    Blocked,
    /// A PR whose labels name no single post-sweep stage.
    Unknown,
}

impl ItemStage {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ItemStage::Untriaged => "untriaged",
            ItemStage::Triage => "triage",
            ItemStage::Curating => "curating",
            ItemStage::Curated => "curated",
            ItemStage::ReadyWait => "ready_wait",
            ItemStage::Building => "building",
            ItemStage::ReviewWait => "review_wait",
            ItemStage::Doctor => "doctor",
            ItemStage::MergeWait => "merge_wait",
            ItemStage::HeldForHuman => "held_for_human",
            ItemStage::Blocked => "blocked",
            ItemStage::Unknown => "unknown",
        }
    }
}

fn has(labels: &BTreeSet<String>, wanted: &str) -> bool {
    labels.contains(wanted)
}

/// Whether `labels` carry any [`OPERATOR_HOLD_LABELS`] entry.
#[must_use]
pub fn operator_hold(labels: &BTreeSet<String>) -> bool {
    OPERATOR_HOLD_LABELS.iter().any(|l| has(labels, l))
}

/// The stage an open item with `labels` is in.
#[must_use]
pub fn item_stage(kind: ItemKind, labels: &BTreeSet<String>) -> ItemStage {
    let list: Vec<String> = labels.iter().cloned().collect();
    match kind {
        ItemKind::Pr => {
            if has(labels, APPROVED) && operator_hold(labels) {
                return ItemStage::HeldForHuman;
            }
            match stage_from_pr_labels(&list) {
                Ok(Stage::ReviewWait) => ItemStage::ReviewWait,
                Ok(Stage::Doctor) => ItemStage::Doctor,
                Ok(Stage::MergeWait) => ItemStage::MergeWait,
                Ok(_) => ItemStage::Unknown,
                Err(super::NoEstimateReason::Blocked) => ItemStage::Blocked,
                Err(_) => ItemStage::Unknown,
            }
        }
        ItemKind::Issue => {
            if check_holds(&list).is_err() || operator_hold(labels) {
                ItemStage::Blocked
            } else if has(labels, BUILDING_LABEL) {
                ItemStage::Building
            } else if has(labels, READY_LABEL) {
                ItemStage::ReadyWait
            } else if has(labels, "loom:curated") {
                ItemStage::Curated
            } else if has(labels, "loom:curating") {
                ItemStage::Curating
            } else if has(labels, "loom:triage") {
                ItemStage::Triage
            } else {
                ItemStage::Untriaged
            }
        }
    }
}

/// One open item at `as_of`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemState {
    pub number: u32,
    pub kind: ItemKind,
    /// Labels held at `as_of`, ascending.
    pub labels: Vec<String>,
    pub stage: ItemStage,
    /// When the item entered `stage` (its last stage change before `as_of`).
    pub stage_entered_at: DateTime<Utc>,
    /// `as_of - stage_entered_at`, whole seconds.
    pub time_in_stage_sec: i64,
    /// The item's creation time when the cache holds it, else its first event.
    pub opened_at: DateTime<Utc>,
    /// Carries an operator label.
    pub operator_hold: bool,
}

/// The whole fleet of one repo at one instant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetState {
    /// Always [`STATE_SCHEMA`].
    pub schema: String,
    /// `owner/repo`.
    pub repo: String,
    /// The instant described. Only events strictly before it were read.
    pub as_of: DateTime<Utc>,
    /// Events read (those for `repo` with `event_time < as_of`).
    pub events_read: usize,
    pub open_issues: usize,
    pub open_prs: usize,
    /// Open items labelled `loom:building` — the slots-in-use proxy.
    pub building: usize,
    /// Open items carrying an operator label.
    pub operator_holds: usize,
    /// Open PRs in [`ItemStage::HeldForHuman`].
    pub held_for_human: usize,
    /// Whether the repo's ready queue was frozen behind the open-PR guard.
    /// `null` until closing references are cached (#10197 PR 2).
    pub pr_open_skip_lockout: Option<bool>,
    /// Open items per stage name.
    pub stage_counts: BTreeMap<String, usize>,
    /// Every open item, by `(kind, number)`.
    pub items: Vec<ItemState>,
}

#[derive(Default)]
struct Replay {
    kind: Option<ItemKind>,
    open: bool,
    opened_at: Option<DateTime<Utc>>,
    first_seen: Option<DateTime<Utc>>,
    labels: BTreeSet<String>,
    stage: Option<ItemStage>,
    stage_entered_at: Option<DateTime<Utc>>,
}

/// `repo`'s fleet at `as_of`, from `events` (any order, any repos).
#[must_use]
pub fn fleet_state(events: &[RawEvent], repo: &str, as_of: DateTime<Utc>) -> FleetState {
    // The leak-freedom line: nothing at or after `as_of` survives it.
    let mut known: Vec<&RawEvent> = events
        .iter()
        .filter(|e| e.event_time < as_of && e.repo.eq_ignore_ascii_case(repo))
        .collect();
    known.sort_by(|a, b| a.canonical_key().cmp(&b.canonical_key()));
    known.dedup_by(|a, b| a.id == b.id);

    let mut items: BTreeMap<(ItemKind, u32), Replay> = BTreeMap::new();
    for event in &known {
        let item = items.entry((event.item_kind, event.item)).or_default();
        item.kind = Some(event.item_kind);
        if item.first_seen.is_none() {
            item.first_seen = Some(event.event_time);
            item.open = true;
        }
        match event.kind {
            EventKind::Opened => {
                item.opened_at = Some(event.event_time);
            }
            EventKind::LabelAdded => {
                if let Some(label) = &event.label {
                    item.labels.insert(label.clone());
                }
            }
            EventKind::LabelRemoved => {
                if let Some(label) = &event.label {
                    item.labels.remove(label);
                }
            }
            EventKind::Closed | EventKind::Merged => item.open = false,
            EventKind::Reopened => item.open = true,
            EventKind::Review | EventKind::CheckRun => {}
        }
        let stage = item_stage(event.item_kind, &item.labels);
        if item.stage != Some(stage) || matches!(event.kind, EventKind::Reopened) {
            item.stage = Some(stage);
            item.stage_entered_at = Some(event.event_time);
        }
    }

    let mut state = FleetState {
        schema: STATE_SCHEMA.to_string(),
        repo: repo.to_string(),
        as_of,
        events_read: known.len(),
        open_issues: 0,
        open_prs: 0,
        building: 0,
        operator_holds: 0,
        held_for_human: 0,
        pr_open_skip_lockout: None,
        stage_counts: BTreeMap::new(),
        items: Vec::new(),
    };
    for ((kind, number), item) in items {
        let (Some(stage), Some(entered), Some(first)) =
            (item.stage, item.stage_entered_at, item.first_seen)
        else {
            continue;
        };
        if !item.open {
            continue;
        }
        let hold = operator_hold(&item.labels);
        match kind {
            ItemKind::Issue => state.open_issues += 1,
            ItemKind::Pr => state.open_prs += 1,
        }
        if has(&item.labels, BUILDING_LABEL) {
            state.building += 1;
        }
        if hold {
            state.operator_holds += 1;
        }
        if stage == ItemStage::HeldForHuman {
            state.held_for_human += 1;
        }
        *state
            .stage_counts
            .entry(stage.as_str().to_string())
            .or_default() += 1;
        state.items.push(ItemState {
            number,
            kind,
            labels: item.labels.into_iter().collect(),
            stage,
            stage_entered_at: entered,
            time_in_stage_sec: (as_of - entered).num_seconds(),
            opened_at: item.opened_at.unwrap_or(first),
            operator_hold: hold,
        });
    }
    state
}

#[cfg(test)]
#[path = "fleet_state_tests.rs"]
mod tests;
