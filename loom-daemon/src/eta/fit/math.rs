//! Hand-ported numerics for the fits: the normal distribution's tail and
//! quantile functions, a dense Cholesky solve, standardization, and the
//! damped Newton driver both fits share.
//!
//! Hand-ported rather than a new crate on purpose: `loom-daemon/Cargo.toml`
//! is a Champion auto-merge veto pattern, and none of this is large.
//!
//! - [`erfc`] is a port of fdlibm's `s_erf.c` (the FreeBSD/musl lineage),
//!   whose error is below 1 ulp; its Sun notice is preserved below.
//! - [`probit`] is Wichura's AS241 (`PPND16`), relative accuracy about 1e-16.

use std::f64::consts::{FRAC_1_SQRT_2, TAU};

use super::{MAX_NEWTON_ITERATIONS, STD_EPS};

/// The rational-approximation coefficients, in one scope so the precision
/// allow covers these tables only.
#[allow(clippy::excessive_precision, clippy::unreadable_literal)]
mod coef {
    // ====================================================
    // Copyright (C) 1993 by Sun Microsystems, Inc. All rights reserved.
    //
    // Developed at SunPro, a Sun Microsystems, Inc. business.
    // Permission to use, copy, modify, and distribute this
    // software is freely granted, provided that this notice
    // is preserved.
    // ====================================================
    //
    // fdlibm s_erf.c, erfc(x).

    pub(super) const ERX: f64 = 8.45062911510467529297e-01;
    // |x| < 0.84375
    pub(super) const PP: [f64; 5] = [
        1.28379167095512558561e-01,
        -3.25042107247001499370e-01,
        -2.84817495755985104766e-02,
        -5.77027029648944159157e-03,
        -2.37630166566501626084e-05,
    ];
    pub(super) const QQ: [f64; 5] = [
        3.97917223959155352819e-01,
        6.50222499887672944485e-02,
        5.08130628187576562776e-03,
        1.32494738004321644526e-04,
        -3.96022827877536812320e-06,
    ];
    // 0.84375 <= |x| < 1.25
    pub(super) const PA: [f64; 7] = [
        -2.36211856075265944077e-03,
        4.14856118683748331666e-01,
        -3.72207876035701323847e-01,
        3.18346619901161753674e-01,
        -1.10894694282396677476e-01,
        3.54783043256182359371e-02,
        -2.16637559486879084300e-03,
    ];
    pub(super) const QA: [f64; 6] = [
        1.06420880400844228286e-01,
        5.40397917702171048937e-01,
        7.18286544141962662868e-02,
        1.26171219808761642112e-01,
        1.36370839120290507362e-02,
        1.19844998467991074170e-02,
    ];
    // 1.25 <= |x| < 1/0.35
    pub(super) const RA: [f64; 8] = [
        -9.86494403484714822705e-03,
        -6.93858572707181764372e-01,
        -1.05586262253232909814e+01,
        -6.23753324503260060396e+01,
        -1.62396669462573470355e+02,
        -1.84605092906711035994e+02,
        -8.12874355063065934246e+01,
        -9.81432934416914548592e+00,
    ];
    pub(super) const SA: [f64; 8] = [
        1.96512716674392571292e+01,
        1.37657754143519042600e+02,
        4.34565877475229228821e+02,
        6.45387271733267880336e+02,
        4.29008140027567833386e+02,
        1.08635005541779435134e+02,
        6.57024977031928170135e+00,
        -6.04244152148580987438e-02,
    ];
    // 1/0.35 <= |x| < 28
    pub(super) const RB: [f64; 7] = [
        -9.86494292470009928597e-03,
        -7.99283237680523006574e-01,
        -1.77579549177547519889e+01,
        -1.60636384855821916062e+02,
        -6.37566443368389627722e+02,
        -1.02509513161107724954e+03,
        -4.83519191608651397019e+02,
    ];
    pub(super) const SB: [f64; 7] = [
        3.03380607434824582924e+01,
        3.25792512996573918826e+02,
        1.53672958608443695994e+03,
        3.19985821950859553908e+03,
        2.55305040643316442583e+03,
        4.74528541206955367215e+02,
        -2.24409524465858183362e+01,
    ];

