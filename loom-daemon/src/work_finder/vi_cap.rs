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
//! **"Open" is any state, not just `loom:issue`.** The holder is chosen over
//! every open very-important issue of the workspace
//! ([`super::WorkSource::list_open_very_important`]), so an issue being built
//! (`loom:building`, no longer in the ready listing) or still in curation
//! keeps the slot and a second very-important issue is not promoted behind
//! its back. When that listing is unavailable (a forge failure, or a source
//! without one) the cap is computed over the ready listing alone: at most one
//! ready issue is very-important, which is the best the tick can know.
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

fn key(item: &WorkItem) -> IssueKey {
    IssueKey {
        number: item.number,
        created_at: item.created_at.clone(),
        level: Level::VeryImportant,
        main_red_fix: false,
    }
}

/// Apply the cap to workspace `idx`'s `ready` listing. `open_vi` is every
/// open very-important issue of the workspace (any state), or `None` when
/// unknown; the ready listing's own very-important issues always count too.
/// Returns the demotions, in `ready` order. Pure: no forge, no log.
pub fn apply(idx: usize, ready: &mut [WorkItem], open_vi: Option<&[WorkItem]>) -> Vec<ViDemotion> {
    let mut open: Vec<IssueKey> = ready
        .iter()
        .filter(|i| is_very_important(i))
        .map(key)
        .collect();
    if open.len() <= VERY_IMPORTANT_CAP && open_vi.is_none() {
        return Vec::new();
    }
    open.extend(open_vi.unwrap_or_default().iter().map(key));
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
