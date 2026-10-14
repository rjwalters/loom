//! The seeded, weighted item draw (#11896; design: #11894,
//! `docs/design/work-finder-step-dispatch.md` §4.2 step 3).
//!
//! Once a workspace is drawn, the step engine draws **one item** of it to
//! advance by one step. This module is that draw, as library code the finder
//! cutover (#11909) calls; nothing here spawns, claims or loops.
//!
//! # The weight
//!
//! Every classified item ([`crate::work_finder::step_item::classify`]) gets
//! `weight = actionable × priority × age`:
//!
//! - **`actionable` ∈ {0, 1}.** 0 for a [`Verdict::Pass`], and for a
//!   [`Step::Builder`] in a repo whose build back-off is engaged. A red-main
//!   fix ([`Lane::RedMain`]) is exempt from the back-off, as it is in the
//!   finder (`work_finder::build_backoff::defers`): it is what drains the
//!   debt. Also 0 for a step whose role's token pool is exhausted (the
//!   role-tick state `record_role_tick_pool_exhausted` keeps, #7607): the
//!   spawn would be refused, so drawing the step only burns the redraw
//!   budget and leaves the slot empty. A weight-0 item never enters the
//!   pool; it is counted by its [`Exclusion`] instead.
//! - **`priority`**: [`ItemWeights::red_main`] for a red-main fix, else
//!   [`ItemWeights::very_important`] / [`ItemWeights::important`] by the
//!   item's level, else 1. One factor, not a product: a red-main fix that is
//!   also very-important weighs the red-main factor.
//! - **`age`** = `1 + min(stage_age_days, cap) / divisor`. The stage age is
//!   the stage-entry provider's answer ([`super::stage_entry`]), else the
//!   item's `created_at`, else 0 (`age_source = none`). There is no stage
//!   term and no per-stage quota (operator ruling, 2026-10-14).
//!
//! The arithmetic is integer, so a draw replays bit for bit: ages are whole
//! minutes, and the pooled weight is `priority × (divisor_min +
//! min(age_min, cap_min))`, which is `priority × age` scaled by the common
//! factor `divisor_min`. The draw probabilities are therefore exactly
//! `w / Σw` of the documented weight.
//!
//! # The very-important cap
//!
//! The `loom:very-important` flood cap ([`crate::work_finder::vi_cap`]) runs
//! over the **whole** item list, at every stage and before weighting, through
//! [`vi_cap::apply`] itself: one live very-important item per workspace keeps
//! ×8, every other ranks as important. Without it ×8 would apply uncapped at
//! every stage.
//!
//! # The seed
//!
//! Each draw has its own seed, scheme `loom-draw/v2` ([`item_seed`]):
//! `sha256("loom-draw/v2:" + minute + ":" + host + ":" + tick_counter + ":" +
//! workspace + ":" + draw_index)`, first 8 bytes big-endian. The workspace
//! draw's `loom-draw/v1` is keyed `(minute, host)` only, so two ticks in one
//! minute repeat it; the tick counter and workspace end that. Draws are
//! without replacement: a drawn item leaves the pool, and the next draw uses
//! the next `draw_index`. `pick.decision` carries the seed inputs, each
//! workspace's initial pool (in pool order) and every draw's weight terms, so
//! the sequence replays from the record.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;

use super::stage_entry::{Source, StageEntry};
use crate::priority_pick::{Level, PickRng};
use crate::work_finder::step_item::{Item, Lane, PassReason, Step, Verdict};
use crate::work_finder::vi_cap::{self, ViDemotion};
use crate::work_finder::WorkItem;

/// The seed-derivation scheme of the item draw; also the hash input's prefix.
pub const ITEM_DRAW_SEED_SCHEME: &str = "loom-draw/v2";

