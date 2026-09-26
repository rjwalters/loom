//! The `--group-by` dimension enum and its wire/display spellings. Split out
//! of `sweep_outcome_summary.rs` to stay under the file-size ratchet
//! threshold (`scripts/check-file-size-budget.sh`, #7711) after Issue #8542
//! added the `complexity` / `model-complexity` dimensions.

use anyhow::{bail, Result};
use serde::Serialize;

/// The `--group-by` dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupBy {
    /// Experiment arm — explicit stamp preferred, inferred marked, unstamped
    /// bucketed as `unknown` (see [`super::resolve_arm`]).
    Arm,
    /// Dispatched model, `default` for a record with none.
    Model,
    /// `owner/repo`.
    Repo,
    /// Emitting host. Degenerate on a local read — see the module docs.
    Host,
    /// UTC calendar day of the envelope's `emitted_at`.
    Day,
    /// The **tap** — `(runtime, credential source)` — that paid for the sweep
    /// (Issue #8556). See [`super::resolve_tap`] for how a record with no
    /// explicit `config["tap"]` stamp is placed.
    Tap,
    /// The Curator's complexity tier (Issue #8542): `mechanical` / `routine` /
    /// `complex`, `unknown` for a record with no marker observed — see
    /// [`crate::telemetry::SweepOutcomeRecord::complexity`]. This is the
    /// routing-evaluation cut: each row's `first_pass_approval_rate` answers
    /// "does this tier's dispatch hold the Judge first-pass rate?".
    #[serde(rename = "complexity")]
    Complexity,
    /// The `model × complexity` cross-tab (Issue #8542): the compound key
    /// `"<model>/<complexity>"`, each side defaulting exactly as its own
    /// single-dimension grouping does (`default` / `unknown`). Answers
    /// "within one tier, does a cheaper model hold the approval rate?" —
    /// the question `--group-by model` and `--group-by complexity` can each
    /// only approximate alone.
    #[serde(rename = "model-complexity")]
    ModelComplexity,
}

impl GroupBy {
    /// Parse a `--group-by` value. Case-insensitive.
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "arm" => Ok(Self::Arm),
            "model" => Ok(Self::Model),
            "repo" => Ok(Self::Repo),
            "host" => Ok(Self::Host),
            "day" => Ok(Self::Day),
            "tap" => Ok(Self::Tap),
            "complexity" => Ok(Self::Complexity),
            "model-complexity" => Ok(Self::ModelComplexity),
            other => bail!(
                "unknown --group-by value {other:?} (expected one of: arm, model, repo, host, \
                 day, tap, complexity, model-complexity)"
            ),
        }
    }

    /// The wire/display spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Arm => "arm",
            Self::Model => "model",
            Self::Repo => "repo",
            Self::Host => "host",
            Self::Day => "day",
            Self::Tap => "tap",
            Self::Complexity => "complexity",
            Self::ModelComplexity => "model-complexity",
        }
    }
}