    // Wichura (1988), AS241 PPND16. Numerators then denominators, constant
    // term first; each denominator's constant term is 1.
    // |q| <= 0.425
    pub(super) const A: [f64; 8] = [
        3.3871328727963666080e0,
        1.3314166789178437745e+2,
        1.9715909503065514427e+3,
        1.3731693765509461125e+4,
        4.5921953931549871457e+4,
        6.7265770927008700853e+4,
        3.3430575583588128105e+4,
        2.5090809287301226727e+3,
    ];
    pub(super) const B: [f64; 7] = [
        4.2313330701600911252e+1,
        6.8718700749205790830e+2,
        5.3941960214247511077e+3,
        2.1213794301586595867e+4,
        3.9307895800092710610e+4,
        2.8729085735721942674e+4,
        5.2264952788528545610e+3,
    ];
    // r <= 5
    pub(super) const C: [f64; 8] = [
        1.42343711074968357734e0,
        4.63033784615654529590e0,
        5.76949722146069140550e0,
        3.64784832476320460504e0,
        1.27045825245236838258e0,
        2.41780725177450611770e-1,
        2.27238449892691845833e-2,
        7.74545014278341407640e-4,
    ];
    pub(super) const D: [f64; 7] = [
        2.05319162663775882187e0,
        1.67638483018380384940e0,
        6.89767334985100004550e-1,
        1.48103976427480074590e-1,
        1.51986665636164571966e-2,
        5.47593808499534494600e-4,
        1.05075007164441684324e-9,
    ];
    // r > 5
    pub(super) const E: [f64; 8] = [
        6.65790464350110377720e0,
        5.46378491116411436990e0,
        1.78482653991729133580e0,
        2.96560571828504891230e-1,
        2.65321895265761230930e-2,
        1.24266094738807843860e-3,
        2.71155556874348757815e-5,
        2.01033439929228813265e-7,
    ];
    pub(super) const F: [f64; 7] = [
        5.99832206555887937690e-1,
        1.36929880922735805310e-1,
        1.48753612908506148525e-2,
        7.86869131145613259100e-4,
        1.84631831751005468180e-5,
        1.42151175831644588870e-7,
        2.04426310338993978564e-15,
    ];
}

/// `c[0] + x·(c[1] + x·(c[2] + …))`, Horner order.
fn poly(c: &[f64], x: f64) -> f64 {
    c.iter().rev().fold(0.0, |acc, &k| acc * x + k)
}

/// `1 + x·(c[0] + x·(c[1] + …))`: a denominator whose constant term is 1.
fn poly1(c: &[f64], x: f64) -> f64 {
    1.0 + x * poly(c, x)
}

/// The complementary error function, full relative accuracy (fdlibm).
#[must_use]
pub fn erfc(x: f64) -> f64 {
    use coef::{ERX, PA, PP, QA, QQ, RA, RB, SA, SB};
    // The IEEE high word, signed, as fdlibm reads it.
    let hx = (x.to_bits() >> 32) as u32 as i32;
    let ix = hx & 0x7fff_ffff;
    if ix >= 0x7ff0_0000 {
        // erfc(NaN) = NaN, erfc(+inf) = 0, erfc(-inf) = 2.
        return if x.is_nan() {
            x
        } else if x > 0.0 {
            0.0
        } else {
            2.0
        };
    }
    if ix < 0x3feb_0000 {
        // |x| < 0.84375
        if ix < 0x3c70_0000 {
            // |x| < 2**-56
            return 1.0 - x;
        }
        let z = x * x;
        let y = poly(&PP, z) / poly1(&QQ, z);
        if hx < 0x3fd0_0000 {
            // x < 1/4
            return 1.0 - (x + x * y);
        }
        let r = x * y + (x - 0.5);
        return 0.5 - r;
    }
    if ix < 0x3ff4_0000 {
        // 0.84375 <= |x| < 1.25
        let s = x.abs() - 1.0;
        let p = poly(&PA, s);
        let q = poly1(&QA, s);
        return if hx >= 0 {
            (1.0 - ERX) - p / q
        } else {
            1.0 + (ERX + p / q)
        };
    }
    if ix < 0x403c_0000 {
        // |x| < 28
        let ax = x.abs();
        let s = 1.0 / (ax * ax);
        let (r, ss) = if ix < 0x4006_db6d {
            // |x| < 1/0.35
            (poly(&RA, s), poly1(&SA, s))
        } else {
            if hx < 0 && ix >= 0x4018_0000 {
                // x < -6
                return 2.0;
            }
            (poly(&RB, s), poly1(&SB, s))
        };
        // ax with its low word cleared, so z*z is exact.
        let z = f64::from_bits(ax.to_bits() & 0xffff_ffff_0000_0000);
        let e = (-z * z - 0.5625).exp() * ((z - ax) * (z + ax) + r / ss).exp();
        return if hx > 0 { e / ax } else { 2.0 - e / ax };
    }
    if hx > 0 {
        0.0
    } else {
        2.0
    }
}

