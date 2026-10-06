//! `little-v0` (#10208): a zero-parameter queue baseline from Little's law.
//!
//! For the stage the item is queued in:
//!
//! ```text
//! wait    = items_ahead / drain_rate                  (current stage)
//! service = Σ recency-weighted mean duration          (each later stage)
//! p50     = wait + service
//! ```
//!
//! `items_ahead` and `drain_rate` (exits per hour, exponentially weighted,
//! half life 6 h, over a 24 h window) arrive through
//! [`EstimateInput::queue`], computed by the tracker from events observed
//! before `as_of` ([`crate::eta::stage_queue`]); this heuristic never
//! fetches. The service times come from the same history every heuristic
//! reads, weighted by recency (half life [`SERVICE_HALF_LIFE_SEC`]).
//!
//! # Interval
//!
//! The drain rate is a count of exits, so it carries Poisson uncertainty.
//! The posterior on the rate is Gamma with shape `α = exits` and mean the
//! observed rate; [`DRAWS`] draws of it (seeded from the estimate's own id via
//! [`seed_for`], so identical input gives an identical explanation) are
//! propagated through `items_ahead / rate`. Fewer exits give a smaller `α`
//! and a wider interval. Service time is a point value (no interval
//! contribution). The shape is an integer, so a Gamma draw is a sum of `α`
//! exponentials: no further distribution is needed.
//!
//! # A floor, never promoted
//!
//! Registered, shadow-only, and expected to lose to `land-v2`: long waits are
//! mostly PRs held for a human, which a queue model cannot see. Its job is to
//! be the bar a fitted model must clear. [`crate::eta::shadow`] gates promote
//! only by two measured gates and `current.land` is never set to it.
//!
//! # Refusals
//!
//! Never a panic or an infinite value: no queue context for the current
//! stage, a non-PR stage, a zero drain rate with items ahead, or a later
//! stage with too little history each refuse. A zero queue needs no drain
//! rate: its wait is zero.

use super::{refuse, SampleSource};
use crate::eta::explanation::{CurrentStageRecord, EstimateResult, QueueRecord, ServiceRecord};
use crate::eta::history::StageSamples;
use crate::eta::queue_features::is_pr_stage;
use crate::eta::simulate::{reachable_path, SplitMix64};
use crate::eta::stall;
use crate::eta::{
    estimate_id, round3, seed_for, CurrentState, EstimateInput, Explanation, Heuristic, Kind,
    NoEstimateReason, Stage, EXPLANATION_SCHEMA,
};
use chrono::Duration;

/// The id. Immutable once shipped.
pub const LITTLE_V0: &str = "little-v0";

/// Whether `id` is a floor baseline that only ever runs in shadow: it is never
/// selectable as `current` and never promotable.
#[must_use]
pub fn is_shadow_only(id: &str) -> bool {
    id == LITTLE_V0
}

/// Gamma-posterior draws behind the interval.
pub const DRAWS: usize = 400;

/// Half life of the service-time weighting: 7 days.
pub const SERVICE_HALF_LIFE_SEC: i64 = 7 * 24 * 3600;

/// Largest Gamma shape drawn (a sum of this many exponentials per draw).
pub const MAX_SHAPE: u32 = 5000;

/// `little-v0`.
#[derive(Debug, Clone, Copy)]
pub struct LittleV0;

/// `items_ahead / drain_rate_per_hr`, whole seconds. The one formula the
/// estimate and its re-derivation share.
#[must_use]
pub fn wait_sec(items_ahead: u32, drain_rate_per_hr: f64) -> i64 {
    (f64::from(items_ahead) / drain_rate_per_hr * 3600.0).round() as i64
}

