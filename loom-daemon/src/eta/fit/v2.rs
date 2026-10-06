//! The `eta-fit/v2` fit and its files (#10508): the same three models as
//! `eta-fit/v1` ([`super::coeffs::fit`]) over the priority-aware feature
//! set [`FEATURES_V2`], written as their own versioned file.
//!
//! # A second file, never a reinterpretation
//!
//! A v2 file is a [`CoefficientFile`] whose `schema` is [`SCHEMA_V2`] and
//! whose `features` are [`FEATURES_V2`]. It lives in its own directory,
//! [`fit_dir_v2`] (`<fit_dir>/v2`), so the v1 loader
//! ([`super::load_latest`]) and the v1 retention ([`super::run::prune_dir`])
//! never see it, and [`read_v2`] refuses anything not tagged v2 (as
//! [`super::read`] refuses anything not tagged v1). A loader therefore picks
//! a file by its schema tag, and a v1 vector is never read against v2
//! positions or the reverse.
//!
//! # One transform for training and serving
//!
//! [`features_v2`] turns each row's v1 raw inputs plus its priority inputs
//! (built by [`crate::eta::priority_inputs::priority_inputs`], the builder
//! serving also calls) into [`FEATURES_V2`] order through
//! [`model_features_v2`], the transform the twin-otter evaluation core calls
//! at serving time for a v2 model ([`crate::eta::twin_otter`]). The hazard
//! and direct-model fitters are the v1 ones, generic over the feature width
//! ([`super::logistic::fit_stage_x`], [`super::aft::fit_x`]); the path
//! statistics read no features and are identical to v1's.

use std::path::{Path, PathBuf};

use super::coeffs::{age_p95_sec, fit_dir, CoefficientFile, FitMeta, FitSettings};
use super::features_v2::{
    model_features_v2, ModelInputsV2, PriorityInputs, FEATURES_V2, N_FEATURES_V2, SCHEMA_V2,
};
use super::{aft, logistic, paths, DwellRow, FitStage, TrainingRow};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

/// The directory v2 coefficient files live in: `<fit_dir>/v2`.
#[must_use]
pub fn fit_dir_v2(workspace_root: &Path) -> PathBuf {
    fit_dir(workspace_root).join("v2")
}

/// The v2 features of `rows`, `priority[i]` being `rows[i]`'s priority
/// inputs.
///
/// # Panics
///
/// `priority` is not as long as `rows`.
#[must_use]
pub fn features_v2(rows: &[TrainingRow], priority: &[PriorityInputs]) -> Vec<[f64; N_FEATURES_V2]> {
    assert_eq!(rows.len(), priority.len(), "one priority input set per row");
    rows.iter()
        .zip(priority)
        .map(|(r, p)| {
            model_features_v2(&ModelInputsV2 {
                base: r.inputs,
                priority: *p,
            })
        })
        .collect()
}

/// An empty v2 file for `meta`, with the current settings and no id yet.
#[must_use]
pub fn empty_v2(meta: &FitMeta) -> CoefficientFile {
    CoefficientFile {
        schema: SCHEMA_V2.to_string(),
        id: String::new(),
        as_of: meta.as_of,
        window: meta.window.clone(),
        fitter: meta.fitter.clone(),
        settings: FitSettings::current(),
        features: FEATURES_V2.iter().map(|f| (*f).to_string()).collect(),
        hazard: BTreeMap::new(),
        hazard_skipped: BTreeMap::new(),
        aft: None,
        path_stats: super::PathStats::default(),
        age_p95_sec: BTreeMap::new(),
    }
}

/// The v2 fit of `rows` (with their `priority` inputs) and `dwells`,
/// processed in the order given, into a file with its derived id. Every
/// rule but the feature set is [`super::coeffs::fit`]'s.
///
/// # Panics
///
/// `priority` is not as long as `rows`.
#[must_use]
pub fn fit_v2(
    meta: &FitMeta,
    rows: &[TrainingRow],
    priority: &[PriorityInputs],
    dwells: &[DwellRow],
) -> CoefficientFile {
    let xs = features_v2(rows, priority);
    let mut file = empty_v2(meta);
    for stage in FitStage::ALL {
        let of_stage: Vec<(&TrainingRow, [f64; N_FEATURES_V2])> = rows
            .iter()
            .zip(&xs)
            .filter(|(r, _)| r.stage == stage)
            .map(|(r, x)| (r, *x))
            .collect();
        match logistic::fit_stage_x(&of_stage) {
            Ok(hazard) => {
                file.hazard.insert(stage, hazard);
            }
            Err(skip) => {
                file.hazard_skipped.insert(stage, skip);
            }
        }
    }
    file.aft = aft::fit_x(rows, &xs);
    file.path_stats = paths::path_stats(dwells);
    file.age_p95_sec = age_p95_sec(rows);
    file.with_derived_id()
}

/// Read the v2 file at `path`. `None` when it is absent, unreadable, or not
/// [`SCHEMA_V2`] (a v1 file included).
#[must_use]
pub fn read_v2(path: &Path) -> Option<CoefficientFile> {
    super::coeffs::read_schema(path, SCHEMA_V2)
}

/// The newest readable v2 fit under `workspace_root` whose `as_of` is
/// strictly before `before`, or `None`: [`super::load_latest`]'s rule over
/// [`fit_dir_v2`].
#[must_use]
pub fn load_latest_v2(workspace_root: &Path, before: DateTime<Utc>) -> Option<CoefficientFile> {
    super::coeffs::load_latest_in(&fit_dir_v2(workspace_root), before, SCHEMA_V2)
}
