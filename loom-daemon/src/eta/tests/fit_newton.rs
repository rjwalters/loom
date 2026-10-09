//! The Newton driver's stopping rule (#10501): a fit at the floating-point
//! noise floor reports converged, and one without a finite optimum still
//! does not.
//!
//! On the real 2026-09-30..10-06 fleet rows, three of ten daily fits ran to
//! the 100-iteration cap and were flagged `converged: false`. By iteration 6
//! to 8 each Newton step's predicted decrease was 1e-17..1e-15 of `|f|`,
//! which is below the rounding noise of a 13k-45k-row sum. From there the
//! line search could only accept noise-sized fractions of an accurate step,
//! and the absolute step and gradient tolerances were out of reach.

use crate::eta::fit::logistic::fit_stage_x;
use crate::eta::fit::math::{
    newton, sigmoid, softplus, Derivatives, Stop, DECREASE_RTOL, STEP_RTOL,
};
use crate::eta::fit::{FitStage, MergeLabel, ModelInputs, TrainingRow, MAX_NEWTON_ITERATIONS};
use crate::eta::simulate::SplitMix64;

/// The rule before #10501: absolute tolerances only.
const OLD: Stop = Stop {
    grad_tol: 0.0,
    step_tol: 1e-12,
    decrease_rtol: 0.0,
    step_rtol: 0.0,
};

/// The rule the fits use now.
const NEW: Stop = Stop {
    grad_tol: 0.0,
    step_tol: 1e-12,
    decrease_rtol: DECREASE_RTOL,
    step_rtol: STEP_RTOL,
};

/// A smooth, strictly convex, non-quadratic objective with a coupled
/// Hessian, of real-fit scale (`f ≈ 1000`), in plain arithmetic so it is
/// bit-identical on every platform: `1000 + Σⱼ (aⱼxⱼ⁴/4 + bⱼxⱼ²/2 − cⱼxⱼ) +
/// κ(Σⱼxⱼ)²/2`.
struct Quartic {
    a: [f64; 3],
    b: [f64; 3],
    c: [f64; 3],
    kappa: f64,
}

impl Quartic {
    const P: Quartic = Quartic {
        a: [2.0, 0.5, 1.0],
        b: [1.0, 3.0, 0.5],
        c: [4.0, -2.0, 1.5],
        kappa: 0.7,
    };

    fn value(&self, x: &[f64]) -> f64 {
        let s: f64 = x.iter().sum();
        let mut f = 1000.0 + 0.5 * self.kappa * s * s;
        for (j, xj) in x.iter().enumerate() {
            f += self.a[j] * xj.powi(4) / 4.0 + self.b[j] * xj * xj / 2.0 - self.c[j] * xj;
        }
        f
    }

    fn gradient(&self, x: &[f64]) -> Vec<f64> {
        let s: f64 = x.iter().sum();
        (0..3)
            .map(|j| self.a[j] * x[j].powi(3) + self.b[j] * x[j] - self.c[j] + self.kappa * s)
            .collect()
    }

    fn derivatives(&self, x: &[f64]) -> Derivatives {
        let mut h = vec![self.kappa; 9];
        for j in 0..3 {
            h[j * 3 + j] += 3.0 * self.a[j] * x[j] * x[j] + self.b[j];
        }
        (self.value(x), self.gradient(x), h)
    }
}

/// Every Newton step's decrease below `1e-14·|f|` is invisible to the line
/// search. This models a sum whose rounding noise is that large (the worst
/// real floor was about `1e-15`): the line-search evaluation of the
/// objective reads that much high.
#[test]
fn a_fit_at_the_noise_floor_converges_with_the_same_minimizer() {
    let q = Quartic::P;
    let blur = |x: &[f64]| {
        let f = q.value(x);
        f + 1e-14 * f.abs()
    };
    let x0 = vec![0.0; 3];

    let old = newton(x0.clone(), OLD, |x| q.derivatives(x), blur);
    assert!(
        !old.converged,
        "the old rule stalls at the floor: {} iterations",
        old.iterations
    );

    let new = newton(x0.clone(), NEW, |x| q.derivatives(x), blur);
    assert!(new.converged, "{} iterations", new.iterations);
    assert!(new.iterations <= 15, "{} iterations", new.iterations);

    // The minimizer itself: the exact objective, run to a full step under
    // 1e-12 (the gradient there is ~1e-15).
    let exact = newton(x0, OLD, |x| q.derivatives(x), |x| q.value(x));
    assert!(exact.converged, "{} iterations", exact.iterations);
    let dist = |x: &[f64]| {
        x.iter()
            .zip(&exact.x)
            .fold(0.0_f64, |m, (a, b)| m.max((a - b).abs()))
    };
    // The final full Newton step lands within about its square.
    assert!(dist(&new.x) < 1e-9, "new rule off by {:e}", dist(&new.x));
    // The stalled run was within its last noise-sized step, so the reported
    // coefficients move by less than the issue's 1e-6.
    assert!(dist(&old.x) < 1e-6, "stalled run off by {:e}", dist(&old.x));
}

