//! Deterministic Monte Carlo over the stage grids.
//!
//! Each of `draws` paths starts at the current stage and walks the model,
//! drawing every stage it visits from that stage's grid and a verdict at
//! every `review_wait`. The generator is consumed in a fixed order per path:
//! one uniform per stage visited, and after each `review_wait` below the
//! rework cap one more uniform for its verdict (`u < p_k` is a rejection).
//! The first stage's draw is conditioned on its age: `u` is mapped onto
//! `[f_age, 1]` and the age is subtracted (floored at zero).
//!
//! The quantiles of the path totals are nearest-rank over the sorted
//! totals, rounded to whole seconds.
//!
//! [`run_explanation`] rebuilds the whole simulation from an explanation's
//! fields alone, which is what makes an estimate recomputable offline.

use super::explanation::{Contributions, Explanation, StageMark};
use super::grid;
use super::{round3, Stage};
use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeMap;

/// SplitMix64: small, fast, and fully specified, so anyone can reproduce a
/// draw sequence from the seed (`rand` is not a dependency of this crate).
#[derive(Debug, Clone)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    /// A generator seeded with `seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        SplitMix64(seed)
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A uniform in `[0, 1)` from the top 53 bits.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1_u64 << 53) as f64
    }
}

/// Everything one simulation reads.
#[derive(Debug, Clone, PartialEq)]
pub struct PathSpec {
    /// The current stage.
    pub start: Stage,
    /// Rejections already taken.
    pub start_rework: u32,
    /// Whether an approved path continues to `merge_wait`.
    pub include_merge: bool,
    /// Grid per stage (`Stage::index`); `None` for a stage not on the path.
    pub grids: [Option<Vec<i64>>; 5],
    /// `(f_age, age_sec)` for the first stage.
    pub conditioning: Option<(f64, i64)>,
    /// `P(reject | attempt k)` for `k = 1..=cap`.
    pub p_by_attempt: Vec<f64>,
    /// Most rework rounds per path.
    pub cap: u32,
    /// Paths.
    pub draws: usize,
    /// Generator seed.
    pub seed: u64,
}

/// The outcome of a simulation.
#[derive(Debug, Clone, PartialEq)]
pub struct Simulation {
    /// Remaining seconds, `(p25, p50, p75)`.
    pub quantiles: (i64, i64, i64),
    /// Per-stage cumulative entry-time seconds, `(p25, p50, p75)`, over the
    /// paths that visit the stage (#9366); `None` for a stage no path
    /// visits. The terminal stage's samples are the path completion times,
    /// so its `p50` equals `quantiles.1`.
    pub entry_marks: [Option<(i64, i64, i64)>; 5],
    /// Fraction of paths visiting each stage.
    pub reached: [f64; 5],
    /// Mean visits per path for each stage.
    pub mean_visits: [f64; 5],
    /// Mean new rework rounds per path.
    pub expected_rework_rounds: f64,
    /// What dominates the result.
    pub contributions: Contributions,
}

impl Simulation {
    /// The explanation's `stage_marks`: one mark per [`Stage::ALL`] stage,
    /// in stage order, projected at wall-clock `as_of` (#9366).
    #[must_use]
    pub fn stage_marks(&self, as_of: DateTime<Utc>) -> Vec<StageMark> {
        Stage::ALL
            .iter()
            .map(|&stage| {
                let times = self.entry_marks[stage.index()].map(|(p25, p50, p75)| {
                    (
                        Some(as_of + Duration::seconds(p25)),
                        Some(as_of + Duration::seconds(p50)),
                        Some(as_of + Duration::seconds(p75)),
                    )
                });
                let (p25_at, p50_at, p75_at) = times.unwrap_or((None, None, None));
                let mean_visits = self.mean_visits[stage.index()];
                StageMark {
                    stage,
                    p25_at,
                    p50_at,
                    p75_at,
                    mean_visits: (mean_visits > 0.0).then_some(mean_visits),
                }
            })
            .collect()
    }
}

