//! The seeded, weighted item draw (#11896; design: #11894,
//! `docs/design/work-finder-step-dispatch.md` §4.2 step 3).
//!
//! The step engine draws **one item** from a single pool of every actionable
//! item across the host's workspaces, to advance it by one step (#12185; the
//! earlier two-stage draw picked a workspace first, then an item in it). This
//! module is that draw, as library code the finder cutover (#11909) calls;
//! nothing here spawns, claims or loops.
//!
//! # The weight
//!
//! Every classified item ([`crate::work_finder::step_item::classify`]) gets
//! an **item weight** `iw = actionable × priority × age`:
//!
//! - **`actionable` ∈ {0, 1}.** 0 for a [`Verdict::Pass`], and for a
//!   [`Step::Builder`] in a repo whose build back-off is engaged. A red-main
//!   fix ([`Lane::RedMain`]) is exempt from the back-off, as it is in the
//!   finder (`work_finder::build_backoff::defers`): it is what drains the
//!   debt. Also 0 for a step whose role's token pool is exhausted (the
//!   role-tick state `record_role_tick_pool_exhausted` keeps, #7607): the
//!   spawn would be refused, so drawing the step only burns the redraw
//!   budget and leaves the slot empty. Also 0 for an `(item, step)` the
//!   outcome ledger has cooling down at the tick's clock
//!   ([`WeighContext::cooldowns`], #11900): a no-op, a pool or claim
//!   refusal, a fast failure, the repeat cap, a landed item's Builder step,
//!   or a Champion re-evaluation ladder rung that is not due. A weight-0 item
//!   never enters the pool; it is counted by its [`Exclusion`] instead.
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
//! factor `divisor_min`.
//!
//! # The pool weight (#12185)
//!
//! The draw weighs an item in the pool by
//!
//! ```text
//! w_item = w_repo × iw × S_r^(α−1) × (1 + in_flight_repo)^(−β)
//! ```
//!
//! where `S_r` is the sum of `iw` over the repo's items still in the pool,
//! `w_repo` is the repo's roster weight (`workspaces.json`, set from the fleet
//! store), `in_flight_repo` is how many of the repo's items someone works on
//! right now (this host's live runs plus the live claims peers advertise), α
//! is [`PoolKnobs::backlog_exponent`] and β is [`PoolKnobs::busy_exponent`].
//!
//! - **α = 0** gives `w_repo × iw / S_r`: the repo's share is its `w_repo`
//!   whatever its backlog, and `iw` splits it among its items. That is the
//!   earlier two-stage draw's distribution.
//! - **α = 1** gives `w_repo × iw`: a flat pool, a repo's share grows with
//!   its backlog.
//! - **β = 0** switches the busy-ness term off; a larger β makes a repo with
//!   work already in flight draw less.
//!
//! The terms are real numbers, so the arithmetic is floating point. The
//! result is scaled so the heaviest item weighs `2^40` and rounded to an
//! integer (a positive weight never rounds to 0), and the pooled draw is
//! integer from there: the roll is drawn without modulo bias ([`roll_for`],
//! rejection sampling), so the draw probabilities are exactly `w / Σw` of the
//! scaled weights, within `2^-40` of the real formula. [`pool_weights`] is the
//! one function the draw and [`replay_record`] both weigh with. A Σw that
//! would overflow `u64` is an error ([`DrawError`]), never a silently
//! saturated total.
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
//! Each draw has its own seed, scheme `loom-draw/v3` ([`item_seed`]):
//! `sha256("loom-draw/v3:" + minute + ":" + host + ":" + tick_counter + ":" +
//! draw_index)`, first 8 bytes big-endian. (`v2` also keyed the drawn
//! workspace; there is no workspace draw now.) Draws are without
//! replacement: a drawn item leaves the pool, and the next draw uses the next
//! `draw_index`, counted across the whole tick.
//!
//! **Replay.** `pick.decision`'s `item_draw` carries the seed inputs, the two
//! exponents, each repo's roster weight and in-flight count (`repos[]`), the
//! **starting pool** of every repo in pool order with every item's weight
//! (`pools[]`), the repos closed mid-tick (`closures[]`), and every draw's
//! roll and picked index. [`replay_record`] re-derives each seed and weighs
//! the logged pool minus earlier picks and closed repos with
//! [`pool_weights`], so the whole sequence replays from the record alone.
//!
//! The tick counter is per process: a daemon restarted within the same
//! minute starts it again at 0 and reuses that minute's seeds. That is
//! harmless (the pools differ, leases dedupe, and replay reads the logged
//! inputs, not a clock), the same tolerance the v1 workspace seed has.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;