/// Default priority factor of a red-main fix (`dispatch.itemWeightRedMain`).
pub const DEFAULT_WEIGHT_RED_MAIN: u32 = 50;
/// Default priority factor of `loom:very-important`
/// (`dispatch.itemWeightVeryImportant`).
pub const DEFAULT_WEIGHT_VERY_IMPORTANT: u32 = 8;
/// Default priority factor of `loom:important` (`dispatch.itemWeightImportant`).
pub const DEFAULT_WEIGHT_IMPORTANT: u32 = 3;
/// Default cap on the stage age the age term counts (`dispatch.itemAgeCapDays`).
pub const DEFAULT_AGE_CAP_DAYS: u32 = 90;
/// Default divisor of the age term (`dispatch.itemAgeDivisorDays`).
pub const DEFAULT_AGE_DIVISOR_DAYS: u32 = 30;

/// Accepted range of each priority factor. At least 1: a factor of 0 would
/// silently remove a priority class from dispatch, which is what `actionable`
/// is for.
pub const PRIORITY_WEIGHT_RANGE: (u64, u64) = (1, 1_000_000);
/// Accepted range of the age cap, in days (0 turns the age term off).
pub const AGE_CAP_DAYS_RANGE: (u64, u64) = (0, 3650);
/// Accepted range of the age divisor, in days.
pub const AGE_DIVISOR_DAYS_RANGE: (u64, u64) = (1, 3650);

/// The `hyperparameters.dispatch` keys of the item draw, with their ranges,
/// in [`ItemWeights`] field order.
pub const HYPERPARAM_KEYS: [(&str, (u64, u64)); 5] = [
    ("itemWeightRedMain", PRIORITY_WEIGHT_RANGE),
    ("itemWeightVeryImportant", PRIORITY_WEIGHT_RANGE),
    ("itemWeightImportant", PRIORITY_WEIGHT_RANGE),
    ("itemAgeCapDays", AGE_CAP_DAYS_RANGE),
    ("itemAgeDivisorDays", AGE_DIVISOR_DAYS_RANGE),
];

const MINUTES_PER_DAY: u64 = 24 * 60;

/// The item draw's numeric knobs (strict hyperparameters block,
/// `dispatch.item*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemWeights {
    pub red_main: u32,
    pub very_important: u32,
    pub important: u32,
    pub age_cap_days: u32,
    pub age_divisor_days: u32,
}

impl Default for ItemWeights {
    fn default() -> Self {
        Self {
            red_main: DEFAULT_WEIGHT_RED_MAIN,
            very_important: DEFAULT_WEIGHT_VERY_IMPORTANT,
            important: DEFAULT_WEIGHT_IMPORTANT,
            age_cap_days: DEFAULT_AGE_CAP_DAYS,
            age_divisor_days: DEFAULT_AGE_DIVISOR_DAYS,
        }
    }
}

impl ItemWeights {
    /// Overlay a hyperparameters layer (`{"dispatch": {...}}`, as
    /// [`crate::hyperparams::layer`] returns it) on the defaults. A missing,
    /// mistyped or out-of-range value keeps its default: the strict gate is
    /// [`crate::hyperparams::startup_init`], and a value edited in mid-run
    /// soft-fails here like every other per-tick reader.
    #[must_use]
    pub fn from_layer(layer: &Value) -> Self {
        let d = Self::default();
        let dispatch = layer.get("dispatch");
        let pick = |i: usize, default: u32| -> u32 {
            let (key, (lo, hi)) = HYPERPARAM_KEYS[i];
            dispatch
                .and_then(|g| g.get(key))
                .and_then(Value::as_u64)
                .filter(|n| (lo..=hi).contains(n))
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(default)
        };
        Self {
            red_main: pick(0, d.red_main),
            very_important: pick(1, d.very_important),
            important: pick(2, d.important),
            age_cap_days: pick(3, d.age_cap_days),
            age_divisor_days: pick(4, d.age_divisor_days),
        }
    }

