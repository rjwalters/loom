//! The `loom:very-important` flood cap, applied at dispatch (#11509).
//!
//! A label cannot be refused when it is applied, so the cap is enforced where
//! the level is read. Each tick, per workspace, only the
//! [`VERY_IMPORTANT_CAP`] oldest **open** very-important issues keep the
//! level ([`crate::priority_pick::very_important_holders`]); every other
//! very-important issue in that tick's ready listing is **demoted**: its
//! in-memory labels have `loom:very-important` replaced by `loom:important`,
//! so [`crate::priority_pick::Level::of`] (via
//! [`super::ready_queue::key_of`]) stays the one level source and the draw,
//! the in-workspace order and the published ready queue all see `important`.
//! The forge labels are never touched.
//!
//! **Only live work holds the slot (#11559).** The holder is the oldest
//! **eligible** very-important issue of the workspace: one carrying
//! `loom:issue` or `loom:building` and no `hold` label (the label registry's
//! `hold` property, `defaults/labels.json`). It is chosen over the ready
//! listing's very-important issues plus every open very-important issue
//! ([`super::WorkSource::list_open_very_important`], which still lists any
//! state), each filtered by [`holds_slot`] here. So an issue being built
//! (`loom:building`, no longer in the ready listing) keeps the slot, an open
//! PR is covered by that same `loom:building` (it stays until the closing
//! merge, #6199), and a blocked, operator-held or not-yet-approved
//! very-important issue keeps its label but does not hold the slot while it
//! cannot move. When the listing is unavailable (a forge failure, or a source
//! without one) the cap is computed over the ready listing alone: at most one
//! eligible ready issue is very-important, which is the best the tick can know.
//!
//! The cap never reads who applied the label, so it binds every actor
//! (operator, Guide, Champion) alike. Demotions are kept on the
//! [`super::TickReport`], exported in `pick.decision`
//! (`very_important_demoted`) and logged as a warning.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

use super::{TickReport, WorkItem, WorkSource};
use crate::priority_pick::{
    very_important_holders, IssueKey, Level, IMPORTANT_LABEL, VERY_IMPORTANT_CAP,
    VERY_IMPORTANT_LABEL,
};

/// One very-important issue the cap ranked as important this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViDemotion {
    /// The workspace's index in the tick.
    pub workspace_idx: usize,
    /// The demoted issue.
    pub number: u32,
    /// The issue holding the workspace's very-important slot.
    pub holder: u32,
}

fn is_very_important(item: &WorkItem) -> bool {
    item.labels.iter().any(|l| l == VERY_IMPORTANT_LABEL)
}

/// The registry's `hold` labels (#10013), derived rather than hand-listed,
/// as in `pr_latency` (see the policy note in `super::labels`).
static HOLD_SET: crate::label_registry::LabelSet =
    crate::label_registry::LabelSet::new(|| crate::label_registry::embedded_set("hold"));

/// The approved-ready label: with [`super::labels::BUILDING_LABEL`], the
/// states in which a very-important issue is live work.
const READY_LABEL: &str = "loom:issue";

/// Whether `item` is eligible to hold the very-important slot (#11559): it
/// is live (`loom:issue` or `loom:building`) and carries no `hold` label.
/// Does not check the very-important label itself.
fn holds_slot(item: &WorkItem) -> bool {
    let live = item
        .labels
        .iter()
        .any(|l| l == READY_LABEL || l == super::labels::BUILDING_LABEL);
    live && !item.labels.iter().any(|l| HOLD_SET.contains(&l.as_str()))
}

fn key(item: &WorkItem) -> IssueKey {
    IssueKey {
        number: item.number,
        created_at: item.created_at.clone(),
        level: Level::VeryImportant,
    }
}

/// Apply the cap to workspace `idx`'s `ready` listing. `open_vi` is every
/// open very-important issue of the workspace (any state), or `None` when
/// unknown; the ready listing's own very-important issues always count too.
/// Only rows that pass [`holds_slot`] (from either input) can hold the slot;
/// every other very-important ready issue is demoted once a holder exists,
/// eligible or not. With no eligible holder nothing is demoted.
/// Returns the demotions, in `ready` order. Pure: no forge, no log.
pub fn apply(idx: usize, ready: &mut [WorkItem], open_vi: Option<&[WorkItem]>) -> Vec<ViDemotion> {
    let ready_vi = ready.iter().filter(|i| is_very_important(i)).count();
    if ready_vi <= VERY_IMPORTANT_CAP && open_vi.is_none() {
        return Vec::new();
    }
    let open: Vec<IssueKey> = ready
        .iter()
        .filter(|i| is_very_important(i))
        .chain(open_vi.unwrap_or_default())
        .filter(|i| holds_slot(i))
        .map(key)
        .collect();
    let holders = very_important_holders(&open);
    let Some(&holder) = holders.first() else {
        return Vec::new();
    };
    let mut demoted = Vec::new();
    for item in ready.iter_mut() {
        if !is_very_important(item) || holders.contains(&item.number) {
            continue;
        }
        item.labels.retain(|l| l != VERY_IMPORTANT_LABEL);
        if !item.labels.iter().any(|l| l == IMPORTANT_LABEL) {
            item.labels.push(IMPORTANT_LABEL.to_string());
        }
        demoted.push(ViDemotion {
            workspace_idx: idx,
            number: item.number,
            holder,
        });
    }
    demoted
}

/// The tick's step: when `ready` holds any very-important issue, read the
/// workspace's open very-important issues from `source` (no forge read
/// otherwise), [`apply`] the cap, record the demotions on `report` and warn.
pub fn enforce(
    idx: usize,
    source: &mut impl WorkSource,
    ready: &mut [WorkItem],
    report: &mut TickReport,
) {
    if !ready.iter().any(is_very_important) {
        return;
    }
    let open_vi = source.list_open_very_important();
    let demoted = apply(idx, ready, open_vi.as_deref());
    for d in &demoted {
        warn_once(d);
    }
    report.vi_demotions.extend(demoted);
}

/// Warn about a demotion the first time this process sees it, so a demotion
/// that persists for hours does not log on every tick; repeats go to debug.
fn warn_once(d: &ViDemotion) {
    static SEEN: OnceLock<Mutex<HashSet<(usize, u32, u32)>>> = OnceLock::new();
    let fresh = SEEN
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert((d.workspace_idx, d.number, d.holder));
    let msg = format!(
        "work_finder: loom:very-important cap ({VERY_IMPORTANT_CAP} per workspace, #11509): \
         issue #{} in workspace #{} ranks as loom:important; #{} holds the slot",
        d.number, d.workspace_idx, d.holder
    );
    if fresh {
        log::warn!("{msg}");
    } else {
        log::debug!("{msg}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "vi_cap_tests.rs"]
mod tests;
