//! Linear quantile regression of remaining time to land, on a log scale:
//! p25, p50 and p75 fitted directly on pinball loss (#10193 "start linear").
//!
//! # Censoring: inverse probability of censoring weights
//!
//! Censoring here is administrative: a training row with `as_of` is censored
//! exactly when it is still open at the cutoff, so its **potential**
//! censoring time `C = cutoff − as_of` is known for every row, landed or not.
//! A landed row with remaining time `y` was only observable because
//! `C > y`, which favours fast landings — the optimism #9970 is fixing. Each
//! landed row is therefore weighted by `1 / G(y)`, where `G(t)` is the share
//! of training rows (landed **and** still-open) with `C > t`. Still-open rows
//! enter through `G`; slow landings count for the slow landings that were
//! censored. `G` is clipped below at [`G_FLOOR`] so one late landing cannot
//! dominate.
//!
//! # Rule 3: everything fitted comes from the fold's training rows only
//!
//! The stage vocabulary, which numeric columns are kept (present on at least
//! two training rows with non-zero spread), their means (also the imputation
//! value) and standard deviations, the censoring curve `G`, the per-issue
//! weights (`1 / rows of that issue`) and the coefficients. The remaining
//! constants — the log offset, the weight floor, the MM iteration count and
//! tolerance — are fixed in code, never computed from data.
//!
//! # Fitting
//!
//! Hunter & Lange's MM algorithm (iteratively reweighted least squares on a
//! quadratic majoriser of the pinball loss) with a ridge penalty on every
//! coefficient but the intercept, from a weighted least-squares start, for a
//! fixed maximum number of iterations. No randomness: the same training rows
//! give the same bits.

use super::dataset::{assert_point_in_time, Label, LeakError, Row};
use chrono::{DateTime, Timelike, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Seconds added before taking the log.
pub const LOG_OFFSET_SEC: f64 = 60.0;

/// Lowest censoring-survival value a landed row is weighted by.
pub const G_FLOOR: f64 = 0.05;

/// Fewest landed training rows a fit needs.
pub const MIN_LANDED: usize = 20;

const MM_ITERATIONS: usize = 100;
const MM_EPSILON: f64 = 1e-4;
const MM_TOLERANCE: f64 = 1e-8;

/// The quantile levels, in output order.
pub const LEVELS: [f64; 3] = [0.25, 0.5, 0.75];

/// The model's tunable settings, chosen on the selection folds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct QrSettings {
    /// Ridge strength in thousandths (an integer so settings compare
    /// exactly).
    pub ridge_milli: u32,
}

impl QrSettings {
    /// The settings grid, in a fixed order (a tie keeps the earliest).
    #[must_use]
    pub fn grid() -> Vec<QrSettings> {
        [10, 100, 1_000]
            .into_iter()
            .map(|ridge_milli| QrSettings { ridge_milli })
            .collect()
    }

    /// `qr-r<milli>`.
    #[must_use]
    pub fn id(&self) -> String {
        format!("qr-r{}", self.ridge_milli)
    }
}

/// The raw numeric columns, each a fixed function of what the row logged at
/// `as_of`: nothing here is fitted.
pub const COLUMNS: [&str; 21] = [
    "log_age",
    "rework_rounds",
    "points",
    "queue_ready",
    "queue_running",
    "active_sweeps_host",
    "doctor_cycles",
    "log_pr_lines",
    "log_pr_files",
    "hour_sin",
    "hour_cos",
    "weekend",
    "repo_first_pass_approval_rate",
    "log_repo_open_prs",
    "repo_pr_open_lockout",
    "log_repo_ci_typical",
    "ci_failing",
    "ci_pending",
    "pr_behind_main",
    "pr_merge_conflict",
    "operator_hold",
];

fn flag(b: Option<bool>) -> Option<f64> {
    b.map(|b| if b { 1.0 } else { 0.0 })
}

fn ln1p(v: Option<i64>) -> Option<f64> {
    v.map(|v| (v.max(0) as f64).ln_1p())
}

/// The raw column values of `row`, `None` where unmeasured.
#[must_use]
pub fn raw_columns(row: &Row) -> [Option<f64>; 21] {
    let s = &row.snapshot;
    let f = s.features.as_ref();
    let get = |pick: &dyn Fn(&crate::eta::explanation::Features) -> Option<f64>| f.and_then(pick);
    let hour = f64::from(s.as_of.hour()) * std::f64::consts::TAU / 24.0;
    let lines = f.and_then(|f| Some(f.pr_additions? + f.pr_deletions?));
    let ci = f.and_then(|f| f.pr_ci_status.clone());
    [
        ln1p(s.age_sec),
        s.rework_rounds.map(f64::from),
        get(&|f| f.points_marker.map(f64::from)),
        get(&|f| f.queue_ready.map(f64::from)),
        get(&|f| f.queue_running.map(f64::from)),
        get(&|f| f.active_sweeps_host.map(f64::from)),
        get(&|f| f.doctor_cycles_so_far.map(f64::from)),
        ln1p(lines),
        ln1p(f.and_then(|f| f.pr_changed_files)),
        Some(hour.sin()),
        Some(hour.cos()),
        Some(if weekend(s.as_of) { 1.0 } else { 0.0 }),
        get(&|f| f.repo_first_pass_approval_rate),
        ln1p(f.and_then(|f| f.repo_open_prs.map(i64::from))),
        get(&|f| flag(f.repo_pr_open_lockout)),
        ln1p(f.and_then(|f| f.repo_ci_typical_duration_sec)),
        ci.as_deref()
            .map(|c| if c == "failing" { 1.0 } else { 0.0 }),
        ci.as_deref()
            .map(|c| if c == "pending" { 1.0 } else { 0.0 }),
        get(&|f| flag(f.pr_behind_main)),
        get(&|f| flag(f.pr_merge_conflict)),
        get(&|f| flag(f.operator_hold)),
    ]
}