    /// The knobs in force for the workspace at `root`, re-read on every call
    /// so an edit hot-applies on the next tick.
    #[must_use]
    pub fn for_root(root: &Path) -> Self {
        Self::from_layer(&crate::hyperparams::layer(root))
    }

    /// The priority factor of an item in `lane` at `level` (after the cap).
    #[must_use]
    pub fn priority(&self, lane: Lane, level: Level) -> u32 {
        if lane == Lane::RedMain {
            return self.red_main;
        }
        match level {
            Level::VeryImportant => self.very_important,
            Level::Important => self.important,
            Level::Default => 1,
        }
    }

    fn cap_minutes(&self) -> u64 {
        u64::from(self.age_cap_days) * MINUTES_PER_DAY
    }

    fn divisor_minutes(&self) -> u64 {
        u64::from(self.age_divisor_days.max(1)) * MINUTES_PER_DAY
    }
}

/// Where an item's stage age came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AgeSource {
    /// The provider's mirror `labels_at`.
    Mirror,
    /// The provider's cached `labeled` events.
    Events,
    /// The item's creation time (the provider's fallback, or the caller's).
    Created,
    /// Nothing known: the age term is 1.
    None,
}

impl AgeSource {
    /// The `age_source` wire name: the provider's own source names, plus
    /// `none`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mirror => Source::Mirror.as_str(),
            Self::Events => Source::Events.as_str(),
            Self::Created => Source::Created.as_str(),
            Self::None => "none",
        }
    }
}

impl From<Source> for AgeSource {
    fn from(source: Source) -> Self {
        match source {
            Source::Mirror => Self::Mirror,
            Source::Events => Self::Events,
            Source::Created => Self::Created,
        }
    }
}

/// One classified item, with what the weight reads beyond its verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawInput {
    /// The classifier's item.
    pub item: Item,
    /// The labels the priority level is read from: the item's issue labels,
    /// or its PR's for a PR-only item. The cap may demote a copy of them; the
    /// caller's are never touched.
    pub labels: Vec<String>,
    /// The item's creation time (issue, or PR for a PR-only item).
    pub created_at: Option<DateTime<Utc>>,
    /// The stage-entry provider's answer, when it had one.
    pub stage_entry: Option<StageEntry>,
}

/// Why an item is out of the pool (weight 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Exclusion {
    /// The classifier passed it.
    Pass(PassReason),
    /// The step's role has no spawnable account in its token pool.
    PoolExhausted,
    /// A Builder step in a repo whose build back-off is engaged.
    BuildBackoff,
}

impl Exclusion {
    /// The wire name: the pass reason's, or `build_backoff`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass(reason) => reason.as_str(),
            Self::PoolExhausted => "pool_exhausted",
            Self::BuildBackoff => "build_backoff",
        }
    }
}

/// The tick facts the weight reads besides the items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeighContext {
    /// The clock the ages are measured against.
    pub now: DateTime<Utc>,
    /// The workspace's build back-off is engaged
    /// (`work_finder::build_backoff`).
    pub build_backoff_engaged: bool,
    /// The roles (`curator`, `builder`, ...: [`Step::role`]) whose token
    /// pool for this workspace is exhausted: the roles of the workspace's
    /// live `TokenSelectionFailureSource::RoleTick` entries
    /// (`SweepRegistry::record_role_tick_pool_exhausted`).
    pub pool_exhausted_roles: BTreeSet<String>,
    /// The workspace's index in the tick (recorded on the cap's demotions).
    pub workspace_idx: usize,
}

/// One item's weight and its terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WeightTerms {
    /// 1 for every pooled item (a 0 is an [`Exclusion`]).
    pub actionable: u8,
    /// The priority factor.
    pub priority: u32,
    /// The level the factor was read from, after the cap.
    pub level: Level,
    /// The stage age in whole minutes, uncapped.
    pub age_minutes: u64,
    /// The age term × 1000, rounded down.
    pub age_permille: u64,
    /// Where the age came from.
    pub age_source: AgeSource,
    /// The draw weight: `priority × (divisor_min + min(age_min, cap_min))`.
    pub weight: u64,
}