impl Heuristic for LittleV0 {
    fn id(&self) -> &'static str {
        LITTLE_V0
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    /// A baseline (#10525): the floor every candidate must clear; never promoted.
    fn tier(&self) -> crate::eta::Tier {
        crate::eta::Tier::Baseline
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        let as_of = input.as_of;
        let id = estimate_id(&input.subject, Kind::Land, LITTLE_V0, as_of);
        let features_omitted = input
            .features
            .complete_omissions(input.features_omitted.clone(), "not_collected");
        let mut explanation = Explanation {
            schema: EXPLANATION_SCHEMA.to_string(),
            estimate_id: id,
            heuristic: LITTLE_V0.to_string(),
            kind: Kind::Land,
            loom: input.provenance.clone(),
            as_of,
            subject: input.subject.clone(),
            current_stage: None,
            history_window: None,
            history: None,
            path: None,
            stages: Vec::new(),
            branches: None,
            combination: None,
            result: None,
            contributions: None,
            features: Some(input.features.clone()),
            features_omitted,
            no_estimate_reason: None,
            // Every heuristic records the binding stall (#10210); little-v0
            // never applies a stall term, so it stays `applied: false`.
            stalled: stall::binding(&input.stalls, as_of),
            truncated: Vec::new(),
            recalibration: None,
            calibration: None,
            twin_otter: None,
            queue: None,
            regime_adjustment: None,
            held_heron: None,
        };
        let current = match &input.current {
            CurrentState::Refused(reason) => return refuse(explanation, *reason),
            CurrentState::At(current) => current,
        };
        // #10218: a held PR reaches every heuristic as `merge_hold`; one that
        // does not model the hold refuses it `blocked`, as before the stage
        // existed (a queue cannot see a wait for a human).
        if current.stage == Stage::MergeHold {
            return refuse(explanation, NoEstimateReason::Blocked);
        }
        explanation.current_stage = Some(CurrentStageRecord {
            stage: current.stage,
            entered_at: current.entered_at,
            age_sec: current.age_sec,
            age_source: current.age_source,
            rework_rounds: current.rework_rounds,
        });
        if !is_pr_stage(current.stage) {
            return refuse(explanation, NoEstimateReason::UnknownStage);
        }
        let Some(queue) = input.queue.iter().find(|q| q.stage == current.stage) else {
            return refuse(explanation, NoEstimateReason::InsufficientSamples);
        };
        let rate = queue.drain_rate_per_hr;
        if queue.items_ahead > 0 && !(rate.is_finite() && rate > 0.0) {
            return refuse(explanation, NoEstimateReason::InsufficientSamples);
        }

        // Service time of each later stage on the always-approve path.
        let sources = [SampleSource::SweepOutcome, SampleSource::StageJournal];
        let mut service = Vec::new();
        let mut samples_min = usize::MAX;
        for stage in reachable_path(current.stage, true, false, false)
            .into_iter()
            .skip(1)
        {
            let Some(mean) = history.weighted_mean(
                &input.subject.repo,
                stage,
                as_of,
                &sources,
                SERVICE_HALF_LIFE_SEC,
            ) else {
                return refuse(explanation, NoEstimateReason::InsufficientSamples);
            };
            samples_min = samples_min.min(mean.n);
            service.push(ServiceRecord {
                stage,
                level: mean.level.as_str().to_string(),
                n: mean.n,
                mean_sec: round3(mean.mean_sec),
            });
        }
        let service_total = service.iter().map(|s| s.mean_sec).sum::<f64>().round() as i64;
        let wait = if queue.items_ahead == 0 {
            0
        } else {
            wait_sec(queue.items_ahead, rate)
        };
        let p50 = wait + service_total;

        // Interval: Gamma(α = exits) posterior on the rate, mean = observed.
        let seed = seed_for(&explanation.estimate_id);
        let shape = queue.exits.clamp(1, MAX_SHAPE);
        let (mut p25, mut p75, mut p90) = (p50, p50, p50);
        if queue.items_ahead > 0 {
            let mut rng = SplitMix64::new(seed);
            let mut totals: Vec<i64> = (0..DRAWS)
                .map(|_| {
                    let g: f64 = (0..shape).map(|_| -(1.0 - rng.next_f64()).ln()).sum();
                    let draw_rate = rate * g / f64::from(shape);
                    (f64::from(queue.items_ahead) / draw_rate * 3600.0).round() as i64
                        + service_total
                })
                .collect();
            totals.sort_unstable();
            let at = |pct: usize| totals[(pct * DRAWS).div_ceil(100).clamp(1, DRAWS) - 1];
            p25 = at(25).min(p50);
            p75 = at(75).max(p50);
            p90 = at(90).max(p75);
        }

        explanation.queue = Some(QueueRecord {
            stage: current.stage,
            scope: queue.scope.as_str().to_string(),
            items_ahead: queue.items_ahead,
            drain_rate_per_hr: rate,
            half_life_sec: queue.half_life_sec,
            window_sec: queue.window_sec,
            exits: queue.exits,
            wait_sec: wait,
            service_half_life_sec: SERVICE_HALF_LIFE_SEC,
            service,
            service_total_sec: service_total,
            draws: DRAWS,
            gamma_shape: shape,
            seed: format!("0x{seed:016x}"),
            rng: "splitmix64".to_string(),
        });
        explanation.result = Some(EstimateResult {
            p25_sec: p25,
            p50_sec: p50,
            p75_sec: p75,
            p90_sec: Some(p90),
            eta_p50_at: as_of + Duration::seconds(p50),
            samples_min: if samples_min == usize::MAX {
                queue.exits as usize
            } else {
                samples_min
            },
            stage_marks: Vec::new(),
            // A queue wait is never answered from a residual-life tail.
            tail_extrapolated: false,
        });
        explanation.enforce_cap();
        explanation
    }
}