/// One labelled hazard row; only `exit` is read by the fit.
fn row(exit: bool) -> TrainingRow {
    TrainingRow {
        stage: FitStage::ReviewWait,
        group: String::new(),
        inputs: ModelInputs::default(),
        starred_any: None,
        star_source: None,
        planner_version: None,
        exit: Some(exit),
        merge: MergeLabel::default(),
    }
}

/// Real-scale penalized hazard fits on correlated features, one per seed.
/// Their objectives are sums over 16,000 rows, like the 12-16k-row stages
/// that stalled. They converge in a handful of iterations; under the old
/// rule seed 10501 crawled at the floor for 36 (measured on x86-64 Linux),
/// as the real fits did for 45-100.
#[test]
fn large_hazard_fits_converge_promptly() {
    const N: usize = 12;
    const ROWS: usize = 16_000;
    for seed in [10_501_u64, 7, 2026, 31_337] {
        let mut rng = SplitMix64::new(seed);
        let labelled: Vec<(TrainingRow, [f64; N])> = (0..ROWS)
            .map(|_| {
                // A shared factor makes the features near-collinear.
                let common = 3.0 * rng.next_f64();
                let x: [f64; N] = std::array::from_fn(|j| {
                    common + 0.3 * rng.next_f64() + if j % 3 == 0 { 2.0 } else { 0.0 }
                });
                let a = -3.0 + 0.8 * (x[0] - x[1]) + 0.4 * x[2] - 0.3 * x[5];
                let exit = rng.next_f64() < sigmoid(a);
                (row(exit), x)
            })
            .collect();
        let rows: Vec<(&TrainingRow, [f64; N])> = labelled.iter().map(|(r, x)| (r, *x)).collect();
        let fit = fit_stage_x(&rows).unwrap();
        assert!(
            fit.converged && fit.iterations <= 15,
            "seed {seed}: converged {} after {} iterations",
            fit.converged,
            fit.iterations
        );
        assert!(fit.objective.is_finite(), "seed {seed}");
    }
}

/// Unpenalized logistic loss on separable data has no finite minimizer:
/// the coefficients grow every step while `f → 0`. The decrement soon falls
/// below `DECREASE_RTOL`, but the step stays large relative to `x`, so the
/// run must still end unconverged at the cap.
#[test]
fn a_fit_without_a_finite_optimum_is_still_flagged() {
    // (z, label): every positive has z₀ > 0, every negative z₀ < 0.
    let data: Vec<([f64; 2], bool)> = (0..40)
        .map(|i| {
            let t = f64::from(i) / 40.0;
            let label = i % 2 == 0;
            let z0 = if label { 0.5 + t } else { -0.5 - t };
            ([z0, (7.0 * t).sin()], label)
        })
        .collect();
    let objective = |w: &[f64]| -> f64 {
        data.iter()
            .map(|(z, y)| {
                let a = w[0] * z[0] + w[1] * z[1];
                softplus(if *y { -a } else { a })
            })
            .sum()
    };
    let derivatives = |w: &[f64]| -> Derivatives {
        let mut g = vec![0.0; 2];
        let mut h = vec![0.0; 4];
        for (z, y) in &data {
            let a = w[0] * z[0] + w[1] * z[1];
            let p = sigmoid(a);
            let r = p - if *y { 1.0 } else { 0.0 };
            for i in 0..2 {
                g[i] += r * z[i];
                for j in 0..2 {
                    h[i * 2 + j] += p * (1.0 - p) * z[i] * z[j];
                }
            }
        }
        (objective(w), g, h)
    };
    let min = newton(vec![0.0; 2], NEW, derivatives, objective);
    assert!(!min.converged, "diverging fit reported converged: {min:?}");
    assert_eq!(min.iterations, MAX_NEWTON_ITERATIONS);
    assert!(min.objective < 1e-13, "it did keep descending: {}", min.objective);
}

/// Near a maximum the Hessian is negative definite, so the step is a
/// Levenberg-damped one. Its `−g·d` is tiny, yet it is no Newton decrement:
/// the noise-floor stop only trusts undamped steps.
#[test]
fn a_damped_step_never_triggers_the_noise_floor_stop() {
    // f = −x²/2 from just beside its maximum at 0: unbounded below.
    let min = newton(
        vec![1e-10],
        NEW,
        |x| (-0.5 * x[0] * x[0], vec![-x[0]], vec![-1.0]),
        |x| -0.5 * x[0] * x[0],
    );
    assert!(!min.converged, "stopped at a maximum: {min:?}");
    assert_eq!(min.iterations, MAX_NEWTON_ITERATIONS);
}
