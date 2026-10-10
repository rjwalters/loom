//! Workspace-first dispatch order for the multi-workspace tick (#11103).
//!
//! The tick used to sort every candidate fleet-wide by
//! [`super::candidate_cmp`] (strict workspace-priority tiers). It now builds
//! the order pass 2 iterates by **repeated draws** through
//! [`crate::priority_pick`]:
//!
//! 1. each workspace's candidates are put in their in-workspace order —
//!    level, then oldest `createdAt`, then issue number
//!    ([`crate::priority_pick::issue_cmp`]);
//! 2. a workspace is drawn among those with candidates left, weighted by its
//!    `fleet_priority` ([`crate::priority_pick::weight_for_priority`]); a
//!    workspace whose next candidate is `loom:very-important` wins the draw
//!    (the draw is among just those when several qualify);
//! 3. the drawn workspace's next candidate is appended, and the draw repeats
//!    until every candidate is placed.
//!
//! The RNG is seeded once per tick ([`tick_seed`]); the seed and every draw
//! (candidates, weights, roll, pick) are kept on the [`TickReport`] and
//! exported in `pick.decision`, so a fixed seed reproduces the exact order.
//!
//! # Transitional level bridge
//!
//! Until the retired labels are migrated (#11103 slices 2 and 4),
//! [`effective_level`] also maps the legacy signals onto the new levels:
//! the star (`loom:operator-priority`) is `important`, a level-2 star and a
//! verified red-main fix are `very-important`. The bridge goes with the
//! legacy readers.

use std::cell::Cell;
use std::collections::{BTreeMap, VecDeque};

use super::{PriorityCandidate, TickReport, WorkItem};
use crate::priority_pick::{self, IssueKey, Level, PickRng, WorkspaceDraw, WorkspaceEntry};
use crate::workspace_registry::DEFAULT_WORKSPACE_PRIORITY;

/// At most this many draws are kept per tick (the first ones, which decide
/// the slots); [`DrawLog::draws_total`] carries the uncapped count.
pub const MAX_RECORDED_DRAWS: usize = 50;

/// One draw: the [`WorkspaceDraw`] record plus the candidate it placed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawStep {
    /// The draw as `priority_pick` recorded it. Workspace names are
    /// [`workspace_key`]s.
    pub draw: WorkspaceDraw,
    /// The drawn workspace's index.
    pub workspace_idx: usize,
    /// The issue the draw placed next in the order.
    pub issue: u32,
}

/// Every draw of one tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawLog {
    /// The tick's RNG seed: replaying the draws from it gives the same order.
    pub seed: u64,
    /// How many draws the tick made (one per candidate).
    pub draws_total: usize,
    /// The first [`MAX_RECORDED_DRAWS`] draws, in order.
    pub steps: Vec<DrawStep>,
}

/// The name a workspace carries inside the draw: its index, zero-padded so
/// the draw's name order is index order.
#[must_use]
pub fn workspace_key(idx: usize) -> String {
    format!("{idx:04}")
}

/// The index a [`workspace_key`] names.
#[must_use]
pub fn workspace_idx(key: &str) -> Option<usize> {
    key.parse().ok()
}

/// `item`'s priority level: its `loom:important` / `loom:very-important`
/// label, raised by the transitional legacy bridge (see the module docs).
/// `red_fix` is whether it is a red-main fix on a verified-red `main`.
#[must_use]
pub fn effective_level(item: &WorkItem, red_fix: bool) -> Level {
    let legacy = if red_fix || item.operator_level() >= 2 {
        Level::VeryImportant
    } else if item.is_operator_priority() {
        Level::Important
    } else {
        Level::Default
    };
    Level::of(&item.labels).max(legacy)
}

fn issue_key(c: &PriorityCandidate) -> IssueKey {
    IssueKey {
        number: c.number,
        created_at: c.created_at.clone(),
        level: c.level,
    }
}