/// Why a spec cannot be simulated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecError {
    /// A stage the path can visit has no grid.
    MissingGrid(Stage),
    /// A grid is not 21 points.
    BadGrid(Stage),
    /// Zero draws.
    NoDraws,
}

/// `P(reject)` at the verdict after `rework` rejections; attempts past the
/// recorded ones reuse the last rate.
#[must_use]
pub fn p_reject(p_by_attempt: &[f64], rework: u32) -> f64 {
    p_by_attempt
        .get(rework as usize)
        .or(p_by_attempt.last())
        .copied()
        .unwrap_or(0.0)
}

/// Whether a path that has taken `start_rework` rejections can take another:
/// some verdict still ahead (`start_rework..cap`) has a non-zero rate. Only
/// the remaining attempts count — an earlier attempt's rate is history.
#[must_use]
pub fn may_reject(p_by_attempt: &[f64], start_rework: u32, cap: u32) -> bool {
    (start_rework..cap).any(|rework| p_reject(p_by_attempt, rework) > 0.0)
}

/// Every stage a path from `start` can visit.
#[must_use]
pub fn reachable(start: Stage, include_merge: bool, may_reject: bool) -> Vec<Stage> {
    let mut stages = Vec::new();
    let mut push = |s: Stage| {
        if !stages.contains(&s) {
            stages.push(s);
        }
    };
    match start {
        Stage::SweepCurator => {
            push(Stage::SweepCurator);
            push(Stage::SweepBuilder);
            push(Stage::ReviewWait);
        }
        Stage::SweepBuilder => {
            push(Stage::SweepBuilder);
            push(Stage::ReviewWait);
        }
        Stage::ReviewWait => push(Stage::ReviewWait),
        Stage::Doctor => {
            push(Stage::Doctor);
            push(Stage::ReviewWait);
        }
        Stage::MergeWait => {
            push(Stage::MergeWait);
            return stages;
        }
    }
    if may_reject {
        push(Stage::Doctor);
    }
    if include_merge {
        push(Stage::MergeWait);
    }
    stages
}