use super::outcome_ledger::StepCooldowns;
use super::stage_entry::{Source, StageEntry};
use crate::priority_pick::{Level, PickRng};
use crate::step_role::StepRole;
use crate::work_finder::step_item::{Item, Lane, PassReason, Step, Verdict};
use crate::work_finder::vi_cap::{self, ViDemotion};
use crate::work_finder::WorkItem;

/// The seed-derivation scheme of the item draw; also the hash input's prefix.
pub const ITEM_DRAW_SEED_SCHEME: &str = "loom-draw/v3";

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

/// `hyperparameters.dispatch` key of the backlog exponent α.
pub const BACKLOG_EXPONENT_KEY: &str = "backlogExponent";
/// `hyperparameters.dispatch` key of the busy-ness exponent β.
pub const BUSY_EXPONENT_KEY: &str = "busyExponent";
/// Default α: 0, the two-stage draw's distribution.
pub const DEFAULT_BACKLOG_EXPONENT: f64 = 0.0;
/// Default β: 0, no busy-ness term.
pub const DEFAULT_BUSY_EXPONENT: f64 = 0.0;
/// Accepted range of α, inclusive.
pub const BACKLOG_EXPONENT_RANGE: (f64, f64) = (0.0, 1.0);
/// Accepted range of β, inclusive. The upper bound only keeps
/// `(1 + in_flight)^(−β)` far from underflow.
pub const BUSY_EXPONENT_RANGE: (f64, f64) = (0.0, 100.0);

/// The heaviest item of a draw weighs this many units once scaled to
/// integers; Σw stays under `u64::MAX` for up to `2^24` pooled items.
const SCALED_MAX_WEIGHT: f64 = (1u64 << 40) as f64;

const MINUTES_PER_DAY: u64 = 24 * 60;

/// The pool draw's two exponents (strict hyperparameters block,
/// `dispatch.backlogExponent` and `dispatch.busyExponent`, #12185).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PoolKnobs {
    /// α in `[0, 1]`: 0 keeps a repo's share fixed whatever its backlog, 1
    /// lets it grow in proportion to the backlog.
    pub backlog_exponent: f64,
    /// β ≥ 0: how strongly a repo with work already in flight is drawn less.
    pub busy_exponent: f64,
}

impl Default for PoolKnobs {
    fn default() -> Self {
        Self {
            backlog_exponent: DEFAULT_BACKLOG_EXPONENT,
            busy_exponent: DEFAULT_BUSY_EXPONENT,
        }
    }
}

impl PoolKnobs {
    /// Overlay a hyperparameters layer (`{"dispatch": {...}}`) on the
    /// defaults. A missing, mistyped, non-finite or out-of-range value keeps
    /// its default, like [`ItemWeights::from_layer`].
    #[must_use]
    pub fn from_layer(layer: &Value) -> Self {
        let d = Self::default();
        let dispatch = layer.get("dispatch");
        let pick = |key: &str, (lo, hi): (f64, f64), default: f64| -> f64 {
            dispatch
                .and_then(|g| g.get(key))
                .and_then(Value::as_f64)
                .filter(|x| x.is_finite() && (lo..=hi).contains(x))
                .unwrap_or(default)
        };
        Self {
            backlog_exponent: pick(
                BACKLOG_EXPONENT_KEY,
                BACKLOG_EXPONENT_RANGE,
                d.backlog_exponent,
            ),
            busy_exponent: pick(BUSY_EXPONENT_KEY, BUSY_EXPONENT_RANGE, d.busy_exponent),
        }
    }

    /// The exponents in force for the workspace at `root`, re-read on every
    /// call so an edit hot-applies on the next tick.
    #[must_use]
    pub fn for_root(root: &Path) -> Self {
        Self::from_layer(&crate::hyperparams::layer(root))
    }
}

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
    /// The outcome ledger has the `(item, step)` cooling down (#11900).
    CoolingDown,
}

