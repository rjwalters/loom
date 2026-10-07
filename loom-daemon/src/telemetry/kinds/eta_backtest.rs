//! `eta.backtest.fold` / `eta.backtest.summary` (#10492): the nightly
//! walk-forward backtest the fleet captain runs for every registered `land`
//! heuristic. **OTLP only**, like the other `eta.*` log kinds.
//!
//! - `eta.backtest.fold` — one record per heuristic per UTC day: how that
//!   heuristic scored on the day's replayed cases, and its paired delta against
//!   the `current` heuristic on the same cases.
//! - `eta.backtest.summary` — one record per non-`current` heuristic per run:
//!   the rolling per-day win count against `current`, its 95% Wilson lower
//!   bound, and `gate_ready` — whether the backtest half of the promotion gate
//!   (`eta promote`) would pass on the data known at the cutoff.
//!
//! Both are pure functions of data observed strictly before their `cutoff`
//! (see `eta::nightly_folds`), so a re-run of one day reproduces its record
//! byte for byte; ids are derived from `(heuristic, day)` alone, so two hosts
//! that both ran the day agree and a duplicate is detectable.
//!
//! **Absent is never zero**: a rate over no cases is omitted, not `0`.
//! **Provenance is required**: a record whose build provenance does not
//! validate is never emitted.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::eta::Provenance;

/// One heuristic's fold for one UTC day.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EtaBacktestFoldRecord {
    /// Derived, never random: `derived_hex(["loom.eta.backtest.fold",
    /// heuristic, day])`.
    pub fold_id: String,
    /// The heuristic folded.
    pub heuristic: String,
    /// The kind it predicts (`land` today).
    pub kind: String,
    /// The fold's UTC day, `YYYY-MM-DD`: its cohort is the cases first known
    /// (resolved) on it, whichever day they were predicted on, so every
    /// resolved case is folded exactly once (#10532 review).
    pub day: String,
    /// The fold's cutoff (the end of `day`): nothing observed at or after it
    /// was read, and the record's time.
    pub cutoff: DateTime<Utc>,
    /// The `current` heuristic the deltas are against.
    pub compared_to: String,
    /// This heuristic is the `current` one (no deltas, no win).
    pub is_current: bool,
    /// Replayed cases that day, answered or refused.
    pub n_cases: u64,
    /// Of those, the ones this heuristic answered.
    pub n_answered: u64,
    /// `n_answered / n_cases`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_rate: Option<f64>,
    /// Mean four-quantile pinball loss over the cases carrying a p90, seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinball4_loss_sec: Option<f64>,
    /// Share of answered cases whose actual fell in `[p25, p75]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cov_25_75: Option<f64>,
    /// Share of decided cases whose actual exceeded p90 (the late surprise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub late_surprise: Option<f64>,
    /// Cases both this heuristic and `current` scored with a p90.
    pub paired_pairs: u64,
    /// This heuristic's paired mean `pinball4` minus `current`'s, seconds;
    /// negative is better.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_pinball4_loss_sec: Option<f64>,
    /// This heuristic's answer rate minus `current`'s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_answer_rate: Option<f64>,
    /// This heuristic's late-surprise rate minus `current`'s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_late_surprise: Option<f64>,
    /// The day's win over `current` on the deciding loss; absent when there
    /// were no pairs or an exact tie.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub win: Option<bool>,
    /// The coefficient file serving predictions made on this day (the newest
    /// whose cutoff is strictly before the day began). A cohort case
    /// predicted on an earlier day is scored with that day's file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit_id: Option<String>,
    /// The computing build.
    pub loom: Provenance,
}

impl EtaBacktestFoldRecord {
    /// Whether the record carries valid provenance.
    #[must_use]
    pub fn has_provenance(&self) -> bool {
        self.loom.is_valid()
    }
}

/// One non-`current` heuristic's rolling backtest standing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EtaBacktestSummaryRecord {
    /// Derived: `derived_hex(["loom.eta.backtest.summary", heuristic,
    /// as_of_day])`.
    pub summary_id: String,
    /// The challenger.
    pub heuristic: String,
    /// The kind it predicts.
    pub kind: String,
    /// The `current` heuristic it is measured against.
    pub compared_to: String,
    /// The newest fold day included, `YYYY-MM-DD`.
    pub as_of_day: String,
    /// The summary's cutoff (the end of `as_of_day`) and the record's time.
    pub cutoff: DateTime<Utc>,
    /// Replayed cases in the window (the union both sides were asked).
    pub cases: u64,
    /// Days decided (one side had the lower mean deciding loss).
    pub days: u64,
    /// Of those, the days the challenger won.
    pub wins: u64,
    /// Days that tied exactly.
    pub ties: u64,
    /// `wins / days`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub win_rate: Option<f64>,
    /// Lower bound of the 95% Wilson interval of the win rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_low: Option<f64>,
    /// Upper bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ci_high: Option<f64>,
    /// Decided days the gate requires.
    pub min_folds: u64,
    /// The backtest half of `eta promote` would pass on this data.
    pub gate_ready: bool,
    /// The gate's own explanation (`BacktestGate::detail`), so the verdict is
    /// never unexplained.
    pub gate_detail: String,
    /// The first prediction day scored, `YYYY-MM-DD`: the first whose
    /// registry carries a retained coefficient file. Each case is scored with
    /// its own prediction day's file, as its daily fold scored it. Absent when
    /// this host has no coefficient file at all (every case is then scored
    /// unfitted, as live serving was).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fitted_from: Option<String>,
    /// Resolved cases left out because they were predicted before
    /// `fitted_from`: no historical fit survives to score them with.
    #[serde(default)]
    pub cases_before_fit: u64,
    /// The coefficient file serving `as_of_day`'s own predictions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit_id: Option<String>,
    /// The computing build.
    pub loom: Provenance,
}

impl EtaBacktestSummaryRecord {
    /// Whether the record carries valid provenance.
    #[must_use]
    pub fn has_provenance(&self) -> bool {
        self.loom.is_valid()
    }
}