/// Run the simulation.
pub fn run(spec: &PathSpec) -> Result<Simulation, SpecError> {
    if spec.draws == 0 {
        return Err(SpecError::NoDraws);
    }
    let rejectable = may_reject(&spec.p_by_attempt, spec.start_rework, spec.cap);
    for stage in reachable(spec.start, spec.include_merge, rejectable) {
        match &spec.grids[stage.index()] {
            None => return Err(SpecError::MissingGrid(stage)),
            Some(g) if g.len() != grid::GRID_POINTS => return Err(SpecError::BadGrid(stage)),
            Some(_) => {}
        }
    }

    let mut rng = SplitMix64::new(spec.seed);
    let mut totals: Vec<(f64, usize)> = Vec::with_capacity(spec.draws);
    let mut per_stage: Vec<[f64; 5]> = Vec::with_capacity(spec.draws);
    let mut reworks: Vec<u32> = Vec::with_capacity(spec.draws);
    let mut visits = [0_usize; 5];
    let mut visited_paths = [0_usize; 5];
    let mut entries: [Vec<(f64, usize)>; 5] = Default::default();

    for path in 0..spec.draws {
        let mut stage = spec.start;
        let mut rework = spec.start_rework;
        let mut first = true;
        let mut total = 0.0;
        let mut times = [0.0_f64; 5];
        let mut seen = [false; 5];
        loop {
            let grid = spec.grids[stage.index()]
                .as_deref()
                .unwrap_or_else(|| unreachable!("checked above"));
            let u = rng.next_f64();
            let duration = match (first, spec.conditioning) {
                (true, Some((f_age, age))) => {
                    (grid::inv_cdf(grid, f_age + u * (1.0 - f_age)) - age as f64).max(0.0)
                }
                _ => grid::inv_cdf(grid, u),
            };
            first = false;
            if !seen[stage.index()] {
                // First visit: the path enters this stage at its running
                // total so far (#9366). Captured without touching the
                // generator, so every draw stream is byte-identical.
                entries[stage.index()].push((total, path));
            }
            total += duration;
            times[stage.index()] += duration;
            visits[stage.index()] += 1;
            seen[stage.index()] = true;
            stage = match stage {
                Stage::SweepCurator => Stage::SweepBuilder,
                Stage::SweepBuilder => Stage::ReviewWait,
                Stage::Doctor => Stage::ReviewWait,
                Stage::ReviewWait => {
                    let mut rejected = false;
                    if rework < spec.cap {
                        rejected = rng.next_f64() < p_reject(&spec.p_by_attempt, rework);
                    }
                    if rejected {
                        rework += 1;
                        Stage::Doctor
                    } else if spec.include_merge {
                        Stage::MergeWait
                    } else {
                        break;
                    }
                }
                Stage::MergeWait => break,
            };
        }
        for (i, s) in seen.iter().enumerate() {
            if *s {
                visited_paths[i] += 1;
            }
        }
        totals.push((total, path));
        per_stage.push(times);
        reworks.push(rework - spec.start_rework);
    }

    totals.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    let quantiles = nearest_rank3(&totals);

    // The stage every path ends on: the approving verdict when the path
    // stops there, else `merge_wait`. Its mark is the path completion time —
    // each path's sample is that path's total, through the same rank math —
    // so the terminal mark's p50 is the estimate itself, to the second.
    let terminal = if spec.include_merge {
        Stage::MergeWait
    } else {
        Stage::ReviewWait
    };
    let mut entry_marks: [Option<(i64, i64, i64)>; 5] = Default::default();
    for stage in Stage::ALL {
        let i = stage.index();
        let samples: &[(f64, usize)] = if i == terminal.index() {
            &totals
        } else {
            entries[i].sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            &entries[i]
        };
        if !samples.is_empty() {
            entry_marks[i] = Some(nearest_rank3(samples));
        }
    }

    let draws = spec.draws as f64;
    let mut reached = [0.0; 5];
    let mut mean_visits = [0.0; 5];
    for i in 0..5 {
        reached[i] = round3(visited_paths[i] as f64 / draws);
        mean_visits[i] = round3(visits[i] as f64 / draws);
    }
    let expected_rework_rounds = round3(reworks.iter().map(|&r| f64::from(r)).sum::<f64>() / draws);

    let contributions = contributions(&totals, &per_stage, &reworks);
    Ok(Simulation {
        quantiles,
        entry_marks,
        reached,
        mean_visits,
        expected_rework_rounds,
        contributions,
    })
}

/// Nearest-rank `(p25, p50, p75)` over sorted `(value, path)` samples,
/// rounded to whole seconds — the one quantile discipline for the path
/// totals and the stage-entry marks alike.
fn nearest_rank3(samples: &[(f64, usize)]) -> (i64, i64, i64) {
    let k = samples.len();
    let q = |pct: usize| -> i64 {
        let rank = (pct * k).div_ceil(100).clamp(1, k);
        samples[rank - 1].0.round() as i64
    };
    (q(25), q(50), q(75))
}

/// Mean per-stage time over the paths ranked in `[lo, hi)` (fractions of K).
fn band_means(
    totals: &[(f64, usize)],
    per_stage: &[[f64; 5]],
    lo: f64,
    hi: f64,
) -> ([f64; 5], f64) {
    let k = totals.len();
    let start = ((lo * k as f64) as usize).min(k);
    let end = ((hi * k as f64) as usize).clamp(start, k);
    let mut sums = [0.0; 5];
    let mut total = 0.0;
    let count = (end - start).max(1) as f64;
    for &(t, path) in &totals[start..end] {
        total += t;
        for (i, s) in per_stage[path].iter().enumerate() {
            sums[i] += s;
        }
    }
    (sums.map(|s| s / count), total / count)
}