fn weekend(at: DateTime<Utc>) -> bool {
    use chrono::Datelike;
    at.weekday().num_days_from_monday() >= 5
}

fn stage_key(row: &Row) -> String {
    row.snapshot
        .stage
        .map_or_else(|| "unknown".to_string(), |s| s.as_str().to_string())
}

/// The fitted model and every fitted transform.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QrModel {
    /// The settings it was fitted with.
    pub settings: QrSettings,
    /// The fold cutoff: every training datum was knowable before it.
    pub cutoff: DateTime<Utc>,
    /// Training rows used (abandoned rows excluded).
    pub rows: usize,
    /// Of those, landed.
    pub landed: usize,
    /// Distinct training issues.
    pub issues: usize,
    /// Stage levels seen in training; the first is the reference level.
    pub stage_vocab: Vec<String>,
    /// Indices into [`COLUMNS`] of the kept numeric columns.
    pub kept: Vec<usize>,
    /// Their training means (the imputation value).
    pub means: Vec<f64>,
    /// Their training standard deviations.
    pub sds: Vec<f64>,
    /// The censoring curve's knots: sorted potential censoring times, s.
    pub censor_times: Vec<i64>,
    /// Coefficients per level (intercept, stage dummies, kept columns), or
    /// `None` when the training set could not support a fit.
    pub coef: Option<[Vec<f64>; 3]>,
}

impl QrModel {
    /// The design vector of `row`, or `None` for a stage training never saw.
    fn design(&self, row: &Row) -> Option<Vec<f64>> {
        let stage = self.stage_vocab.iter().position(|s| *s == stage_key(row))?;
        let raw = raw_columns(row);
        let mut x = Vec::with_capacity(self.stage_vocab.len() + self.kept.len());
        x.push(1.0);
        x.extend((1..self.stage_vocab.len()).map(|i| if i == stage { 1.0 } else { 0.0 }));
        for (k, &col) in self.kept.iter().enumerate() {
            x.push(raw[col].map_or(0.0, |v| (v - self.means[k]) / self.sds[k]));
        }
        Some(x)
    }

    /// `G(t)`: the share of training rows whose potential censoring time
    /// exceeds `t`.
    fn survival(&self, t: i64) -> f64 {
        let n = self.censor_times.len();
        let at_or_below = self.censor_times.partition_point(|c| *c <= t);
        (n - at_or_below) as f64 / n.max(1) as f64
    }

    /// Fit on `training`, every row of which must be knowable before
    /// `cutoff` (checked first: a leaky training set is refused, not fitted).
    pub fn fit(
        training: &[Row],
        cutoff: DateTime<Utc>,
        settings: QrSettings,
    ) -> Result<Self, LeakError> {
        assert_point_in_time(training, cutoff)?;
        let usable: Vec<&Row> = training
            .iter()
            .filter(|r| !matches!(r.label, Label::Abandoned { .. }))
            .collect();
        let mut per_issue: BTreeMap<&str, usize> = BTreeMap::new();
        for row in &usable {
            *per_issue.entry(row.snapshot.story.as_str()).or_default() += 1;
        }
        let stage_vocab: Vec<String> = usable
            .iter()
            .map(|r| stage_key(r))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let raws: Vec<[Option<f64>; 21]> = usable.iter().map(|r| raw_columns(r)).collect();
        let (mut kept, mut means, mut sds) = (Vec::new(), Vec::new(), Vec::new());
        for col in 0..COLUMNS.len() {
            let values: Vec<f64> = raws.iter().filter_map(|r| r[col]).collect();
            if values.len() < 2 {
                continue;
            }
            let mean = values.iter().sum::<f64>() / values.len() as f64;
            let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / values.len() as f64;
            if var > 1e-12 {
                kept.push(col);
                means.push(mean);
                sds.push(var.sqrt());
            }
        }
        let mut censor_times: Vec<i64> = usable
            .iter()
            .map(|r| (cutoff - r.snapshot.as_of).num_seconds())
            .collect();
        censor_times.sort_unstable();
        let mut model = QrModel {
            settings,
            cutoff,
            rows: usable.len(),
            landed: 0,
            issues: per_issue.len(),
            stage_vocab,
            kept,
            means,
            sds,
            censor_times,
            coef: None,
        };
        let mut xs = Vec::new();
        let mut ys = Vec::new();
        let mut ws = Vec::new();
        for row in &usable {
            let Label::Landed { remaining_sec, .. } = row.label else {
                continue;
            };
            let Some(x) = model.design(row) else { continue };
            let g = model.survival(remaining_sec).max(G_FLOOR);
            xs.push(x);
            ys.push((remaining_sec as f64 + LOG_OFFSET_SEC).ln());
            ws.push(1.0 / (per_issue[row.snapshot.story.as_str()] as f64 * g));
        }
        model.landed = ys.len();
        if model.landed >= MIN_LANDED {
            let ridge = f64::from(settings.ridge_milli) / 1_000.0;
            let fits: Option<Vec<Vec<f64>>> = LEVELS
                .iter()
                .map(|tau| fit_level(&xs, &ys, &ws, *tau, ridge))
                .collect();
            model.coef = fits.and_then(|f| <[Vec<f64>; 3]>::try_from(f).ok());
        }
        Ok(model)
    }

