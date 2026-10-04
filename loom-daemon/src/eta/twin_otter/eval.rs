//! The deterministic parts of twin-otter: the feature vector, the
//! current-stage survival curve, the direct (AFT) quantiles and the blend.

use super::{EvalConfig, EvalError, TwinOtterInput, PROBIT_TAUS};
use crate::eta::fit::math::sigmoid;
use crate::eta::fit::{clock, model_features, AftFit, HazardFit, ModelInputs, FEATURES};
use chrono::{DateTime, Duration, Utc};

/// Each nullable input field, and the model feature it feeds.
const NULLABLE: [(&str, &str); 9] = [
    ("ahead", "log_ahead"),
    ("n_stage_repo", "log_n_stage_repo"),
    ("exits_repo_6h", "log_exits_repo_6h"),
    ("exits_repo_24h", "log_exits_repo_24h"),
    ("exits_fleet_6h", "log_exits_fleet_6h"),
    ("merges_repo_24h", "log_merges_repo_24h"),
    ("merges_fleet_6h", "log_merges_fleet_6h"),
    ("since_merge_h", "log_since_merge"),
    ("n_stage_fleet", "log_n_stage_fleet"),
];

/// One item's features, laid out in the model's feature order.
pub(super) struct Features<'a> {
    input: &'a TwinOtterInput,
    /// For each model feature, its position in [`FEATURES`].
    index: Vec<usize>,
    /// For each model feature, whether it is imputed at standardized 0.
    imputed_at: Vec<bool>,
    /// The `None` input fields, in [`NULLABLE`] order.
    imputed: Vec<&'static str>,
}