impl Exclusion {
    /// The wire name: the pass reason's, or `build_backoff`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass(reason) => reason.as_str(),
            Self::PoolExhausted => "pool_exhausted",
            Self::BuildBackoff => "build_backoff",
            Self::CoolingDown => "cooling_down",
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
    /// The outcome ledger's query (#11900): every `(item, step)` cooling down,
    /// from this host's ledger, the peers' broadcasts and the forge ladder.
    /// Read at [`Self::now`].
    pub cooldowns: StepCooldowns,
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
    /// The item weight `iw`: `priority × (divisor_min + min(age_min,
    /// cap_min))`. The pool weight ([`pool_weights`]) scales it by the
    /// repo's terms.
    pub weight: u64,
}

/// A pooled item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The index of the workspace (repo) it came from.
    pub workspace_idx: usize,
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
        let target = input.item.target().unwrap_or_default();
        if ctx
            .cooldowns
            .is_cooling(target, StepRole::from(step), ctx.now)
        {
            *excluded.entry(Exclusion::CoolingDown).or_default() += 1;
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
            workspace_idx: ctx.workspace_idx,
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
}

/// The seed of draw `draw_index` under `key`: the first 8 bytes, read
/// big-endian, of `sha256("loom-draw/v3:" + minute + ":" + host + ":" +
/// tick_counter + ":" + draw_index)`, numbers in plain decimal, strings
/// verbatim.
#[must_use]
pub fn item_seed(key: &SeedKey, draw_index: u32) -> u64 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(
        format!(
            "{ITEM_DRAW_SEED_SCHEME}:{}:{}:{}:{draw_index}",
            key.minute, key.host, key.tick_counter
        )
        .as_bytes(),
    );
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    u64::from_be_bytes(head)
}

/// A draw that cannot be made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawError {
    /// Σw does not fit a `u64`. Unreachable at the documented ranges (the
    /// scaled weights sum to under `2^64` for up to `2^24` items), but an
    /// error rather than a saturated total that would skew the draw.
    WeightOverflow,
}

impl std::fmt::Display for DrawError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WeightOverflow => f.write_str("item draw: total weight overflows u64"),
        }
    }
}

impl std::error::Error for DrawError {}

/// Σ `weights`, or [`DrawError::WeightOverflow`].
///
/// # Errors
/// When the sum does not fit a `u64`.
pub fn total_weight(weights: impl IntoIterator<Item = u64>) -> Result<u64, DrawError> {
    weights
        .into_iter()
        .try_fold(0u64, |acc, w| acc.checked_add(w).ok_or(DrawError::WeightOverflow))
}

/// The roll in `0..total` (`total` > 0) for `seed`: [`PickRng`] values,
/// rejecting the low `2^64 mod total` ones so every roll is equally likely
/// (no modulo bias).
#[must_use]
pub fn roll_for(seed: u64, total: u64) -> u64 {
    let total = total.max(1);
    let reject_below = total.wrapping_neg() % total;
    let mut rng = PickRng::from_seed(seed);
    loop {
        let x = rng.next_u64();
        if x >= reject_below {
            return x % total;
        }
    }
}

/// The index of the item `roll` selects among `weights`, in order: the first
/// whose cumulative weight exceeds `roll`. `None` when `roll` is past Σw.
#[must_use]
pub fn pick_by_roll(weights: impl IntoIterator<Item = u64>, roll: u64) -> Option<usize> {
    let mut acc = 0u64;
    for (i, w) in weights.into_iter().enumerate() {
        acc = acc.checked_add(w)?;
        if roll < acc {
            return Some(i);
        }
    }
    None
}

/// What the draw knows about a repo besides its items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepoTerms {
    /// `w_repo`: the repo's roster weight (0: never drawn).
    pub weight: u32,
    /// `in_flight_repo`: its items someone works on right now.
    pub in_flight: usize,
}

/// One pooled item as the pool weight reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolRow {
    /// The repo's workspace index.
    pub repo: usize,
    /// The item weight `iw` ([`WeightTerms::weight`]).
    pub item_weight: u64,
}

/// A repo's terms in one draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepoDrawTerms {
    /// `w_repo`.
    pub weight: u32,
    /// `in_flight_repo`.
    pub in_flight: usize,
    /// `n_items`: the repo's items in the pool at this draw.
    pub items: usize,
    /// `S_r`: the sum of their item weights.
    pub pool_weight: u64,
}

/// The weights of one draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolWeights {
    /// The integer draw weight of each row, in row order.
    pub weights: Vec<u64>,
    /// The terms of every repo with a row, by workspace index.
    pub repos: BTreeMap<usize, RepoDrawTerms>,
}