fn contributions(
    totals: &[(f64, usize)],
    per_stage: &[[f64; 5]],
    reworks: &[u32],
) -> Contributions {
    let mut p50_share = BTreeMap::new();
    let (mid, mid_total) = band_means(totals, per_stage, 0.4, 0.6);
    if mid_total > 0.0 {
        for stage in Stage::ALL {
            let v = mid[stage.index()];
            if v > 0.0 {
                p50_share.insert(stage.as_str().to_string(), round3(v / mid_total));
            }
        }
    }

    let mut iqr_share = BTreeMap::new();
    let (upper, _) = band_means(totals, per_stage, 0.5, 0.75);
    let (lower, _) = band_means(totals, per_stage, 0.25, 0.5);
    let gaps: Vec<f64> = (0..5).map(|i| (upper[i] - lower[i]).max(0.0)).collect();
    let gap_sum: f64 = gaps.iter().sum();
    if gap_sum > 0.0 {
        for stage in Stage::ALL {
            let g = gaps[stage.index()];
            if g > 0.0 {
                iqr_share.insert(stage.as_str().to_string(), round3(g / gap_sum));
            }
        }
    }

    let k = totals.len();
    let rework_fraction_by_quartile = (0..4)
        .map(|quartile| {
            let start = quartile * k / 4;
            let end = (quartile + 1) * k / 4;
            let n = (end - start).max(1) as f64;
            let reworked = totals[start..end]
                .iter()
                .filter(|(_, path)| reworks[*path] > 0)
                .count() as f64;
            round3(reworked / n)
        })
        .collect();

    Contributions {
        p50_share,
        iqr_share,
        rework_fraction_by_quartile,
    }
}

/// Parse an explanation's `0x…` seed.
#[must_use]
pub fn parse_seed(seed: &str) -> Option<u64> {
    u64::from_str_radix(seed.strip_prefix("0x")?, 16).ok()
}

/// Rebuild the simulation from an explanation's own fields — `path`,
/// `current_stage`, `stages[].distribution.grid_sec`,
/// `stages[0].conditioning`, `branches`, `combination` — and nothing else.
/// `None` when the explanation carries no estimate or was truncated.
#[must_use]
pub fn spec_from_explanation(explanation: &Explanation) -> Option<PathSpec> {
    let path = explanation.path.as_ref()?;
    let current = explanation.current_stage.as_ref()?;
    let combination = explanation.combination.as_ref()?;
    let mut grids: [Option<Vec<i64>>; 5] = Default::default();
    for entry in &explanation.stages {
        if entry.distribution.grid_sec.len() != grid::GRID_POINTS {
            return None;
        }
        grids[entry.stage.index()] = Some(entry.distribution.grid_sec.clone());
    }
    let conditioning = explanation
        .stages
        .first()
        .and_then(|e| e.conditioning.as_ref())
        .map(|c| (c.f_age, c.age_sec));
    let (p_by_attempt, cap) = match &explanation.branches {
        Some(b) => (b.changes_requested.p_by_attempt.clone(), b.changes_requested.cap),
        None => (Vec::new(), 0),
    };
    Some(PathSpec {
        start: path.start,
        start_rework: current.rework_rounds,
        include_merge: path.include_merge,
        grids,
        conditioning,
        p_by_attempt,
        cap,
        draws: combination.draws,
        seed: parse_seed(&combination.seed)?,
    })
}

/// Recompute an explanation's quantiles from its own fields.
#[must_use]
pub fn run_explanation(explanation: &Explanation) -> Option<(i64, i64, i64)> {
    let spec = spec_from_explanation(explanation)?;
    run(&spec).ok().map(|s| s.quantiles)
}

/// Recompute an explanation's stage marks (#9366) from the same fields
/// [`run_explanation`] reads — `path`, `current_stage`,
/// `stages[].distribution.grid_sec`, `stages[0].conditioning`, `branches`,
/// `combination` — and nothing else. `None` when the explanation carries no
/// estimate or was truncated.
#[must_use]
pub fn run_marks(explanation: &Explanation) -> Option<Vec<StageMark>> {
    let spec = spec_from_explanation(explanation)?;
    run(&spec).ok().map(|s| s.stage_marks(explanation.as_of))
}