/// A pooled item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub item: Item,
    pub step: Step,
    pub terms: WeightTerms,
}

impl Candidate {
    /// The number the step is invoked on.
    #[must_use]
    pub fn target(&self) -> u32 {
        self.item.target().unwrap_or_default()
    }
}

/// One workspace's items, weighed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Weighing {
    /// The items with a non-zero weight, in item order (issue, then PR).
    pub pool: Vec<Candidate>,
    /// The weight-0 items, counted by reason.
    pub excluded: BTreeMap<Exclusion, usize>,
    /// The cap's demotions, in item order.
    pub demotions: Vec<ViDemotion>,
}

fn rfc3339(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// The age of `input`'s stage at `now`, and where it came from.
fn stage_age(input: &DrawInput, now: DateTime<Utc>) -> (u64, AgeSource) {
    let (at, source) = match (input.stage_entry, input.created_at) {
        (Some(entry), _) => (entry.at, AgeSource::from(entry.source)),
        (None, Some(created)) => (created, AgeSource::Created),
        (None, None) => return (0, AgeSource::None),
    };
    let minutes = (now - at).num_minutes();
    (u64::try_from(minutes).unwrap_or(0), source)
}

/// The weight terms of a step item in `lane` at `level`.
#[must_use]
pub fn weight_terms(
    weights: &ItemWeights,
    lane: Lane,
    level: Level,
    age_minutes: u64,
    age_source: AgeSource,
) -> WeightTerms {
    let priority = weights.priority(lane, level);
    let divisor = weights.divisor_minutes();
    let scaled_age = divisor + age_minutes.min(weights.cap_minutes());
    WeightTerms {
        actionable: 1,
        priority,
        level,
        age_minutes,
        age_permille: scaled_age.saturating_mul(1000) / divisor,
        age_source,
        weight: u64::from(priority).saturating_mul(scaled_age),
    }
}

/// Weigh one workspace's classified items: run the very-important cap over
/// all of them, count the weight-0 ones by reason, and pool the rest in item
/// order. Pure: no forge, no clock, no log.
#[must_use]
pub fn weigh(inputs: &[DrawInput], ctx: &WeighContext, weights: &ItemWeights) -> Weighing {
    // The cap reads every item, whatever its stage or verdict: `holds_slot`
    // decides which of them may keep the level.
    let mut capped: Vec<WorkItem> = inputs
        .iter()
        .map(|i| {
            let number = i.item.issue.or(i.item.pr).unwrap_or_default();
            WorkItem::with_created_at(number, i.labels.clone(), i.created_at.map(rfc3339))
        })
        .collect();
    let demotions = vi_cap::apply(ctx.workspace_idx, &mut capped, None);

    let mut pool = Vec::new();
    let mut excluded: BTreeMap<Exclusion, usize> = BTreeMap::new();
    for (input, work) in inputs.iter().zip(&capped) {
        let step = match input.item.verdict {
            Verdict::Pass(reason) => {
                *excluded.entry(Exclusion::Pass(reason)).or_default() += 1;
                continue;
            }
            Verdict::Step(step) => step,
        };
        if ctx.pool_exhausted_roles.contains(step.role()) {
            *excluded.entry(Exclusion::PoolExhausted).or_default() += 1;
            continue;
        }
        if step == Step::Builder && ctx.build_backoff_engaged && input.item.lane != Lane::RedMain {
            *excluded.entry(Exclusion::BuildBackoff).or_default() += 1;
            continue;
        }
        let (age_minutes, age_source) = stage_age(input, ctx.now);
        let terms = weight_terms(
            weights,
            input.item.lane,
            Level::of(&work.labels),
            age_minutes,
            age_source,
        );
        pool.push(Candidate {
            item: input.item.clone(),
            step,
            terms,
        });
    }
    pool.sort_by_key(|c| (c.item.issue.is_none(), c.item.issue, c.item.pr));
    Weighing {
        pool,
        excluded,
        demotions,
    }
}

/// What one tick's item draws are seeded from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedKey {
    /// The wall-clock minute, `floor(unix_utc_secs / 60)`.
    pub minute: u64,
    /// The host id ([`crate::sweep_registry::host_identity`]).
    pub host: String,
    /// The process's tick counter.
    pub tick_counter: u64,
    /// The drawn workspace, as `pick.decision` names it (the forge slug).
    pub workspace: String,
}

