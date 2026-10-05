//! The per-stage exit hazard: L2-regularized logistic regression of "left
//! the stage within the next 30 minutes" on standardized features, fitted by
//! Newton (IRLS).
//!
//! Objective (the #10223 fixture's `objectives.hazard`, verbatim — a **sum**,
//! not a mean): `Σᵢ ln(1 + exp(−sᵢ(w·zᵢ + b))) + ‖w‖²/(2C)` with
//! `C = HAZARD_C`, `b` unpenalized and `sᵢ = ±1` (exited / not). With
//! `A = [Z | 1]`, the gradient is `Aᵀ(p − y) + [w/C; 0]` and the Hessian
//! `Aᵀ diag(p(1−p)) A + diag(1/C, …, 1/C, 0)`.

use super::coeffs::{HazardFit, HazardSkip, SkipReason};
use super::math::{
    mirror_upper, newton, sigmoid, softplus, standardization, standardize, Derivatives, Stop,
};
use super::{model_features, TrainingRow, HAZARD_C, MIN_STAGE_EXITS, MIN_STAGE_ROWS, N_FEATURES};

/// Newton stops once a full step is below this (`‖·‖∞`).
const STEP_TOL: f64 = 1e-12;

/// Fit one stage's hazard from `rows` — that stage's rows with an exit label;
/// any row without one is ignored. A stage with fewer than
/// [`MIN_STAGE_ROWS`] labelled rows, or fewer than [`MIN_STAGE_EXITS`] exits,
/// is refused with the gate it failed.
///
/// # Errors
///
/// The [`HazardSkip`] naming the failed gate.
pub fn fit_stage(rows: &[&TrainingRow]) -> Result<HazardFit, HazardSkip> {
    let labelled: Vec<(&TrainingRow, bool)> = rows
        .iter()
        .filter_map(|r| r.exit.map(|e| (*r, e)))
        .collect();
    let n = labelled.len();
    let exits = labelled.iter().filter(|(_, e)| *e).count();
    let skip = |reason| HazardSkip {
        reason,
        rows: n,
        exits,
    };
    if n < MIN_STAGE_ROWS {
        return Err(skip(SkipReason::BelowMinRows));
    }
    if exits < MIN_STAGE_EXITS {
        return Err(skip(SkipReason::BelowMinExits));
    }

    let xs: Vec<[f64; N_FEATURES]> = labelled
        .iter()
        .map(|(r, _)| model_features(&r.inputs))
        .collect();
    let (mu, sd) = standardization(&xs);
    let zs: Vec<[f64; N_FEATURES]> = xs.iter().map(|x| standardize(x, &mu, &sd)).collect();
    let ys: Vec<bool> = labelled.iter().map(|(_, e)| *e).collect();

    // Start from w = 0 and b = logit(exit rate); the rate is kept off 0 and 1
    // so an all-exit stage still starts finite.
    let half = 0.5 / n as f64;
    let rate = (exits as f64 / n as f64).clamp(half, 1.0 - half);
    let mut x0 = vec![0.0; N_FEATURES + 1];
    x0[N_FEATURES] = (rate / (1.0 - rate)).ln();

    let stop = Stop {
        grad_tol: 0.0,
        step_tol: STEP_TOL,
    };
    let min = newton(
        x0,
        stop,
        |theta| derivatives(&zs, &ys, theta),
        |theta| objective(&zs, &ys, theta),
    );
    Ok(HazardFit {
        mu,
        sd,
        coef: min.x[..N_FEATURES].to_vec(),
        intercept: min.x[N_FEATURES],
        rows: n,
        exits,
        objective: min.objective,
        iterations: min.iterations,
        converged: min.converged,
    })
}

/// `w·z + b` for `theta = [w; b]`.
fn linear(z: &[f64; N_FEATURES], theta: &[f64]) -> f64 {
    z.iter().zip(theta).map(|(a, b)| a * b).sum::<f64>() + theta[N_FEATURES]
}

/// `‖w‖²/(2C)`.
fn penalty(theta: &[f64]) -> f64 {
    theta[..N_FEATURES].iter().map(|w| w * w).sum::<f64>() / (2.0 * HAZARD_C)
}

/// The objective alone.
fn objective(zs: &[[f64; N_FEATURES]], ys: &[bool], theta: &[f64]) -> f64 {
    let loss: f64 = zs
        .iter()
        .zip(ys)
        .map(|(z, &y)| {
            let a = linear(z, theta);
            softplus(if y { -a } else { a })
        })
        .sum();
    loss + penalty(theta)
}

/// The objective, its gradient and its Hessian.
fn derivatives(zs: &[[f64; N_FEATURES]], ys: &[bool], theta: &[f64]) -> Derivatives {
    let p = N_FEATURES + 1;
    let mut f = 0.0;
    let mut g = vec![0.0; p];
    let mut h = vec![0.0; p * p];
    let mut row = [0.0; N_FEATURES + 1];
    for (z, &y) in zs.iter().zip(ys) {
        row[..N_FEATURES].copy_from_slice(z);
        row[N_FEATURES] = 1.0;
        let a = linear(z, theta);
        f += softplus(if y { -a } else { a });
        let prob = sigmoid(a);
        let resid = prob - if y { 1.0 } else { 0.0 };
        let weight = prob * (1.0 - prob);
        for i in 0..p {
            g[i] += resid * row[i];
            let wi = weight * row[i];
            for j in i..p {
                h[i * p + j] += wi * row[j];
            }
        }
    }
    f += penalty(theta);
    for i in 0..N_FEATURES {
        g[i] += theta[i] / HAZARD_C;
        h[i * p + i] += 1.0 / HAZARD_C;
    }
    mirror_upper(&mut h, p);
    (f, g, h)
}
