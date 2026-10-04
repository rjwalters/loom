//! The fitted-model candidates the walk-forward evaluator chooses among:
//! one family per model, each with its own settings grid.

use super::dataset::{LeakError, Row};
use super::model::{KmModel, KmSettings};
use super::qr::{QrModel, QrSettings};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One candidate's settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "family")]
pub enum Settings {
    /// Per-stage, per-age-bucket Kaplan–Meier quantiles ([`KmModel`]).
    Km(KmSettings),
    /// Linear quantile regression ([`QrModel`]).
    Qr(QrSettings),
}

/// A fitted candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "family")]
pub enum Fitted {
    /// See [`KmModel`].
    Km(KmModel),
    /// See [`QrModel`].
    Qr(QrModel),
}

impl Settings {
    /// Every family's grid, in a fixed order.
    #[must_use]
    pub fn grid() -> Vec<Settings> {
        KmSettings::grid()
            .into_iter()
            .map(Settings::Km)
            .chain(QrSettings::grid().into_iter().map(Settings::Qr))
            .collect()
    }

    /// `km-…` or `qr-…`.
    #[must_use]
    pub fn id(&self) -> String {
        match self {
            Settings::Km(s) => s.id(),
            Settings::Qr(s) => s.id(),
        }
    }

    /// `km` or `qr`: settings are chosen per family.
    #[must_use]
    pub fn family(&self) -> &'static str {
        match self {
            Settings::Km(_) => "km",
            Settings::Qr(_) => "qr",
        }
    }

    /// Fit on `training` (refused when it is not point-in-time at
    /// `cutoff`).
    pub fn fit(&self, training: &[Row], cutoff: DateTime<Utc>) -> Result<Fitted, LeakError> {
        Ok(match self {
            Settings::Km(s) => Fitted::Km(KmModel::fit(training, cutoff, *s)?),
            Settings::Qr(s) => Fitted::Qr(QrModel::fit(training, cutoff, *s)?),
        })
    }
}

impl Fitted {
    /// Remaining-time quartiles, seconds, when the model answers.
    #[must_use]
    pub fn predict(&self, row: &Row) -> Option<(i64, i64, i64)> {
        match self {
            Fitted::Km(m) => m.predict(row),
            Fitted::Qr(m) => m.predict(row),
        }
    }
}
