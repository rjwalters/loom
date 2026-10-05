//! The pooled direct model: a censoring-aware log-normal accelerated-failure-
//! time regression of time-to-merge, fitted by damped Newton.
//!
//! - **Stages:** those with at least [`MIN_STAGE_ROWS`] rows and
//!   [`MIN_STAGE_EXITS`] merge events, in [`FitStage`] order; other stages'
//!   rows are excluded. (Curator's choice on #10221: the issue gates only the
//!   hazard, and a stage whose rows are all censored has no finite optimum.)
//! - **Design:** `Z = [stage one-hot | standardized features]`, with `mu`/`sd`
//!   unweighted and pooled over the included rows; `y = ln(max(dur_h,
//!   MIN_DUR_H))`; `r = (y − Z·β) / σ_k`, `σ_k = exp(log_sigma[k])`.
//! - **Weights:** `wᵢ = 1 / (included rows in its group)`, `W = Σwᵢ`, so every
//!   PR counts once however long it stayed open.
//! - **Objective:** `(1/W)·Σ wᵢnᵢ + AFT_L2·‖β_features‖²`, with
//!   `nᵢ = r²/2 + ½ln 2π + ln σ_k` for a merge and `−ln Φ̄(rᵢ)` for a censored
//!   row. With every `wᵢ = 1` it is exactly the #10223 fixture's
//!   `objectives.aft`.
//! - **Newton:** from a warm start (stage intercepts = per-stage mean of `y`,
//!   `log σ` = per-stage `ln std(y)`, features 0) — an all-zero start
//!   diverges, its Hessian indefinite from the first step — with Levenberg
//!   damping and Armijo backtracking; stops at `‖g‖∞ < 1e-10` or a full step
//!   under `1e-13`.

use std::collections::BTreeMap;

use super::coeffs::AftFit;
use super::math::{
    mirror_upper, newton, norm_logpdf, norm_logsf, standardization, standardize, Derivatives, Stop,
};
use super::{
    model_features, FitStage, TrainingRow, AFT_L2, MIN_DUR_H, MIN_STAGE_EXITS, MIN_STAGE_ROWS,
    N_FEATURES,
};

/// Converged once `‖gradient‖∞` is below this.
const GRAD_TOL: f64 = 1e-10;

/// Converged once a full step is below this (`‖·‖∞`).
const STEP_TOL: f64 = 1e-13;

/// One included row, ready for the likelihood.
struct Obs {
    /// Index into the model's stages.
    k: usize,
    /// Standardized features.
    z: [f64; N_FEATURES],
    /// `ln(max(dur_h, MIN_DUR_H))`.
    y: f64,
    /// A merge (not censored).
    event: bool,
    /// `1 / (included rows in its group)`.
    w: f64,
}

/// The likelihood over the included rows. Parameters are
/// `[K stage intercepts, 20 feature coefficients, K log σ]`.
struct Problem {
    stages: usize,
    obs: Vec<Obs>,
    total_weight: f64,
}

/// The stages that pass the direct model's gate, in [`FitStage`] order.
#[must_use]
pub fn eligible_stages(rows: &[TrainingRow]) -> Vec<FitStage> {
    FitStage::ALL
        .into_iter()
        .filter(|stage| {
            let of_stage = rows.iter().filter(|r| r.stage == *stage);
            let n = of_stage.clone().count();
            let events = of_stage.filter(|r| r.merge.merged).count();
            n >= MIN_STAGE_ROWS && events >= MIN_STAGE_EXITS
        })
        .collect()
}

/// Fit the direct model over `rows`, in the order given. `None` when no stage
/// passes the gate.
#[must_use]
pub fn fit(rows: &[TrainingRow]) -> Option<AftFit> {
    let stages = eligible_stages(rows);
    if stages.is_empty() {
        return None;
    }
    let included: Vec<(usize, &TrainingRow)> = rows
        .iter()
        .filter_map(|r| stages.iter().position(|s| *s == r.stage).map(|k| (k, r)))
        .collect();
    let mut group_rows: BTreeMap<&str, usize> = BTreeMap::new();
    for (_, r) in &included {
        *group_rows.entry(r.group.as_str()).or_default() += 1;
    }
    let xs: Vec<[f64; N_FEATURES]> = included
        .iter()
        .map(|(_, r)| model_features(&r.inputs))
        .collect();
    let (mu, sd) = standardization(&xs);
    let obs: Vec<Obs> = included
        .iter()
        .zip(&xs)
        .map(|((k, r), x)| Obs {
            k: *k,
            z: standardize(x, &mu, &sd),
            y: r.merge.dur_h.max(MIN_DUR_H).ln(),
            event: r.merge.merged,
            w: 1.0 / group_rows[r.group.as_str()] as f64,
        })
        .collect();
    let problem = Problem {
        stages: stages.len(),
        total_weight: obs.iter().map(|o| o.w).sum(),
        obs,
    };
    let stop = Stop {
        grad_tol: GRAD_TOL,
        step_tol: STEP_TOL,
    };
    let min = newton(
        problem.warm_start(),
        stop,
        |theta| problem.derivatives(theta),
        |theta| problem.objective(theta),
    );
    let nb = problem.stages + N_FEATURES;
    Some(AftFit {
        stages,
        mu,
        sd,
        beta: min.x[..nb].to_vec(),
        log_sigma: min.x[nb..].to_vec(),
        objective: min.objective,
        converged: min.converged,
        rows: problem.obs.len(),
        events: problem.obs.iter().filter(|o| o.event).count(),
        groups: group_rows.len(),
        iterations: min.iterations,
    })
}