/// `x^e` for the exponents the knobs make common, without `powf`'s rounding.
fn power(x: f64, e: f64) -> f64 {
    if e == 0.0 {
        1.0
    } else if e == 1.0 {
        x
    } else if e == -1.0 {
        1.0 / x
    } else {
        x.powf(e)
    }
}

/// The draw weights of `rows` (module docs, "The pool weight"):
/// `w_repo × iw × S_r^(α−1) × (1 + in_flight_repo)^(−β)`, scaled so the
/// heaviest weighs `2^40`. A row whose repo is not in `repos`, or whose
/// `w_repo` or `iw` is 0, weighs 0. Pure; the draw and [`replay_record`]
/// both weigh with it.
#[must_use]
pub fn pool_weights(
    rows: &[PoolRow],
    repos: &BTreeMap<usize, RepoTerms>,
    knobs: &PoolKnobs,
) -> PoolWeights {
    let mut terms: BTreeMap<usize, RepoDrawTerms> = BTreeMap::new();
    for row in rows {
        let known = repos.get(&row.repo).copied().unwrap_or(RepoTerms {
            weight: 0,
            in_flight: 0,
        });
        let t = terms.entry(row.repo).or_insert(RepoDrawTerms {
            weight: known.weight,
            in_flight: known.in_flight,
            items: 0,
            pool_weight: 0,
        });
        t.items += 1;
        t.pool_weight = t.pool_weight.saturating_add(row.item_weight);
    }
    // Per repo: w_repo × S^(α−1) × (1 + in_flight)^(−β).
    let factor: BTreeMap<usize, f64> = terms
        .iter()
        .map(|(&repo, t)| {
            let backlog = power(t.pool_weight as f64, knobs.backlog_exponent - 1.0);
            let busy = power(1.0 + t.in_flight as f64, -knobs.busy_exponent);
            (repo, f64::from(t.weight) * backlog * busy)
        })
        .collect();
    let raw: Vec<f64> = rows
        .iter()
        .map(|row| {
            let w = row.item_weight as f64 * factor[&row.repo];
            if w.is_finite() && w > 0.0 {
                w
            } else {
                0.0
            }
        })
        .collect();
    let heaviest = raw.iter().copied().fold(0.0_f64, f64::max);
    let weights = if heaviest > 0.0 {
        let scale = SCALED_MAX_WEIGHT / heaviest;
        raw.iter()
            .map(|&w| {
                if w > 0.0 {
                    ((w * scale).round() as u64).max(1)
                } else {
                    0
                }
            })
            .collect()
    } else {
        vec![0; rows.len()]
    };
    PoolWeights {
        weights,
        repos: terms,
    }
}

/// The weight terms of one draw: what `pick.decision` and the step span
/// record beside the drawn item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawTerms {
    /// The drawn item's integer draw weight.
    pub weight: u64,
    /// The pool's total weight when it was drawn.
    pub total_weight: u64,
    /// Its repo's terms at that draw.
    pub repo: RepoDrawTerms,
}

/// One draw, as `pick.decision` records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawRecord {
    /// 0-based position in the tick's draws.
    pub draw_index: u32,
    /// [`item_seed`] of the key and `draw_index`.
    pub seed: u64,
    /// Items in the pool when drawn.
    pub pool_size: usize,
    /// Their total weight.
    pub total_weight: u64,
    /// The roll in `0..total_weight`.
    pub roll: u64,
    /// The drawn item's index in the pool as it stood (the starting pool
    /// minus earlier picks and closed repos, in pool order).
    pub picked_index: usize,
    /// The drawn item.
    pub picked: Candidate,
    /// The drawn item's weight terms.
    pub terms: DrawTerms,
}

/// Successive draws without replacement from the tick's one pool. The
/// cutover takes one draw per slot and draws again on a gate or claim pass.
#[derive(Debug, Clone)]
pub struct ItemDraw {
    start: Vec<Candidate>,
    pool: Vec<Candidate>,
    repos: BTreeMap<usize, RepoTerms>,
    knobs: PoolKnobs,
    key: SeedKey,
    next_index: u32,
    /// `(workspace index, draws taken)` of each repo [`Self::close_repo`]
    /// took out of the pool.
    closures: Vec<(usize, u32)>,
}