    /// Remaining-time quartiles for `row`, seconds, or `None` when the model
    /// was not fitted or never saw the row's stage. Quantile crossing is
    /// removed by sorting.
    #[must_use]
    pub fn predict(&self, row: &Row) -> Option<(i64, i64, i64)> {
        let coef = self.coef.as_ref()?;
        let x = self.design(row)?;
        let mut q: Vec<f64> = coef.iter().map(|b| dot(b, &x)).collect();
        q.sort_by(f64::total_cmp);
        let secs = |z: f64| ((z.exp() - LOG_OFFSET_SEC).max(0.0)).round() as i64;
        Some((secs(q[0]), secs(q[1]), secs(q[2])))
    }
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Solve `(Σ c_i x_i x_iᵀ + Λ) β = Σ x_i d_i`, Λ the ridge on every
/// coefficient but the intercept, by Cholesky. `None` if not positive
/// definite.
fn solve_weighted(xs: &[Vec<f64>], c: &[f64], d: &[f64], ridge: f64) -> Option<Vec<f64>> {
    let p = xs.first()?.len();
    let mut a = vec![vec![0.0_f64; p]; p];
    let mut b = vec![0.0_f64; p];
    for ((x, ci), di) in xs.iter().zip(c).zip(d) {
        for j in 0..p {
            b[j] += x[j] * di;
            for k in 0..=j {
                a[j][k] += ci * x[j] * x[k];
            }
        }
    }
    for (j, row) in a.iter_mut().enumerate().skip(1) {
        row[j] += ridge;
    }
    // Cholesky, lower triangle in place.
    for j in 0..p {
        let diag = a[j][j] - dot(&a[j][..j], &a[j][..j]);
        if diag <= 1e-12 {
            return None;
        }
        let diag = diag.sqrt();
        a[j][j] = diag;
        for i in (j + 1)..p {
            let v = a[i][j] - dot(&a[i][..j], &a[j][..j]);
            a[i][j] = v / diag;
        }
    }
    let mut z = vec![0.0_f64; p];
    for i in 0..p {
        let mut v = b[i];
        for k in 0..i {
            v -= a[i][k] * z[k];
        }
        z[i] = v / a[i][i];
    }
    let mut beta = vec![0.0_f64; p];
    for i in (0..p).rev() {
        let mut v = z[i];
        for k in (i + 1)..p {
            v -= a[k][i] * beta[k];
        }
        beta[i] = v / a[i][i];
    }
    Some(beta)
}

/// One level's coefficients: weighted ridge least squares, then MM steps on
/// the weighted pinball loss plus `ridge · Σw · ‖β₋₀‖²`.
fn fit_level(xs: &[Vec<f64>], ys: &[f64], ws: &[f64], tau: f64, ridge: f64) -> Option<Vec<f64>> {
    let total: f64 = ws.iter().sum();
    let wy: Vec<f64> = ws.iter().zip(ys).map(|(w, y)| w * y).collect();
    let mut beta = solve_weighted(xs, ws, &wy, ridge * total)?;
    for _ in 0..MM_ITERATIONS {
        let mut c = Vec::with_capacity(ys.len());
        let mut d = Vec::with_capacity(ys.len());
        for ((x, y), w) in xs.iter().zip(ys).zip(ws) {
            let a = MM_EPSILON + (y - dot(&beta, x)).abs();
            c.push(w / a);
            d.push(w * (y / a + 2.0 * tau - 1.0));
        }
        // The majoriser's normal equations carry the penalty `μ‖β₋₀‖²` as
        // `4μ` on the diagonal (the surrogate is a quarter of `r²/a`).
        let next = solve_weighted(xs, &c, &d, 4.0 * ridge * total)?;
        let moved = next
            .iter()
            .zip(&beta)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0_f64, f64::max);
        beta = next;
        if moved < MM_TOLERANCE {
            break;
        }
    }
    beta.iter().all(|b| b.is_finite()).then_some(beta)
}