/// The in-workspace order: level, oldest `createdAt`, issue number.
#[must_use]
pub fn in_workspace_cmp(a: &PriorityCandidate, b: &PriorityCandidate) -> std::cmp::Ordering {
    priority_pick::issue_cmp(&issue_key(a), &issue_key(b))
}

/// `candidates` in draw order (see the module docs), plus the draw log
/// (`None` when there was nothing to draw). `priorities[i]` is workspace
/// `i`'s `fleet_priority`; a missing entry is the default.
#[must_use]
pub fn draw_order(
    candidates: Vec<PriorityCandidate>,
    priorities: &[u32],
    seed: u64,
) -> (Vec<PriorityCandidate>, Option<DrawLog>) {
    let mut queues: BTreeMap<usize, VecDeque<PriorityCandidate>> = BTreeMap::new();
    for c in candidates {
        queues.entry(c.workspace_idx).or_default().push_back(c);
    }
    for q in queues.values_mut() {
        q.make_contiguous().sort_by(in_workspace_cmp);
    }
    let mut rng = PickRng::from_seed(seed);
    let mut order = Vec::new();
    let mut steps = Vec::new();
    loop {
        let entries: Vec<WorkspaceEntry> = queues
            .iter()
            .filter_map(|(idx, q)| {
                q.front().map(|next| WorkspaceEntry {
                    workspace: workspace_key(*idx),
                    priority: priorities
                        .get(*idx)
                        .copied()
                        .unwrap_or(DEFAULT_WORKSPACE_PRIORITY),
                    has_very_important: next.level == Level::VeryImportant,
                })
            })
            .collect();
        let Some(draw) = priority_pick::draw_workspace(&entries, &mut rng) else {
            break;
        };
        let Some((idx, c)) = workspace_idx(&draw.picked).and_then(|idx| {
            queues
                .get_mut(&idx)
                .and_then(VecDeque::pop_front)
                .map(|c| (idx, c))
        }) else {
            break;
        };
        if steps.len() < MAX_RECORDED_DRAWS {
            steps.push(DrawStep {
                draw,
                workspace_idx: idx,
                issue: c.number,
            });
        }
        order.push(c);
    }
    let log = (!order.is_empty()).then_some(DrawLog {
        seed,
        draws_total: order.len(),
        steps,
    });
    (order, log)
}

/// Put the tick's candidates in draw order, seeded by [`tick_seed`], and
/// keep the draw log on `report`. The multi-workspace tick's one call site.
#[must_use]
pub fn order(
    candidates: Vec<PriorityCandidate>,
    priorities: &[u32],
    report: &mut TickReport,
) -> Vec<PriorityCandidate> {
    let (ordered, log) = draw_order(candidates, priorities, tick_seed());
    report.workspace_draw = log;
    ordered
}

thread_local! {
    /// A seed pinned by [`with_seed`] for the ticks run inside it.
    static SEED_OVERRIDE: Cell<Option<u64>> = const { Cell::new(None) };
}

/// Run `f` with every tick on this thread seeded by `seed`, so a test drives
/// the real dispatch path through a known draw sequence.
pub fn with_seed<R>(seed: u64, f: impl FnOnce() -> R) -> R {
    let previous = SEED_OVERRIDE.with(|s| s.replace(Some(seed)));
    let out = f();
    SEED_OVERRIDE.with(|s| s.set(previous));
    out
}

/// The seed every unit test's tick uses unless [`with_seed`] pins another,
/// so ticks under test are deterministic.
#[cfg(test)]
pub const TEST_DEFAULT_SEED: u64 = 11103;

/// This tick's draw seed: a [`with_seed`] pin, else fresh entropy (the wall
/// clock mixed with a process counter; a fixed seed under `cfg(test)`).
#[must_use]
pub fn tick_seed() -> u64 {
    if let Some(seed) = SEED_OVERRIDE.with(Cell::get) {
        return seed;
    }
    fresh_seed()
}

#[cfg(test)]
fn fresh_seed() -> u64 {
    TEST_DEFAULT_SEED
}

#[cfg(not(test))]
fn fresh_seed() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let n = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    nanos ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "workspace_draw_tests.rs"]
mod tests;
