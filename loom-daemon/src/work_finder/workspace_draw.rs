//! Workspace-first dispatch order for the multi-workspace tick (#11103).
//!
//! The tick used to sort every candidate fleet-wide by
//! [`super::candidate_cmp`] (strict workspace-priority tiers). It now builds
//! the order pass 2 iterates by **repeated draws** through
//! [`crate::priority_pick`]:
//!
//! 1. each workspace's candidates are put in their in-workspace order —
//!    verified red-main fix first (#11547), then level, then oldest
//!    `createdAt`, then issue number ([`crate::priority_pick::issue_cmp`]);
//! 2. a workspace is drawn among those with candidates left, with
//!    probability w / Σw ([`crate::priority_pick::draw_workspace`], #11742):
//!    w is its roster `weight` plus the tick's [`weight_bonus`] points while
//!    its remaining candidates include a verified red-main fix,
//!    `loom:very-important` work or `loom:important` work. No tier wins
//!    outright;
//! 3. the drawn workspace's next candidate is appended, and the draw repeats
//!    until every candidate is placed.
//!
//! The RNG is seeded once per tick ([`tick_seed`]) from the wall-clock
//! minute and the host id ([`minute_seed`], #11683); the seed, its inputs and
//! every draw (candidates, base weights, bonuses, roll, pick) are kept on the
//! [`TickReport`] and exported in `pick.decision`, so the logged
//! `(minute, host)` alone reproduces the exact order.
//!
//! The level is the issue's own `loom:important` / `loom:very-important`
//! label ([`Level::of`]); there is no legacy bridge (#11508).

use std::cell::Cell;
use std::collections::{BTreeMap, VecDeque};

use super::{PriorityCandidate, TickReport};
use crate::priority_pick::{
    self, IssueKey, Level, PickRng, WeightBonus, WorkspaceDraw, WorkspaceEntry,
};
use crate::workspace_registry::DEFAULT_WORKSPACE_WEIGHT;

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
    /// What [`seed`](Self::seed) was derived from (#11683): `None` when it
    /// was pinned ([`with_seed`]) or passed to [`draw_order`] directly.
    pub seed_inputs: Option<SeedInputs>,
}

/// The inputs a tick's seed was derived from: enough to recompute it with
/// [`minute_seed`] (#11683).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedInputs {
    /// The derivation scheme, [`DRAW_SEED_SCHEME`].
    pub scheme: &'static str,
    /// The wall-clock minute, `floor(unix_utc_secs / 60)`.
    pub minute: u64,
    /// The host id ([`crate::sweep_registry::host_identity`]).
    pub host: String,
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

fn issue_key(c: &PriorityCandidate) -> IssueKey {
    IssueKey {
        number: c.number,
        created_at: c.created_at.clone(),
        level: c.level,
        main_red_fix: c.main_red_fix,
    }
}

/// The in-workspace order: verified red-main fix, level, oldest
/// `createdAt`, issue number.
#[must_use]
pub fn in_workspace_cmp(a: &PriorityCandidate, b: &PriorityCandidate) -> std::cmp::Ordering {
    priority_pick::issue_cmp(&issue_key(a), &issue_key(b))
}

/// `candidates` in draw order (see the module docs), plus the draw log
/// (`None` when there was nothing to draw). `weights[i]` is workspace `i`'s
/// roster `weight`; a missing entry is [`DEFAULT_WORKSPACE_WEIGHT`]. Every
/// draw adds `bonus` (#11742). A workspace of weight 0 is never drawn, so its
/// candidates are left out of the order.
#[must_use]
pub fn draw_order(
    candidates: Vec<PriorityCandidate>,
    weights: &[u32],
    seed: u64,
    bonus: &WeightBonus,
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
            .filter(|(_, q)| !q.is_empty())
            .map(|(idx, q)| WorkspaceEntry {
                workspace: workspace_key(*idx),
                weight: weights
                    .get(*idx)
                    .copied()
                    .unwrap_or(DEFAULT_WORKSPACE_WEIGHT),
                has_red_main_fix: q.iter().any(|c| c.main_red_fix),
                has_very_important: q.iter().any(|c| c.level == Level::VeryImportant),
                has_important: q.iter().any(|c| c.level == Level::Important),
            })
            .collect();
        let Some(draw) = priority_pick::draw_workspace(&entries, bonus, &mut rng) else {
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
        seed_inputs: None,
    });
    (order, log)
}