/// The seed of draw `draw_index` under `key`: the first 8 bytes, read
/// big-endian, of `sha256("loom-draw/v2:" + minute + ":" + host + ":" +
/// tick_counter + ":" + workspace + ":" + draw_index)`, numbers in plain
/// decimal, strings verbatim.
#[must_use]
pub fn item_seed(key: &SeedKey, draw_index: u32) -> u64 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(
        format!(
            "{ITEM_DRAW_SEED_SCHEME}:{}:{}:{}:{}:{draw_index}",
            key.minute, key.host, key.tick_counter, key.workspace
        )
        .as_bytes(),
    );
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(head)
}

/// The index `pool` draws with `seed`, the pool's total weight and the roll
/// in `0..total`; `None` when the pool weighs nothing. P(i) = `w_i / Σw`.
#[must_use]
pub fn pick_index(pool: &[Candidate], seed: u64) -> Option<(usize, u64, u64)> {
    let total = pool
        .iter()
        .fold(0u64, |acc, c| acc.saturating_add(c.terms.weight));
    if total == 0 {
        return None;
    }
    let roll = PickRng::from_seed(seed).below(total);
    let mut acc = 0u64;
    for (i, c) in pool.iter().enumerate() {
        acc = acc.saturating_add(c.terms.weight);
        if roll < acc {
            return Some((i, total, roll));
        }
    }
    None
}

/// One draw, as `pick.decision` records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawRecord {
    /// 0-based position in the tick's draws for this workspace.
    pub draw_index: u32,
    /// [`item_seed`] of the key and `draw_index`.
    pub seed: u64,
    /// Items in the pool when drawn.
    pub pool_size: usize,
    /// Their total weight.
    pub total_weight: u64,
    /// The roll in `0..total_weight`.
    pub roll: u64,
    /// The drawn item.
    pub picked: Candidate,
}

/// Successive draws without replacement from one workspace's pool. The
/// cutover takes one draw per slot and draws again on a gate or claim pass.
#[derive(Debug, Clone)]
pub struct ItemDraw {
    pool: Vec<Candidate>,
    key: SeedKey,
    next_index: u32,
}

impl ItemDraw {
    #[must_use]
    pub fn new(pool: Vec<Candidate>, key: SeedKey) -> Self {
        Self {
            pool,
            key,
            next_index: 0,
        }
    }

    /// The items not drawn yet.
    #[must_use]
    pub fn remaining(&self) -> &[Candidate] {
        &self.pool
    }

    /// The seed key.
    #[must_use]
    pub fn key(&self) -> &SeedKey {
        &self.key
    }
}

impl Iterator for ItemDraw {
    type Item = DrawRecord;

    fn next(&mut self) -> Option<DrawRecord> {
        let seed = item_seed(&self.key, self.next_index);
        let pool_size = self.pool.len();
        let (i, total_weight, roll) = pick_index(&self.pool, seed)?;
        let record = DrawRecord {
            draw_index: self.next_index,
            seed,
            pool_size,
            total_weight,
            roll,
            picked: self.pool.remove(i),
        };
        self.next_index += 1;
        Some(record)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "item_draw_tests.rs"]
mod tests;