impl ItemDraw {
    /// A draw over `pool` (every repo's items, repos in workspace order, each
    /// repo's in item order), where `repos` holds each repo's terms.
    #[must_use]
    pub fn new(
        pool: Vec<Candidate>,
        repos: BTreeMap<usize, RepoTerms>,
        knobs: PoolKnobs,
        key: SeedKey,
    ) -> Self {
        Self {
            start: pool.clone(),
            pool,
            repos,
            knobs,
            key,
            next_index: 0,
            closures: Vec::new(),
        }
    }

    /// The pool before any draw, in pool order: what `pick.decision` logs so
    /// the draws replay.
    #[must_use]
    pub fn starting_pool(&self) -> &[Candidate] {
        &self.start
    }

    /// Each repo's terms, by workspace index.
    #[must_use]
    pub fn repos(&self) -> &BTreeMap<usize, RepoTerms> {
        &self.repos
    }

    /// The exponents this draw weighs with.
    #[must_use]
    pub fn knobs(&self) -> &PoolKnobs {
        &self.knobs
    }

    /// Take every remaining item of workspace `idx` out of the pool for the
    /// rest of the tick (its per-repo cap refused a draw). Recorded, so the
    /// draws still replay.
    pub fn close_repo(&mut self, idx: usize) {
        if self.pool.iter().any(|c| c.workspace_idx == idx) {
            self.pool.retain(|c| c.workspace_idx != idx);
            self.closures.push((idx, self.next_index));
        }
    }

    /// The repos taken out of the pool, as `(workspace index, draws taken
    /// before)`: they are absent from every draw with an index at least that.
    #[must_use]
    pub fn closures(&self) -> &[(usize, u32)] {
        &self.closures
    }

    /// The next draw, `Ok(None)` once the pool holds nothing drawable.
    ///
    /// # Errors
    /// [`DrawError::WeightOverflow`] when the pool's Σw does not fit a `u64`.
    pub fn try_next(&mut self) -> Result<Option<DrawRecord>, DrawError> {
        let seed = item_seed(&self.key, self.next_index);
        let pool_size = self.pool.len();
        let rows: Vec<PoolRow> = self
            .pool
            .iter()
            .map(|c| PoolRow {
                repo: c.workspace_idx,
                item_weight: c.terms.weight,
            })
            .collect();
        let weighed = pool_weights(&rows, &self.repos, &self.knobs);
        let total = total_weight(weighed.weights.iter().copied())?;
        if total == 0 {
            return Ok(None);
        }
        let roll = roll_for(seed, total);
        let Some(i) = pick_by_roll(weighed.weights.iter().copied(), roll) else {
            return Ok(None);
        };
        let picked = self.pool.remove(i);
        let repo = weighed.repos[&picked.workspace_idx];
        let record = DrawRecord {
            draw_index: self.next_index,
            seed,
            pool_size,
            total_weight: total,
            roll,
            picked_index: i,
            picked,
            terms: DrawTerms {
                weight: weighed.weights[i],
                total_weight: total,
                repo,
            },
        };
        self.next_index += 1;
        Ok(Some(record))
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

    /// [`ItemDraw::try_next`]; an overflow is logged and ends the draws.
    fn next(&mut self) -> Option<DrawRecord> {
        match self.try_next() {
            Ok(draw) => draw,
            Err(e) => {
                log::error!("{e} ({} items in the pool)", self.pool.len());
                None
            }
        }
    }
}

/// One replayed pick: the pool row it selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayedPick {
    pub repo: String,
    pub draw_index: u32,
    pub issue: Option<u32>,
    pub pr: Option<u32>,
}