/// Put the tick's candidates in draw order, seeded by [`tick_seed`] with the
/// [`weight_bonus`] in force, and keep the draw log on `report`. The
/// multi-workspace tick's one call site.
#[must_use]
pub fn order(
    candidates: Vec<PriorityCandidate>,
    weights: &[u32],
    report: &mut TickReport,
) -> Vec<PriorityCandidate> {
    let (seed, inputs) = tick_seed();
    let (ordered, mut log) = draw_order(candidates, weights, seed, &weight_bonus());
    if let Some(log) = log.as_mut() {
        log.seed_inputs = inputs;
    }
    report.workspace_draw = log;
    ordered
}

/// The process's current weight bonuses (`dispatch.weightBonus*`, #11742).
/// Published once per tick by the work-finder loop's config reload
/// ([`super::ConfiguredMaxReloader::refresh`]) so a committed-config or
/// `LOOM_HYPERPARAMS` edit applies on the next tick without a restart, and
/// read by [`order`]. A process global rather than a tick argument because
/// the tick entry points live in the size-frozen `work_finder.rs`.
#[cfg(not(test))]
static WEIGHT_BONUS: std::sync::Mutex<WeightBonus> = std::sync::Mutex::new(WeightBonus {
    red_main_fix: priority_pick::DEFAULT_BONUS_RED_MAIN_FIX,
    very_important: priority_pick::DEFAULT_BONUS_VERY_IMPORTANT,
    important: priority_pick::DEFAULT_BONUS_IMPORTANT,
});

#[cfg(test)]
thread_local! {
    /// Under `cfg(test)` the bonuses are per thread, so a test that
    /// publishes some cannot change the draws of a tick another test runs
    /// concurrently (the same isolation [`with_seed`] gives the seed).
    static WEIGHT_BONUS: Cell<WeightBonus> = Cell::new(WeightBonus::default());
}

/// The weight bonuses the next tick uses.
#[must_use]
pub fn weight_bonus() -> WeightBonus {
    #[cfg(not(test))]
    {
        *WEIGHT_BONUS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    #[cfg(test)]
    {
        WEIGHT_BONUS.with(Cell::get)
    }
}

/// Publish the weight bonuses for the next tick. Returns the previous value,
/// so a caller can log a transition.
pub fn set_weight_bonus(bonus: WeightBonus) -> WeightBonus {
    #[cfg(not(test))]
    {
        std::mem::replace(
            &mut *WEIGHT_BONUS
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            bonus,
        )
    }
    #[cfg(test)]
    {
        WEIGHT_BONUS.with(|c| c.replace(bonus))
    }
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

/// The seed-derivation scheme a [`SeedInputs`] names; also the hash
/// input's prefix.
pub const DRAW_SEED_SCHEME: &str = "loom-draw/v1";

/// The draw seed for wall-clock `minute` (`floor(unix_utc_secs / 60)`) on
/// `host`: the first 8 bytes, read big-endian, of
/// `sha256("loom-draw/v1:" + minute + ":" + host)`, with `minute` in plain
/// decimal and `host` verbatim (#11683).
///
/// Ticks on one host in the same minute draw the same order (harmless: the
/// candidates differ between ticks, and leases dedupe); different hosts in
/// the same minute draw different orders, so they do not all reach for the
/// same issue first. Hosts that resolve no id share `unknown-host` and so
/// share orders — also tolerated, since leases and in-flight checks still
/// dedupe.
#[must_use]
pub fn minute_seed(minute: u64, host: &str) -> u64 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{DRAW_SEED_SCHEME}:{minute}:{host}").as_bytes());
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(head)
}

/// This tick's draw seed and what it was derived from: a [`with_seed`] pin
/// (no inputs), else [`minute_seed`] of the current minute and this host's
/// id. Under `cfg(test)` an unpinned tick gets [`TEST_DEFAULT_SEED`] instead,
/// so no test sees a clock-dependent order.
#[must_use]
pub fn tick_seed() -> (u64, Option<SeedInputs>) {
    if let Some(seed) = SEED_OVERRIDE.with(Cell::get) {
        return (seed, None);
    }
    fresh_seed()
}

#[cfg(test)]
fn fresh_seed() -> (u64, Option<SeedInputs>) {
    (TEST_DEFAULT_SEED, None)
}

#[cfg(not(test))]
fn fresh_seed() -> (u64, Option<SeedInputs>) {
    let minute = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 60);
    let host = crate::sweep_registry::host_identity();
    let seed = minute_seed(minute, &host);
    (
        seed,
        Some(SeedInputs {
            scheme: DRAW_SEED_SCHEME,
            minute,
            host,
        }),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "workspace_draw_tests.rs"]
mod tests;