impl Problem {
    /// Stage intercepts plus feature coefficients.
    fn n_beta(&self) -> usize {
        self.stages + N_FEATURES
    }

    /// Stage intercepts = per-stage mean of `y`; `log σ` = per-stage
    /// `ln std(y)` (population); feature coefficients 0.
    fn warm_start(&self) -> Vec<f64> {
        let nb = self.n_beta();
        let mut x = vec![0.0; nb + self.stages];
        for k in 0..self.stages {
            let ys: Vec<f64> = self.obs.iter().filter(|o| o.k == k).map(|o| o.y).collect();
            if ys.is_empty() {
                continue;
            }
            let n = ys.len() as f64;
            let mean = ys.iter().sum::<f64>() / n;
            let var = ys.iter().map(|y| (y - mean).powi(2)).sum::<f64>() / n;
            x[k] = mean;
            x[nb + k] = if var > 0.0 { 0.5 * var.ln() } else { 0.0 };
        }
        x
    }

    /// `(r, log σ_k)` for one row.
    fn residual(&self, o: &Obs, theta: &[f64]) -> (f64, f64) {
        let nb = self.n_beta();
        let eta = theta[o.k]
            + o.z
                .iter()
                .zip(&theta[self.stages..nb])
                .map(|(a, b)| a * b)
                .sum::<f64>();
        let log_sigma = theta[nb + o.k];
        ((o.y - eta) / log_sigma.exp(), log_sigma)
    }

    /// `AFT_L2·‖β_features‖²`.
    fn penalty(&self, theta: &[f64]) -> f64 {
        AFT_L2
            * theta[self.stages..self.n_beta()]
                .iter()
                .map(|b| b * b)
                .sum::<f64>()
    }

    /// The objective alone.
    fn objective(&self, theta: &[f64]) -> f64 {
        let sum: f64 = self
            .obs
            .iter()
            .map(|o| {
                let (r, log_sigma) = self.residual(o, theta);
                o.w * neg_log_lik(o.event, r, log_sigma)
            })
            .sum();
        sum / self.total_weight + self.penalty(theta)
    }

    /// The objective, its gradient and its Hessian. Per row, with `D = r`,
    /// `D₂ = 1` for a merge and `D = λ = φ(r)/Φ̄(r)`, `D₂ = λ(λ − r)` for a
    /// censored row: `∂n/∂β = −D·z/σ`, `∂n/∂ℓ = −D·r + [merge]`,
    /// `∂²n/∂β² = D₂·zzᵀ/σ²`, `∂²n/∂β∂ℓ = (D₂·r + D)·z/σ`,
    /// `∂²n/∂ℓ² = D₂·r² + D·r`.
    fn derivatives(&self, theta: &[f64]) -> Derivatives {
        let ks = self.stages;
        let nb = self.n_beta();
        let p = nb + ks;
        let mut sum = 0.0;
        let mut g = vec![0.0; p];
        let mut h = vec![0.0; p * p];
        let mut z = vec![0.0; nb];
        for o in &self.obs {
            let (r, log_sigma) = self.residual(o, theta);
            let sigma = log_sigma.exp();
            let (d, d2) = if o.event {
                (r, 1.0)
            } else {
                let lambda = (norm_logpdf(r) - norm_logsf(r)).exp();
                (lambda, lambda * (lambda - r))
            };
            sum += o.w * neg_log_lik(o.event, r, log_sigma);

            z.fill(0.0);
            z[o.k] = 1.0;
            z[ks..nb].copy_from_slice(&o.z);
            let l = nb + o.k;
            let g_beta = -o.w * d / sigma;
            let h_beta = o.w * d2 / (sigma * sigma);
            let h_cross = o.w * (d2 * r + d) / sigma;
            for i in 0..nb {
                g[i] += g_beta * z[i];
                let a = h_beta * z[i];
                for j in i..nb {
                    h[i * p + j] += a * z[j];
                }
                h[i * p + l] += h_cross * z[i];
            }
            g[l] += o.w * (-d * r + if o.event { 1.0 } else { 0.0 });
            h[l * p + l] += o.w * (d2 * r * r + d * r);
        }
        let inv = 1.0 / self.total_weight;
        let f = sum / self.total_weight + self.penalty(theta);
        for v in &mut g {
            *v *= inv;
        }
        for v in &mut h {
            *v *= inv;
        }
        for i in ks..nb {
            g[i] += 2.0 * AFT_L2 * theta[i];
            h[i * p + i] += 2.0 * AFT_L2;
        }
        mirror_upper(&mut h, p);
        (f, g, h)
    }
}

/// One row's negative log-likelihood: `r²/2 + ½ln 2π + ln σ` for a merge,
/// `−ln Φ̄(r)` for a censored row.
fn neg_log_lik(event: bool, r: f64, log_sigma: f64) -> f64 {
    if event {
        log_sigma - norm_logpdf(r)
    } else {
        -norm_logsf(r)
    }
}