/// Replay `record`'s draws from what it logged alone: start from the logged
/// `pools[]` (in `index` order) and `repos[]`, and for each draw, drop the
/// repos `closures[]` closed by then, weigh the pool left with
/// [`pool_weights`], re-derive the seed from the header, check the logged
/// seed, total, roll, weight, picked index and item against it, and remove
/// the pick. The logged pool must be indexed `0..n` and every row's repo must
/// be in `repos[]`, the draws must be numbered consecutively from 0, and each
/// draw's `pool_size` must be the pool left at that draw. Returns the picks
/// in draw order. Reassemble a chunked record first.
///
/// # Errors
/// A description of the first draw that does not replay.
pub fn replay_record(
    record: &crate::telemetry::kinds::pick_decision::PickItemDraw,
) -> Result<Vec<ReplayedPick>, String> {
    let num = |s: &str, what: &str| -> Result<u64, String> {
        s.parse()
            .map_err(|_| format!("{what} {s:?} is not a number"))
    };
    let exponent = |s: &str, what: &str| -> Result<f64, String> {
        s.parse::<f64>()
            .ok()
            .filter(|x| x.is_finite())
            .ok_or_else(|| format!("{what} {s:?} is not a number"))
    };
    let minute = num(&record.seed_minute, "seed_minute")?;
    let tick_counter = num(&record.tick_counter, "tick_counter")?;
    let knobs = PoolKnobs {
        backlog_exponent: exponent(&record.backlog_exponent, "backlog_exponent")?,
        busy_exponent: exponent(&record.busy_exponent, "busy_exponent")?,
    };
    let mut ids: BTreeMap<&str, usize> = BTreeMap::new();
    let mut repos: BTreeMap<usize, RepoTerms> = BTreeMap::new();
    for (id, r) in record.repos.iter().enumerate() {
        ids.insert(r.repo.as_str(), id);
        repos.insert(
            id,
            RepoTerms {
                weight: r.weight,
                in_flight: r.in_flight,
            },
        );
    }
    let mut pool: Vec<&crate::telemetry::kinds::pick_decision::PickItemPoolEntry> =
        record.pools.iter().collect();
    pool.sort_by_key(|r| r.index);
    if pool
        .iter()
        .zip(0u32..)
        .any(|(row, expected)| row.index != expected)
    {
        return Err("the logged pool is not indexed 0..n".to_string());
    }
    let row_repo = |row: &crate::telemetry::kinds::pick_decision::PickItemPoolEntry| {
        ids.get(row.repo.as_str()).copied().ok_or_else(|| {
            format!("pool row {} names {}, which repos[] lacks", row.index, row.repo)
        })
    };
    let mut picks = Vec::new();
    for (expected, d) in (0u32..).zip(&record.draws) {
        if d.draw_index != expected {
            return Err(format!("the record skips draw {expected}"));
        }
        let at = format!("draw {}", d.draw_index);
        let closed: Vec<&str> = record
            .closures
            .iter()
            .filter(|c| c.after_draw <= d.draw_index)
            .map(|c| c.repo.as_str())
            .collect();
        let live: Vec<&crate::telemetry::kinds::pick_decision::PickItemPoolEntry> = pool
            .iter()
            .copied()
            .filter(|row| !closed.contains(&row.repo.as_str()))
            .collect();
        if d.pool_size != live.len() {
            return Err(format!(
                "{at}: pool_size {} != {} items left in the logged pool",
                d.pool_size,
                live.len()
            ));
        }
        let rows: Vec<PoolRow> = live
            .iter()
            .map(|row| {
                Ok(PoolRow {
                    repo: row_repo(row)?,
                    item_weight: row.weight,
                })
            })
            .collect::<Result<_, String>>()?;
        let weighed = pool_weights(&rows, &repos, &knobs);
        let total = total_weight(weighed.weights.iter().copied()).map_err(|e| e.to_string())?;
        let key = SeedKey {
            minute,
            host: record.seed_host.clone(),
            tick_counter,
        };
        let seed = item_seed(&key, d.draw_index);
        if d.seed != seed.to_string() || d.total_weight != total || total == 0 {
            return Err(format!("{at}: seed or total does not match the logged pool"));
        }
        let roll = roll_for(seed, total);
        if roll != d.roll {
            return Err(format!("{at}: roll {roll} != logged {}", d.roll));
        }
        let i = pick_by_roll(weighed.weights.iter().copied(), roll)
            .ok_or_else(|| format!("{at}: roll past the pool"))?;
        let row = live[i];
        if i != d.picked_index
            || row.issue != d.issue
            || row.pr != d.pr
            || row.repo != d.repo
            || weighed.weights[i] != d.draw_weight
        {
            return Err(format!(
                "{at}: replay picked index {i}, the record names {}",
                d.picked_index
            ));
        }
        let index = row.index;
        pool.retain(|r| r.index != index);
        picks.push(ReplayedPick {
            repo: d.repo.clone(),
            draw_index: d.draw_index,
            issue: row.issue,
            pr: row.pr,
        });
    }
    Ok(picks)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "item_draw_tests.rs"]
mod tests;
