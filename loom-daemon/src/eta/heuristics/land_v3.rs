//! `land-v3` (#9970, Slice 1): `land-v2`'s path, with each stage grid
//! calibrated before it is drawn from.
//!
//! # Why
//!
//! Live scoring of `land-v2` showed its p25–p75 interval covering ~5% of
//! landings against a 50% target, with a heavy late (optimistic) tail. One
//! mechanism the estimator is structurally blind to: the Monte Carlo draws
//! every stage **independently** (`combination.independence_assumed`), but
//! real stages of one item are positively correlated — a sweep that is slow in
//! Builder tends to be slow in review and merge as well (a contended repo, a
//! rebase loop, a flaky CI matrix). A sum of independent draws concentrates
//! more tightly than a sum of correlated ones, so the simulated interval is
//! too narrow.
//!
//! # What changed, and only what changed
//!
//! Identical to [`super::LandV2`] — same journals, same Kaplan–Meier grids,
//! same always-merge path, same Monte Carlo — except that each post-dispatch
//! stage's grid passes through [`adjust`] first:
//!
//! 1. **Interval widening** (every post-dispatch stage): each grid point's
//!    distance from the raw median is stretched by [`UPPER_STRETCH`] above it
//!    and [`LOWER_STRETCH`] below it (floored at zero). The stage median
//!    itself does not move.
//! 2. **Complexity** (`sweep.builder` only): scaled by
//!    `(points / REFERENCE_POINTS)^COMPLEXITY_ELASTICITY`, from the issue's
//!    `points:N` label (or the `loom:points` marker). Per the downstream
//!    phase-independence data on #9970, size explains Builder time
//!    (ρ ≈ 0.45) and next to nothing downstream (ρ 0.09–0.20), so **no other
//!    stage is scaled by size**.
//! 3. **Queue friction** (`review_wait`, `merge_wait`): every grid point is
//!    shifted by [`FRICTION_SEC_PER_RUNNING_SWEEP`] per sweep running in the
//!    queue view (`features.queue_running`) — each is a PR competing for the
//!    Judge and for the repo's merge serialization.
//! 4. **Downstream floor** (`review_wait`): no grid point below
//!    [`REVIEW_FLOOR_SEC`], the low edge of the observed Judge review cluster.
//!
//! `ready_wait` is never adjusted: its draws are already one per slot
//! turnover from the dispatch plan.
//!
//! Every transform is recorded in the stage's `distribution.adjustment`, and
//! the grid the explanation carries is the adjusted one, so an estimate still
//! recomputes exactly from its own explanation.
//!
//! # Missing inputs degrade, never fabricate
//!
//! A feature `land-v3` reads but the input lacks contributes nothing (scale
//! `1.0`, offset `0`) and is named in `features_omitted` with reason
//! [`INPUT_MISSING`]. Today the tracker populates `labels` (so a live
//! `points:N` label does scale Builder) but not `queue_running`, and the
//! backtest replays with no features at all — so replayed `land-v3`
//! estimates are `land-v2` plus the interval widening and floor alone, and
//! the friction term stays inert until point-in-time queue state is
//! reconstructed (#10197). The input is read from `EstimateInput` only —
//! never looked up live.
//!
//! # The constants are fixture-derived, not fitted to live data
//!
//! [`UPPER_STRETCH`] and [`LOWER_STRETCH`] were chosen on deterministic
//! correlated-stage fixtures (the one in `eta::tests::land_v3`, plus five
//! variants of its stage mix and slowness regimes used while choosing). On
//! all six, a lower-side stretch of `1.3` moved `land-v2`'s replayed
//! p25–p75 coverage toward 50% **and** lowered mean pinball loss, while
//! **every** upward stretch tried (1.3–2.0) *raised* pinball loss: on a
//! stationary fixture `land-v2`'s p75 is not too early, so there is no
//! fixture evidence for widening the right tail. The late misses seen live
//! come from what a stationary history cannot show (queue contention,
//! non-stationarity, refusals at `merge_wait`), so the right tail is left at
//! `1.0` here and is a knob for the operator's real-history tuning (hours
//! −96..−48), not a number this module can justify. That tuning and the
//! promotion decision are the operator's steps after this ships. The other
//! constants are the documented priors above.
//!
//! # It does not get to skip the gate
//!
//! `land-v3` ships **registered, not current**, exactly like `land-v2`
//! before it: promotion is [`crate::eta::shadow`]'s two-gate rule.

use super::{estimate_path, PathRules};
use crate::eta::explanation::{FeatureOmitted, Features, StageAdjustment};
use crate::eta::grid::GRID_POINTS;
use crate::eta::history::{SampleSource, StageSamples};
use crate::eta::{round3, EstimateInput, Explanation, Heuristic, Kind, Stage};
use crate::points_marker::POINTS_VALUES;
use crate::story_points::classify_points_labels;

/// The id. Immutable once shipped.
pub const LAND_V3: &str = "land-v3";

