//! `land-2026-10-08-ranked-rook` (#10921): twin-otter-b, except that a PR
//! waiting for review is answered from its place in the **Judge's real pick
//! order**.
//!
//! # Why
//!
//! Every other `land` heuristic sees the review queue as a count or a
//! learned feature, in first-in-first-out order (`little-v0`'s
//! [`crate::eta::stage_queue`]) or not at all. The Judge does not review in
//! that order: it walks `pr_planning::ordered_queue`, starred PRs first,
//! then newest first. So two PRs waiting in one repo got the same review
//! wait whatever their rank, and starring a PR moved no ETA until new
//! outcomes refit something. Here the rank is an input, read on the next
//! pass with no refit ([`crate::eta::planner_queue`]).
//!
//! # The switching rule
//!
//! | state at `as_of` | answered by |
//! |---|---|
//! | `review_wait`, with a Judge position and a positive repo review drain rate | the planner queue below |
//! | anything else, refusals included | `land-2026-10-04-twin-otter-b`, unchanged apart from the id |
//!
//! Too little evidence (no position, no review exit in the repo's window,
//! or a later stage without history) answers as twin-otter-b: it degrades,
//! never fabricates.
//!
//! # The planner queue
//!
//! ```text
//! wait    = (items_ahead + 1) / drain_rate     (the subject's own review exit included)
//! service = Σ recency-weighted mean duration   (each later stage, little-v0's)
//! p50     = wait + service
//! ```
//!
//! `items_ahead` is the Judge-ordered position in the subject's repo, and
//! `drain_rate` that repo's review exits per hour (the Judge runs one
//! instance per repo), both from
//! [`crate::eta::stage_queue::StageQueue::planner`]. The stages after review
//! use `little-v0`'s base: the recency-weighted mean of each stage on the
//! always-approve path (half life [`SERVICE_HALF_LIFE_SEC`]). The interval
//! is `little-v0`'s Gamma posterior on the rate (shape = the repo's exits,
//! [`DRAWS`] draws seeded from the estimate id). It ignores PRs that arrive
//! later and are picked first (newer, or starred later); that is a known
//! optimism for an old unstarred PR.
//!
//! # No conditioning on the time already waited
//!
//! The position is the state: the answer is the remaining time from
//! `as_of`, and the PR's age in `review_wait` is not read. Every other
//! `land` heuristic conditions the review wait on its elapsed age, so in
//! the 52-fold walk-forward (loom-experiments#21) a PR further back in its
//! repo's queue got a *shorter* ETA (Spearman −0.3 to −0.47 between
//! position and p50, against −0.04 for the outcomes). Here, at any age, a
//! later Judge position never gets an earlier quantile, and two PRs at the
//! same position get the same p50 whatever they have waited.
//!
//! # The explanation
//!
//! A queue answer records `queue` with `order` `judge_planner`, the FIFO
//! `items_ahead` beside it and the operator level; [`recompute`] derives the
//! four quantiles from that record alone (`simulate::run_explanation`). A
//! twin-otter-b answer is twin-otter-b's explanation.
//!
//! Ships **registered, not current**, tier `candidate`. Promotion is
//! [`crate::eta::shadow`]'s two-gate rule (#10233).

use super::little_v0::{DRAWS, MAX_SHAPE, SERVICE_HALF_LIFE_SEC};
use super::{blank, LandTwinOtterB};
use crate::eta::explanation::{CurrentStageRecord, EstimateResult, QueueRecord, ServiceRecord};
use crate::eta::fit::CoefficientFile;
use crate::eta::history::{SampleSource, StageSamples};
use crate::eta::simulate::{reachable_path, SplitMix64};
use crate::eta::{
    estimate_id, round3, seed_for, CurrentState, EstimateInput, Explanation, Heuristic, Kind,
    Stage, Tier,
};
use chrono::Duration;
use std::sync::Arc;

/// The id. Immutable once shipped.
pub const LAND_RANKED_ROOK: &str = "land-2026-10-08-ranked-rook";

/// `queue.order` of a planner-queue answer.
pub const ORDER: &str = "judge_planner";

/// `land-2026-10-08-ranked-rook`.
#[derive(Debug, Clone, Default)]
pub struct LandRankedRook {
    base: LandTwinOtterB,
}

impl LandRankedRook {
    /// The heuristic over `fit` (twin-otter-b's, for every other state).
    #[must_use]
    pub fn new(fit: Option<Arc<CoefficientFile>>) -> Self {
        LandRankedRook {
            base: LandTwinOtterB::new(fit),
        }
    }
}

impl Heuristic for LandRankedRook {
    fn id(&self) -> &'static str {
        LAND_RANKED_ROOK
    }

    fn kind(&self) -> Kind {
        Kind::Land
    }

    /// A candidate: eligible for promotion through the #10233 gate, counted
    /// in the shadow budget.
    fn tier(&self) -> Tier {
        Tier::Candidate
    }

    /// Its base answers a held PR, so it reads the modeled input (#10284).
    fn models_hold(&self) -> bool {
        true
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        if let Some(explanation) = planner_answer(input, history) {
            return explanation;
        }
        let mut explanation = self.base.estimate(input, history);
        explanation.heuristic = LAND_RANKED_ROOK.to_string();
        explanation.estimate_id =
            estimate_id(&input.subject, Kind::Land, LAND_RANKED_ROOK, input.as_of);
        explanation
    }
}