/// The standard normal CDF, `Φ(r) = ½·erfc(−r/√2)`.
#[must_use]
pub fn norm_cdf(r: f64) -> f64 {
    0.5 * erfc(-r * FRAC_1_SQRT_2)
}

/// `ln φ(r)`, the standard normal log density.
#[must_use]
pub fn norm_logpdf(r: f64) -> f64 {
    -0.5 * r * r - 0.5 * TAU.ln()
}

/// `ln Φ̄(r) = ln(1 − Φ(r))`, accurate in both tails:
/// `ln_1p(−½·erfc(−r/√2))` below 0, `ln(½·erfc(r/√2))` on `[0, 37]`, and
/// above 37 the asymptotic `ln φ(r) − ln r + ln(1 − 1/r² + 3/r⁴)`.
#[must_use]
pub fn norm_logsf(r: f64) -> f64 {
    if r.is_nan() {
        r
    } else if r < 0.0 {
        (-0.5 * erfc(-r * FRAC_1_SQRT_2)).ln_1p()
    } else if r <= 37.0 {
        (0.5 * erfc(r * FRAC_1_SQRT_2)).ln()
    } else {
        let r2 = r * r;
        norm_logpdf(r) - r.ln() + (1.0 - 1.0 / r2 + 3.0 / (r2 * r2)).ln()
    }
}

/// The standard normal quantile `Φ⁻¹(p)` (Wichura AS241, `PPND16`).
/// `−∞` at `p ≤ 0`, `+∞` at `p ≥ 1`, NaN for NaN.
#[must_use]
pub fn probit(p: f64) -> f64 {
    use coef::{A, B, C, D, E, F};
    if p.is_nan() {
        return p;
    }
    if p <= 0.0 {
        return f64::NEG_INFINITY;
    }
    if p >= 1.0 {
        return f64::INFINITY;
    }
    let q = p - 0.5;
    if q.abs() <= 0.425 {
        let r = 0.180625 - q * q;
        return q * poly(&A, r) / poly1(&B, r);
    }
    let tail = if q < 0.0 { p } else { 1.0 - p };
    let r = (-tail.ln()).sqrt();
    let x = if r <= 5.0 {
        let r = r - 1.6;
        poly(&C, r) / poly1(&D, r)
    } else {
        let r = r - 5.0;
        poly(&E, r) / poly1(&F, r)
    };
    if q < 0.0 {
        -x
    } else {
        x
    }
}

/// The logistic function, without overflow in either tail.
#[must_use]
pub fn sigmoid(a: f64) -> f64 {
    if a >= 0.0 {
        1.0 / (1.0 + (-a).exp())
    } else {
        let e = a.exp();
        e / (1.0 + e)
    }
}

/// `ln(1 + eˣ)`, without overflow or cancellation.
#[must_use]
pub fn softplus(x: f64) -> f64 {
    if x > 0.0 {
        x + (-x).exp().ln_1p()
    } else {
        x.exp().ln_1p()
    }
}

