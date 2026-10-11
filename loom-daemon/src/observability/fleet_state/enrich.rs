//! Forge-dated entry instants and rework counts for `fleet.state` rows
//! (#11391), from the same per-item label history `eta.stage_outcome` reads
//! (#11367, `history.rs`).
//!
//! The in-memory tracker loses a row's entry when the daemon restarts: every
//! row it first sees mid-stage is `entered_at_lower_bound`. The forge still
//! has the label event that put the item in the stage, so a lower-bound row
//! of a stage a label enters is dated by it. The result is remembered per
//! `(repo, issue, stage)`, so it is applied again for free when a sweep row
//! (rebuilt each pass from the registry) is lower-bound again, and a dated
//! row is read once. A row no label dates (a sweep's curator/builder stage,
//! or a history with no such event) stays a lower bound.
//!
//! The same history of a PR counts the Judge rejections it took
//! ([`rejections`]), once per PR; later rejections are counted by
//! `build_view` as rows enter `doctor`.
//!
//! No extra reads: every history goes through the pass-wide cache that
//! `outcomes::diff` then starts from, and at most [`MAX_READS_PER_PASS`] new
//! histories are read per pass (the rest are retried next pass).

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};

use super::history::{labeled, LabelHistory};
use super::outcomes::{forge_entry, ForgeReads};
use super::FleetView;
use crate::telemetry::kinds::fleet_state::FleetStage;

/// The most histories one pass reads for enrichment.
pub const MAX_READS_PER_PASS: usize = 100;

const CHANGES_REQUESTED: &str = "loom:changes-requested";

/// A pass's history reads by `(repo, number)`; a failed read is `None`.
pub type Histories = BTreeMap<(String, u32), Option<LabelHistory>>;

/// What enrichment remembers between passes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnrichMemory {
    /// Forge entry instants found for a lower-bound row.
    dated: BTreeMap<(String, u32, FleetStage), DateTime<Utc>>,
    /// Rows whose history was read and holds no such event.
    undatable: BTreeSet<(String, u32, FleetStage)>,
    /// PRs whose rejections were counted.
    counted: BTreeSet<(String, u32)>,
}

impl EnrichMemory {
    /// Empty memory (const, for the pass-state static).
    #[must_use]
    pub const fn new() -> Self {
        EnrichMemory {
            dated: BTreeMap::new(),
            undatable: BTreeSet::new(),
            counted: BTreeSet::new(),
        }
    }
}

/// The Judge rejections `history` shows by `now`: each arrival of
/// `loom:changes-requested`, except a repeat with no `loom:review-requested`
/// or `loom:pr` arrival since the last one (a relabel inside one attempt).
#[must_use]
pub fn rejections(history: &LabelHistory, now: DateTime<Utc>) -> u32 {
    let mut count = 0;
    let mut open = true;
    for (at, event) in &history.events {
        if *at > now {
            break;
        }
        if labeled(event, &[CHANGES_REQUESTED]) {
            if open {
                count += 1;
                open = false;
            }
        } else if labeled(event, &["loom:review-requested", "loom:pr"]) {
            open = true;
        }
    }
    count
}

fn number(stage: FleetStage, issue: u32, pr: Option<u32>) -> Option<u32> {
    match stage {
        FleetStage::ReadyWait => Some(issue),
        FleetStage::ReviewWait
        | FleetStage::Doctor
        | FleetStage::MergeWait
        | FleetStage::MergeHold => pr,
        _ => None,
    }
}

/// Read `(repo, n)` into `histories` unless cached; `None` when the budget is
/// spent or the read failed.
fn read<'a, F: ForgeReads>(
    forge: &mut F,
    histories: &'a mut Histories,
    reads: &mut usize,
    repo: &str,
    n: u32,
) -> Option<&'a LabelHistory> {
    let key = (repo.to_string(), n);
    if !histories.contains_key(&key) {
        if *reads >= MAX_READS_PER_PASS {
            return None;
        }
        *reads += 1;
        let history = forge.label_history(repo, n);
        histories.insert(key.clone(), history);
    }
    histories.get(&key).and_then(Option::as_ref)
}

/// Date `view`'s lower-bound rows from the forge and count each new PR's
/// rejections. `histories` is the pass's cache, to hand on to the outcome
/// diff.
pub fn enrich<F: ForgeReads>(
    view: &mut FleetView,
    memory: &mut EnrichMemory,
    forge: &mut F,
    histories: &mut Histories,
    now: DateTime<Utc>,
) {
    let mut reads = 0;
    let mut live = BTreeSet::new();
    let mut live_prs = BTreeSet::new();
    for (repo, state) in &mut view.repos {
        for (&issue, row) in &mut state.rows {
            let key = (repo.clone(), issue, row.stage);
            live.insert(key.clone());
            if let Some(pr) = row.pr {
                live_prs.insert((repo.clone(), pr));
            }
            if row.entered_at_lower_bound {
                let mut found = memory.dated.get(&key).copied();
                if found.is_none() && !memory.undatable.contains(&key) {
                    if let Some(n) = number(row.stage, issue, row.pr) {
                        if let Some(history) = read(forge, histories, &mut reads, repo, n) {
                            match forge_entry(history, row.stage, now) {
                                Some(at) => {
                                    memory.dated.insert(key.clone(), at);
                                    found = Some(at);
                                }
                                None => {
                                    memory.undatable.insert(key.clone());
                                }
                            }
                        }
                    }
                }
                if let Some(at) = found {
                    row.entered_at = at;
                    row.entered_at_lower_bound = false;
                }
            }
            if let Some(pr) = row.pr {
                let counted = (repo.clone(), pr);
                if !memory.counted.contains(&counted) {
                    if let Some(history) = read(forge, histories, &mut reads, repo, pr) {
                        row.rework = row.rework.max(rejections(history, now));
                        memory.counted.insert(counted);
                    }
                }
            }
        }
    }
    memory.dated.retain(|k, _| live.contains(k));
    memory.undatable.retain(|k| live.contains(k));
    memory.counted.retain(|k| live_prs.contains(k));
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "enrich_tests.rs"]
mod tests;