/// Stretch of each stage grid's distance **above** its raw median. `1.0`
/// (none): no fixture supports a value above it — see the module doc. The
/// live right tail is the operator's real-history tuning knob.
pub const UPPER_STRETCH: f64 = 1.0;

/// Stretch of each stage grid's distance **below** its raw median.
/// Fixture-derived (see the module doc).
pub const LOWER_STRETCH: f64 = 1.3;

/// The story-point size whose Builder time the unscaled history is taken to
/// describe: the middle of the `1, 2, 3, 5, 8, 13` vocabulary.
pub const REFERENCE_POINTS: f64 = 3.0;

/// How Builder time grows with size: sub-linear, a prior consistent with
/// the moderate size↔Builder correlation (ρ ≈ 0.45) reported on #9970.
pub const COMPLEXITY_ELASTICITY: f64 = 0.5;

/// Seconds of review/merge friction added per sweep running in the queue
/// view: a prior, five minutes per competing PR.
pub const FRICTION_SEC_PER_RUNNING_SWEEP: i64 = 300;

/// The fewest seconds a `review_wait` may take: one minute, the low edge of
/// the 1.0–2.5 min Judge review cluster reported on #9970.
pub const REVIEW_FLOOR_SEC: i64 = 60;

/// `features_omitted` reason for an input `land-v3` reads but did not get.
pub const INPUT_MISSING: &str = "land_v3_input_missing";

/// `land-v3`.
#[derive(Debug, Clone, Copy)]
pub struct LandV3;

impl Heuristic for LandV3 {
    fn id(&self) -> &'static str {
        LAND_V3
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        let mut input = input.clone();
        if story_points(&input.features).is_none() {
            note_missing(&mut input.features_omitted, "points_marker");
        }
        if input.features.queue_running.is_none() {
            note_missing(&mut input.features_omitted, "queue_running");
        }
        estimate_path(
            PathRules {
                id: LAND_V3,
                kind: Kind::Land,
                sources: &[SampleSource::SweepOutcome, SampleSource::StageJournal],
                always_merge: true,
                censoring: true,
                adjust: Some(adjust),
            },
            &input,
            history,
        )
    }
}

fn note_missing(omitted: &mut Vec<FeatureOmitted>, name: &str) {
    if !omitted.iter().any(|o| o.name == name) {
        omitted.push(FeatureOmitted {
            name: name.to_string(),
            reason: INPUT_MISSING.to_string(),
        });
    }
}

/// The item's story-point size: its one in-vocabulary `points:N` label,
/// else an in-vocabulary `loom:points` marker. `None` when unsized — never a
/// guess.
#[must_use]
pub fn story_points(features: &Features) -> Option<u32> {
    features
        .labels
        .as_deref()
        .and_then(|labels| classify_points_labels(labels).value())
        .or_else(|| {
            features
                .points_marker
                .filter(|p| POINTS_VALUES.contains(&p.to_string().as_str()))
        })
}

/// The Builder-stage multiplier for a `points`-sized item.
#[must_use]
pub fn complexity_scale(points: u32) -> f64 {
    round3((f64::from(points) / REFERENCE_POINTS).powf(COMPLEXITY_ELASTICITY))
}

/// `land-v3`'s per-stage grid transform. Pure: reads `stage`, the input's
/// features and the raw grid, nothing else.
fn adjust(
    stage: Stage,
    input: &EstimateInput,
    raw: Vec<i64>,
) -> (Vec<i64>, Option<StageAdjustment>) {
    if stage == Stage::ReadyWait || raw.len() != GRID_POINTS {
        return (raw, None);
    }
    let (scale, scale_basis) = match (stage, story_points(&input.features)) {
        (Stage::SweepBuilder, Some(points)) => {
            (complexity_scale(points), format!("points:{points}"))
        }
        _ => (1.0, "none".to_string()),
    };
    let offset_sec = match stage {
        Stage::ReviewWait | Stage::MergeWait => input
            .features
            .queue_running
            .map_or(0, |n| i64::from(n).saturating_mul(FRICTION_SEC_PER_RUNNING_SWEEP)),
        _ => 0,
    };
    let floor_sec = if stage == Stage::ReviewWait {
        REVIEW_FLOOR_SEC
    } else {
        0
    };
    let raw_p50 = raw[GRID_POINTS / 2];
    let grid = raw
        .iter()
        .map(|&g| {
            let distance = (g - raw_p50) as f64;
            let stretch = if distance >= 0.0 {
                UPPER_STRETCH
            } else {
                LOWER_STRETCH
            };
            let stretched = (raw_p50 as f64 + stretch * distance).max(0.0);
            ((stretched * scale).round() as i64)
                .saturating_add(offset_sec)
                .max(floor_sec)
        })
        .collect();
    let record = StageAdjustment {
        raw_p50,
        upper_stretch: UPPER_STRETCH,
        lower_stretch: LOWER_STRETCH,
        scale,
        scale_basis,
        offset_sec,
        floor_sec,
    };
    (grid, Some(record))
}