/// Solve `a·x = b` for a symmetric positive-definite `a` (`n×n`, row-major;
/// only its lower triangle is read) by Cholesky. `None` when `a` is not
/// numerically positive definite.
#[must_use]
pub fn cholesky_solve(a: &[f64], b: &[f64]) -> Option<Vec<f64>> {
    let n = b.len();
    if a.len() != n * n {
        return None;
    }
    let mut l = vec![0.0; n * n];
    for j in 0..n {
        let mut d = a[j * n + j];
        for k in 0..j {
            d -= l[j * n + k] * l[j * n + k];
        }
        if d <= 0.0 || !d.is_finite() {
            return None;
        }
        let ljj = d.sqrt();
        l[j * n + j] = ljj;
        for i in (j + 1)..n {
            let mut s = a[i * n + j];
            for k in 0..j {
                s -= l[i * n + k] * l[j * n + k];
            }
            l[i * n + j] = s / ljj;
        }
    }
    // L·y = b, then Lᵀ·x = y.
    let mut y = vec![0.0; n];
    for i in 0..n {
        let mut s = b[i];
        for k in 0..i {
            s -= l[i * n + k] * y[k];
        }
        y[i] = s / l[i * n + i];
    }
    let mut x = vec![0.0; n];
    for i in (0..n).rev() {
        let mut s = y[i];
        for k in (i + 1)..n {
            s -= l[k * n + i] * x[k];
        }
        x[i] = s / l[i * n + i];
    }
    Some(x)
}

/// Per-feature mean and population standard deviation plus [`STD_EPS`],
/// two-pass. An empty input gives means of 0 and deviations of `STD_EPS`.
#[must_use]
pub fn standardization<const N: usize>(xs: &[[f64; N]]) -> (Vec<f64>, Vec<f64>) {
    let mut mu = vec![0.0; N];
    let mut sd = vec![STD_EPS; N];
    if xs.is_empty() {
        return (mu, sd);
    }
    let n = xs.len() as f64;
    for (j, m) in mu.iter_mut().enumerate() {
        *m = xs.iter().map(|x| x[j]).sum::<f64>() / n;
    }
    for (j, s) in sd.iter_mut().enumerate() {
        let var = xs.iter().map(|x| (x[j] - mu[j]).powi(2)).sum::<f64>() / n;
        *s = var.sqrt() + STD_EPS;
    }
    (mu, sd)
}

/// `(x − mu) / sd`, elementwise.
#[must_use]
pub fn standardize<const N: usize>(x: &[f64; N], mu: &[f64], sd: &[f64]) -> [f64; N] {
    let mut z = [0.0; N];
    for (j, v) in z.iter_mut().enumerate() {
        *v = (x[j] - mu[j]) / sd[j];
    }
    z
}

/// Copy the upper triangle of a row-major `p×p` matrix onto its lower one.
pub(crate) fn mirror_upper(h: &mut [f64], p: usize) {
    for i in 0..p {
        for j in 0..i {
            h[i * p + j] = h[j * p + i];
        }
    }
}

/// The largest absolute entry.
fn inf_norm(v: &[f64]) -> f64 {
    v.iter().fold(0.0, |m, x| m.max(x.abs()))
}

/// [`Stop::decrease_rtol`] for both fits (#10501): a predicted decrease this
/// far below `|f|` is under the rounding noise of a sum over thousands of
/// rows (about `√n·ε·|f|`), so no line search can confirm it. Real fits
/// that stalled sat at `1e-18`..`1e-15` of `|f|`; the last genuine step
/// before each, `1e-9` or more.
pub(crate) const DECREASE_RTOL: f64 = 1e-13;

/// [`Stop::step_rtol`] for both fits (#10501). Keeps a run whose
/// coefficients still move — no finite optimum, `f → 0`, steps of `1/k` of
/// `‖x‖` — from passing on the decrement alone. Real noise-floor steps
/// measured `≤ 5e-7` relative, and the full step taken there leaves an error
/// of about its square.
pub(crate) const STEP_RTOL: f64 = 1e-5;

/// When [`newton`] stops.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Stop {
    /// Converged once `‖gradient‖∞` is below this (0 disables).
    pub grad_tol: f64,
    /// Converged once the full Newton step's `‖·‖∞` is below this.
    pub step_tol: f64,
    /// Noise-floor stop (#10501), with [`Stop::step_rtol`]: converged once an
    /// undamped Newton step's predicted decrease `λ²/2 = −g·d/2` is at most
    /// this times `max(|f|, 1)` (0 disables). Scale-aware: the same rule
    /// fits a sum objective, which grows with the rows, and a mean one.
    pub decrease_rtol: f64,
    /// The noise-floor stop also needs `‖d‖∞ ≤ step_rtol · max(‖x‖∞, 1)`.
    pub step_rtol: f64,
}

/// A minimizer [`newton`] found.
#[derive(Debug, Clone)]
pub(crate) struct Minimum {
    pub x: Vec<f64>,
    pub objective: f64,
    pub iterations: u32,
    pub converged: bool,
}