impl<'a> Features<'a> {
    /// Map `names` onto the shared transform's output, and note the inputs
    /// to impute.
    pub(super) fn new(names: &[String], input: &'a TwinOtterInput) -> Result<Self, EvalError> {
        check_input(input)?;
        let index = names
            .iter()
            .map(|name| {
                FEATURES.iter().position(|f| f == name).ok_or_else(|| {
                    EvalError::InvalidModel(format!("unknown feature name `{name}`"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let missing = [
            input.ahead.is_none(),
            input.n_stage_repo.is_none(),
            input.exits_repo_6h.is_none(),
            input.exits_repo_24h.is_none(),
            input.exits_fleet_6h.is_none(),
            input.merges_repo_24h.is_none(),
            input.merges_fleet_6h.is_none(),
            input.since_merge_h.is_none(),
            input.n_stage_fleet.is_none(),
        ];
        let imputed = NULLABLE
            .iter()
            .zip(missing)
            .filter(|(_, m)| *m)
            .map(|((field, _), _)| *field)
            .collect();
        let imputed_at = names
            .iter()
            .map(|name| {
                NULLABLE
                    .iter()
                    .zip(missing)
                    .any(|((_, feature), m)| m && feature == name)
            })
            .collect();
        Ok(Features {
            input,
            index,
            imputed_at,
            imputed,
        })
    }

    /// The number of model features.
    pub(super) fn len(&self) -> usize {
        self.index.len()
    }

    /// The `None` input fields.
    pub(super) fn imputed(&self) -> &[&'static str] {
        &self.imputed
    }

    /// The standardized features at age `age_h` and instant `at`, with every
    /// other input frozen. An imputed feature is 0.
    fn standardized(&self, age_h: f64, at: DateTime<Utc>, mu: &[f64], sd: &[f64]) -> Vec<f64> {
        let (hour_utc, weekend) = clock(at);
        let x = model_features(&self.raw(age_h, hour_utc, weekend));
        self.index
            .iter()
            .zip(&self.imputed_at)
            .zip(mu.iter().zip(sd))
            .map(|((&i, &imputed), (m, s))| if imputed { 0.0 } else { (x[i] - m) / s })
            .collect()
    }

    /// The shared transform's raw inputs. A `None` count is a placeholder 0
    /// here; its standardized value is replaced by 0.
    fn raw(&self, age_h: f64, hour_utc: f64, weekend: bool) -> ModelInputs {
        let i = self.input;
        ModelInputs {
            age_h,
            ahead: i.ahead.unwrap_or(0),
            n_stage_repo: i.n_stage_repo.unwrap_or(0),
            exits_repo_6h: i.exits_repo_6h.unwrap_or(0),
            exits_repo_24h: i.exits_repo_24h.unwrap_or(0),
            exits_fleet_6h: i.exits_fleet_6h.unwrap_or(0),
            merges_repo_24h: i.merges_repo_24h.unwrap_or(0),
            merges_fleet_6h: i.merges_fleet_6h.unwrap_or(0),
            since_merge_h: i.since_merge_h.unwrap_or(0.0),
            n_stage_fleet: i.n_stage_fleet.unwrap_or(0),
            hour_utc,
            weekend,
            rework: i.rework,
            op_hold: i.op_hold != 0,
            sequenced: i.sequenced != 0,
            starred: i.starred != 0,
            conflict: i.conflict != 0,
            ci_fail: i.ci_fail != 0,
            blocked: i.blocked != 0,
        }
    }
}

/// `S_1..S_steps` under the current stage's hazard, and whether the age
/// clamp bound at any step.
pub(super) fn survival_curve(
    hazard: &HazardFit,
    features: &Features<'_>,
    config: &EvalConfig,
) -> (Vec<f64>, bool) {
    let input = features.input;
    let mut survival = Vec::with_capacity(config.steps);
    let mut s = 1.0_f64;
    let mut clamped = false;
    for j in 0..config.steps {
        let offset_h = j as f64 * config.step_h;
        let (age_h, bound) = clamp_age(input.age_h + offset_h, config.age_clamp_h);
        clamped |= bound;
        let z = features.standardized(age_h, at(input.as_of, offset_h), &hazard.mu, &hazard.sd);
        s *= 1.0 - sigmoid(hazard.intercept + dot(&hazard.coef, &z));
        survival.push(s);
    }
    (survival, clamped)
}

/// The direct model's quantiles at [`super::TAUS`], each capped at `cap_h`.
/// `stage` is the current stage's position in `aft.stages`.
pub(super) fn aft_quantiles(
    aft: &AftFit,
    stage: usize,
    features: &Features<'_>,
    config: &EvalConfig,
) -> [f64; 4] {
    let input = features.input;
    let (age_h, _) = clamp_age(input.age_h, config.age_clamp_h);
    let z = features.standardized(age_h, input.as_of, &aft.mu, &aft.sd);
    // The stage one-hot selects one intercept; the features follow them.
    let k = aft.stages.len();
    let linear = aft.beta[stage] + dot(&aft.beta[k..], &z);
    let sigma = aft.log_sigma[stage].exp();
    PROBIT_TAUS.map(|probit| (linear + sigma * probit).exp().min(config.cap_h))
}

/// The blend: the elementwise mean of the two parts, each capped at `cap_h`,
/// then a cumulative max so the quantiles never decrease.
#[must_use]
pub fn blend(path_q: &[f64; 4], aft_q: &[f64; 4], cap_h: f64) -> [f64; 4] {
    let mut out = [0.0; 4];
    let mut floor = f64::NEG_INFINITY;
    for (o, (p, a)) in out.iter_mut().zip(path_q.iter().zip(aft_q)) {
        floor = floor.max(0.5 * (p.min(cap_h) + a.min(cap_h)));
        *o = floor;
    }
    out
}

/// A hazard model whose shape fits `n` features.
pub(super) fn check_hazard(hazard: &HazardFit, n: usize) -> Result<(), EvalError> {
    check_standardization("hazard", &hazard.mu, &hazard.sd, n)?;
    check_finite("hazard coef", &hazard.coef, n)?;
    check_finite("hazard intercept", &[hazard.intercept], 1)
}

/// A direct model whose shape fits `n` features.
pub(super) fn check_aft(aft: &AftFit, n: usize) -> Result<(), EvalError> {
    check_standardization("aft", &aft.mu, &aft.sd, n)?;
    let k = aft.stages.len();
    check_finite("aft beta", &aft.beta, k + n)?;
    check_finite("aft log_sigma", &aft.log_sigma, k)
}

fn check_standardization(what: &str, mu: &[f64], sd: &[f64], n: usize) -> Result<(), EvalError> {
    check_finite(&format!("{what} mu"), mu, n)?;
    check_finite(&format!("{what} sd"), sd, n)?;
    if sd.iter().any(|s| *s <= 0.0) {
        return Err(EvalError::InvalidModel(format!("{what} sd is not positive")));
    }
    Ok(())
}

fn check_finite(what: &str, values: &[f64], len: usize) -> Result<(), EvalError> {
    if values.len() != len {
        return Err(EvalError::InvalidModel(format!(
            "{what} has {} entries, expected {len}",
            values.len()
        )));
    }
    if values.iter().any(|v| !v.is_finite()) {
        return Err(EvalError::InvalidModel(format!("{what} is not finite")));
    }
    Ok(())
}

fn check_input(input: &TwinOtterInput) -> Result<(), EvalError> {
    if !(input.age_h.is_finite() && input.age_h >= 0.0) {
        return Err(EvalError::InvalidInput("age_h must be non-negative and finite".to_string()));
    }
    if input
        .since_merge_h
        .is_some_and(|h| !(h.is_finite() && h >= 0.0))
    {
        return Err(EvalError::InvalidInput(
            "since_merge_h must be non-negative and finite".to_string(),
        ));
    }
    Ok(())
}

/// The age feature's hours under the optional clamp, and whether it bound.
fn clamp_age(age_h: f64, clamp_h: Option<f64>) -> (f64, bool) {
    match clamp_h {
        Some(clamp) if age_h > clamp => (clamp, true),
        _ => (age_h, false),
    }
}

/// `as_of + offset_h`, to the millisecond.
fn at(as_of: DateTime<Utc>, offset_h: f64) -> DateTime<Utc> {
    as_of + Duration::milliseconds((offset_h * 3_600_000.0).round() as i64)
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}