/// `(p25, p50, p75, p90)` seconds of a queue answer: `departures / rate`
/// plus `service_total`, the interval from `DRAWS` Gamma(`shape`) draws of
/// the rate seeded with `seed`. The one formula the estimate and
/// [`recompute`] share.
fn quantiles(
    departures: u32,
    rate: f64,
    shape: u32,
    service_total: i64,
    seed: u64,
) -> (i64, i64, i64, i64) {
    let p50 = (f64::from(departures) / rate * 3600.0).round() as i64 + service_total;
    let mut rng = SplitMix64::new(seed);
    let mut totals: Vec<i64> = (0..DRAWS)
        .map(|_| {
            let g: f64 = (0..shape).map(|_| -(1.0 - rng.next_f64()).ln()).sum();
            let draw_rate = rate * g / f64::from(shape);
            (f64::from(departures) / draw_rate * 3600.0).round() as i64 + service_total
        })
        .collect();
    totals.sort_unstable();
    let at = |pct: usize| totals[(pct * DRAWS).div_ceil(100).clamp(1, DRAWS) - 1];
    let p25 = at(25).min(p50);
    let p75 = at(75).max(p50);
    (p25, p50, p75, at(90).max(p75))
}

/// The four quantiles of a ranked-rook queue answer, from its `queue`
/// record alone. `None` for any other record.
#[must_use]
pub fn recompute(record: &QueueRecord) -> Option<(i64, i64, i64, i64)> {
    let rate = record.drain_rate_per_hr;
    if record.order.as_deref() != Some(ORDER) || !(rate.is_finite() && rate > 0.0) {
        return None;
    }
    let seed = u64::from_str_radix(record.seed.trim_start_matches("0x"), 16).ok()?;
    Some(quantiles(
        record.items_ahead.saturating_add(1),
        rate,
        record.gamma_shape,
        record.service_total_sec,
        seed,
    ))
}

/// The planner-queue answer for a PR in `review_wait`, or `None` when it is
/// in another state or the evidence is too thin.
fn planner_answer(input: &EstimateInput, history: &StageSamples) -> Option<Explanation> {
    let CurrentState::At(current) = &input.current else {
        return None;
    };
    if current.stage != Stage::ReviewWait {
        return None;
    }
    let queue = input.queue.iter().find(|q| q.stage == current.stage)?;
    let planner = queue.planner.as_ref()?;
    let rate = planner.drain_rate_per_hr;
    if !(rate.is_finite() && rate > 0.0) {
        return None;
    }
    let as_of = input.as_of;
    let sources = [SampleSource::SweepOutcome, SampleSource::StageJournal];
    let mut service = Vec::new();
    let mut samples_min = planner.exits as usize;
    for stage in reachable_path(current.stage, true, false, false)
        .into_iter()
        .skip(1)
    {
        let mean = history.weighted_mean(
            &input.subject.repo,
            stage,
            as_of,
            &sources,
            SERVICE_HALF_LIFE_SEC,
        )?;
        samples_min = samples_min.min(mean.n);
        service.push(ServiceRecord {
            stage,
            level: mean.level.as_str().to_string(),
            n: mean.n,
            mean_sec: round3(mean.mean_sec),
        });
    }
    let service_total = service.iter().map(|s| s.mean_sec).sum::<f64>().round() as i64;
    let mut explanation = blank(LAND_RANKED_ROOK, Kind::Land, input);
    explanation.current_stage = Some(CurrentStageRecord {
        stage: current.stage,
        entered_at: current.entered_at,
        age_sec: current.age_sec,
        age_source: current.age_source,
        rework_rounds: current.rework_rounds,
    });
    let seed = seed_for(&explanation.estimate_id);
    let shape = planner.exits.clamp(1, MAX_SHAPE);
    let departures = planner.items_ahead.saturating_add(1);
    let (p25, p50, p75, p90) = quantiles(departures, rate, shape, service_total, seed);
    explanation.queue = Some(QueueRecord {
        stage: current.stage,
        scope: "repo".to_string(),
        items_ahead: planner.items_ahead,
        drain_rate_per_hr: rate,
        half_life_sec: queue.half_life_sec,
        window_sec: queue.window_sec,
        exits: planner.exits,
        wait_sec: p50 - service_total,
        service_half_life_sec: SERVICE_HALF_LIFE_SEC,
        service,
        service_total_sec: service_total,
        draws: DRAWS,
        gamma_shape: shape,
        seed: format!("0x{seed:016x}"),
        rng: "splitmix64".to_string(),
        order: Some(ORDER.to_string()),
        fifo_items_ahead: Some(queue.items_ahead),
        operator_level: Some(planner.operator_level),
    });
    explanation.result = Some(EstimateResult {
        p25_sec: p25,
        p50_sec: p50,
        p75_sec: p75,
        p90_sec: Some(p90),
        eta_p50_at: as_of + Duration::seconds(p50),
        samples_min,
        stage_marks: Vec::new(),
        tail_extrapolated: false,
    });
    explanation.enforce_cap();
    Some(explanation)
}