/// The objective, its gradient and its Hessian (`n×n`, row-major) at a point.
pub(crate) type Derivatives = (f64, Vec<f64>, Vec<f64>);

/// Solve `h·d = rhs`, adding Levenberg damping `λI` (growing tenfold) when
/// `h` is not positive definite. The flag is `true` for an undamped step —
/// `h` itself positive definite, so `d` is a true Newton step.
fn damped_solve(h: &[f64], rhs: &[f64]) -> Option<(Vec<f64>, bool)> {
    if let Some(d) = cholesky_solve(h, rhs) {
        return Some((d, true));
    }
    let n = rhs.len();
    let scale = (0..n).fold(1.0_f64, |m, i| m.max(h[i * n + i].abs()));
    let mut lambda = 1e-8 * scale;
    for _ in 0..40 {
        let mut damped = h.to_vec();
        for i in 0..n {
            damped[i * n + i] += lambda;
        }
        if let Some(d) = cholesky_solve(&damped, rhs) {
            return Some((d, false));
        }
        lambda *= 10.0;
    }
    None
}

/// Minimize by Newton's method with Levenberg damping and Armijo
/// backtracking, from `x0`, for at most [`MAX_NEWTON_ITERATIONS`] steps.
///
/// `eval` gives the objective with its derivatives; `value` the objective
/// alone (the line search). A full step whose `‖·‖∞` is below
/// `stop.step_tol` is taken and ends the run as converged, so a line search
/// that only shrinks the step can never fake convergence.
///
/// So is an undamped step at the noise floor (#10501): its predicted
/// decrease under `stop.decrease_rtol · max(|f|, 1)` and its size under
/// `stop.step_rtol · max(‖x‖∞, 1)`. There the decrease is below the
/// objective's own rounding, the line search can only accept noise-sized
/// fractions of an accurate step, and the absolute tolerances are out of
/// reach; from inside Newton's quadratic region the full step leaves an
/// error of about `‖d‖²`. Hitting the iteration cap, or a failed solve or
/// line search, still reports `converged: false`.
pub(crate) fn newton(
    x0: Vec<f64>,
    stop: Stop,
    eval: impl Fn(&[f64]) -> Derivatives,
    value: impl Fn(&[f64]) -> f64,
) -> Minimum {
    const ARMIJO: f64 = 1e-4;
    const MAX_HALVINGS: u32 = 60;
    let mut x = x0;
    let (mut f, mut g, mut h) = eval(&x);
    let mut iterations = 0;
    let mut converged = false;
    while iterations < MAX_NEWTON_ITERATIONS {
        if inf_norm(&g) < stop.grad_tol {
            converged = true;
            break;
        }
        let rhs: Vec<f64> = g.iter().map(|v| -v).collect();
        let Some((step, newton_step)) = damped_solve(&h, &rhs) else {
            break;
        };
        iterations += 1;
        let slope: f64 = g.iter().zip(&step).map(|(a, b)| a * b).sum();
        let step_norm = inf_norm(&step);
        let noise_floor = newton_step
            && stop.decrease_rtol > 0.0
            && -0.5 * slope <= stop.decrease_rtol * f.abs().max(1.0)
            && step_norm <= stop.step_rtol * inf_norm(&x).max(1.0);
        if step_norm < stop.step_tol || noise_floor {
            for (xi, di) in x.iter_mut().zip(&step) {
                *xi += di;
            }
            f = value(&x);
            converged = true;
            break;
        }
        // Rounding slack, so a step that is exact to the last bit is not
        // refused for an objective equal to f within its own ulp.
        let slack = 4.0 * f64::EPSILON * f.abs().max(1.0);
        let mut t = 1.0;
        let mut accepted = None;
        for _ in 0..MAX_HALVINGS {
            let cand: Vec<f64> = x.iter().zip(&step).map(|(xi, di)| xi + t * di).collect();
            let fc = value(&cand);
            if fc.is_finite() && fc <= f + ARMIJO * t * slope + slack {
                accepted = Some(cand);
                break;
            }
            t *= 0.5;
        }
        let Some(cand) = accepted else {
            break;
        };
        x = cand;
        (f, g, h) = eval(&x);
    }
    Minimum {
        x,
        objective: f,
        iterations,
        converged,
    }
}
