//! The `eta-fit/v3` fit and its files (#10521): the same three models as
//! `eta-fit/v1` and `eta-fit/v2` over the friction-aware feature set
//! [`FEATURES_V3`], written as their own versioned file.
//!
//! # A third file, never a reinterpretation
//!
//! A v3 file is a [`CoefficientFile`] whose `schema` is [`SCHEMA_V3`] and
//! whose `features` are [`FEATURES_V3`]. It lives in [`fit_dir_v3`]
//! (`<fit_dir>/v3`), so neither the v1 nor the v2 loader and retention ever
//! see it, and [`read_v3`] refuses any file not tagged v3.
//!
//! # One transform for training and serving
//!
//! [`features_v3`] reads each training row's own recorded inputs
//! ([`super::features_v3::training_inputs_v3`]: the v1 inputs, the priority
//! inputs and the [`crate::eta::loop_features::LoopFeatures`], all built by
//! the builders serving also calls) and turns them into [`FEATURES_V3`]
//! order through [`model_features_v3`], the transform the twin-otter
//! evaluation core calls for a v3 model ([`crate::eta::twin_otter`]). The
//! hazard and direct-model fitters are the width-generic v1 ones.

use std::path::{Path, PathBuf};

use super::coeffs::{age_p95_sec, fit_dir, CoefficientFile, FitMeta, FitSettings};
use super::features_v3::{
    model_features_v3, training_inputs_v3, FEATURES_V3, N_FEATURES_V3, SCHEMA_V3,
};
use super::rows::Assembled;
use super::{aft, logistic, paths, FitStage, TrainingRow};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

/// The directory v3 coefficient files live in: `<fit_dir>/v3`.
#[must_use]
pub fn fit_dir_v3(workspace_root: &Path) -> PathBuf {
    fit_dir(workspace_root).join("v3")
}

/// The v3 features of every training row of `assembled`, in row order.
///
/// # Panics
///
/// `assembled`'s per-row vectors (`priority_inputs`, `loops`) are not as long
/// as its `rows`.
#[must_use]
pub fn features_v3(assembled: &Assembled) -> Vec<[f64; N_FEATURES_V3]> {
    (0..assembled.rows.len())
        .map(|i| {
            let inputs = training_inputs_v3(assembled, i)
                .expect("one priority input set and one loop feature set per row");
            model_features_v3(&inputs)
        })
        .collect()
}

/// An empty v3 file for `meta`, with the current settings and no id yet.
#[must_use]
pub fn empty_v3(meta: &FitMeta) -> CoefficientFile {
    CoefficientFile {
        schema: SCHEMA_V3.to_string(),
        id: String::new(),
        as_of: meta.as_of,
        window: meta.window.clone(),
        fitter: meta.fitter.clone(),
        settings: FitSettings::current(),
        features: FEATURES_V3.iter().map(|f| (*f).to_string()).collect(),
        hazard: BTreeMap::new(),
        hazard_skipped: BTreeMap::new(),
        aft: None,
        path_stats: super::PathStats::default(),
        age_p95_sec: BTreeMap::new(),
    }
}

/// The v3 fit of `assembled`'s rows and dwells into a file with its derived
/// id. Every rule but the feature set is [`super::coeffs::fit`]'s.
///
/// # Panics
///
/// As [`features_v3`].
#[must_use]
pub fn fit_v3(meta: &FitMeta, assembled: &Assembled) -> CoefficientFile {
    let rows: &[TrainingRow] = &assembled.rows;
    let xs = features_v3(assembled);
    let mut file = empty_v3(meta);
    for stage in FitStage::ALL {
        let of_stage: Vec<(&TrainingRow, [f64; N_FEATURES_V3])> = rows
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
    file.path_stats = paths::path_stats(&assembled.dwells);
    file.age_p95_sec = age_p95_sec(rows);
    file.with_derived_id()
}

/// Read the v3 file at `path`. `None` when it is absent, unreadable, or not
/// [`SCHEMA_V3`] (a v1 or v2 file included).
#[must_use]
pub fn read_v3(path: &Path) -> Option<CoefficientFile> {
    super::coeffs::read_schema(path, SCHEMA_V3)
}

/// The newest readable v3 fit under `workspace_root` whose `as_of` is
/// strictly before `before`, or `None`: [`super::load_latest`]'s rule over
/// [`fit_dir_v3`].
#[must_use]
pub fn load_latest_v3(workspace_root: &Path, before: DateTime<Utc>) -> Option<CoefficientFile> {
    super::coeffs::load_latest_in(&fit_dir_v3(workspace_root), before, SCHEMA_V3)
}
