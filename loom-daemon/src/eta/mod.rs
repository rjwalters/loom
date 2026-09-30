//! ETA as a Loom primitive (#9289): versioned, explainable estimates of when an
//! issue's sweep finishes (`finish`) and when its work lands (`land`).
//!
//! # The model
//!
//! Remaining time is a sum over the stages an issue still has to pass, from
//! the stage it is in now, with one branch at every Judge verdict:
//!
//! ```text
//! sweep.curator → sweep.builder → review_wait ─┬─ approved ──→ merge_wait → landed
//!                                              └─ changes_requested → doctor ─┘ (loop, capped)
//! ```
//!
//! Each stage is summarised per `(repo, stage)` as a 21-point nearest-rank
//! quantile grid of observed durations ([`grid`]), with a host-wide fallback
//! and a hard floor of [`MIN_SAMPLES`]. The stage the issue is in now is
//! conditioned on the time it has already spent there (the inspection
//! paradox). The stages are combined by a deterministic Monte Carlo over the
//! grids ([`simulate`]), seeded from the estimate's own derived id.
//!
//! # Explanation first
//!
//! A heuristic's output is an [`explanation::Explanation`], and the quantiles
//! are computed *from* it. Everything the simulation reads — the grids, the
//! branch probabilities, the conditioning, the seed — is in the explanation,
//! so anyone holding one can recompute its result exactly
//! ([`simulate::run_explanation`]).
//!
//! # Purity
//!
//! Every estimator here is a pure function of `(history observed before
//! as_of, current state)`. [`history::StageSamples::select`] refuses any
//! sample observed at or after `as_of`, so a replay over past instants cannot
//! leak its own future (the backtest in #9325 depends on this).
//!
//! # Limitation: history is host-local (#9343)
//!
//! v1 history is this host's own journals only, so estimates are empty on
//! hosts that ran no sweeps for a repo, biased where they did, and
//! inconsistent between hosts; the human-gated stages happen on the forge,
//! not on any host. Every explanation records `history.scope = "local"` and
//! the samples each source and host contributed. See [`history`] for the
//! detail and the fleet-snapshot direction.
//!
//! # Versioning
//!
//! A heuristic id (`finish-v1`, `land-v1`) is immutable once shipped: a
//! golden test pins each id's output over a fixed fixture. A behaviour change
//! is a new id, registered next to the old one ([`Registry`]).

pub mod backtest;
pub mod config;
pub mod emit;
pub mod explanation;
pub mod grid;
pub mod heuristics;
pub mod history;
pub mod journal;
pub mod labels;
pub mod score;
pub mod simulate;
pub mod tracker;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
pub(crate) mod tests;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

pub use explanation::Explanation;
pub use history::StageSamples;

/// The explanation schema tag every estimate carries.
pub const EXPLANATION_SCHEMA: &str = "eta-explanation/v1";

/// Fewest samples a stage distribution may be built from. Below it, at both
/// the repo and the host level, the estimate is refused.
pub const MIN_SAMPLES: usize = 8;

/// Fewest samples longer than the current age a conditioned stage needs.
/// Below it the item has outlived the repo's own history.
pub const MIN_COND: usize = 5;

/// Most recent samples kept per stage distribution.
pub const MAX_SAMPLES: usize = 200;

/// History window, in days before `as_of`.
pub const WINDOW_DAYS: i64 = 60;

/// Monte Carlo paths per estimate.
pub const DRAWS: usize = 4000;

/// Most rework rounds (Judge rejections) a simulated path may take.
/// `sweep.max_doctor_cycles` defaults to 1, and the sweep may grant one more
/// bounded cycle for a distinct defect; past that the PR is blocked and does
/// not land, so the landing-conditional path approves at the cap.
pub const MAX_REWORK_ROUNDS: u32 = 2;

/// What an estimate predicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The running sweep reaches a terminal state.
    Finish,
    /// The issue's PR merges, or the issue closes as completed.
    Land,
}

impl Kind {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Finish => "finish",
            Kind::Land => "land",
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One stage of the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Stage {
    /// The sweep's Curator phase.
    #[serde(rename = "sweep.curator")]
    SweepCurator,
    /// The sweep's Builder phase, ending when the PR opens.
    #[serde(rename = "sweep.builder")]
    SweepBuilder,
    /// Waiting for, and receiving, a Judge verdict.
    #[serde(rename = "review_wait")]
    ReviewWait,
    /// Doctor rework after `loom:changes-requested`.
    #[serde(rename = "doctor")]
    Doctor,
    /// Approved (`loom:pr`), waiting to merge.
    #[serde(rename = "merge_wait")]
    MergeWait,
}

impl Stage {
    /// Every stage, in path order.
    pub const ALL: [Stage; 5] = [
        Stage::SweepCurator,
        Stage::SweepBuilder,
        Stage::ReviewWait,
        Stage::Doctor,
        Stage::MergeWait,
    ];

    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::SweepCurator => "sweep.curator",
            Stage::SweepBuilder => "sweep.builder",
            Stage::ReviewWait => "review_wait",
            Stage::Doctor => "doctor",
            Stage::MergeWait => "merge_wait",
        }
    }

    /// Dense index, for per-stage arrays.
    #[must_use]
    pub fn index(self) -> usize {
        match self {
            Stage::SweepCurator => 0,
            Stage::SweepBuilder => 1,
            Stage::ReviewWait => 2,
            Stage::Doctor => 3,
            Stage::MergeWait => 4,
        }
    }

    /// The stage a `sweep.outcome` phase name records (`judge` is the
    /// in-sweep `review_wait`, `merge` the in-sweep `merge_wait`).
    #[must_use]
    pub fn from_sweep_phase(phase: &str) -> Option<Stage> {
        match phase {
            "curator" => Some(Stage::SweepCurator),
            "builder" => Some(Stage::SweepBuilder),
            "judge" => Some(Stage::ReviewWait),
            "doctor" => Some(Stage::Doctor),
            "merge" => Some(Stage::MergeWait),
            _ => None,
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why no estimate was made. A closed set: a refusal is data, never a zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoEstimateReason {
    /// A hold or park label (`loom:blocked`, `loom:operator`, …).
    Blocked,
    /// A human-gated stage the model does not cover (intake, approval).
    HumanGated,
    /// Fewer than [`MIN_SAMPLES`] samples for a needed stage, at every level.
    InsufficientSamples,
    /// The item has been in its stage longer than all but [`MIN_COND`] samples.
    BeyondHistory,
    /// Not started; needs the dispatch plan (a later phase).
    NoDispatchPlan,
    /// No stage, or contradictory stage labels.
    UnknownStage,
    /// The inputs are too old to describe the present.
    StaleInputs,
}

impl NoEstimateReason {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            NoEstimateReason::Blocked => "blocked",
            NoEstimateReason::HumanGated => "human_gated",
            NoEstimateReason::InsufficientSamples => "insufficient_samples",
            NoEstimateReason::BeyondHistory => "beyond_history",
            NoEstimateReason::NoDispatchPlan => "no_dispatch_plan",
            NoEstimateReason::UnknownStage => "unknown_stage",
            NoEstimateReason::StaleInputs => "stale_inputs",
        }
    }
}

impl fmt::Display for NoEstimateReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which Loom build computed a record — required on every ETA record
/// (operator requirement on #9289). Sourced from
/// [`crate::telemetry::trace::provenance::daemon`], the same source every
/// span's `loom.daemon.*` attributes come from.
///
/// A build whose revision or tree state is `unknown` (a tarball build) still
/// emits, so no data is lost, but with `complete: false`; accuracy queries
/// exclude incomplete rows, because a result that cannot be pinned to a
/// commit cannot be attributed to a heuristic's code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// Loom version (`CARGO_PKG_VERSION`).
    pub version: String,
    /// Full 40-hex git SHA, or `unknown` for a tarball build.
    pub revision: String,
    /// `clean`, `dirty` or `unknown`.
    pub tree_state: String,
    /// `revision` is a full 40-hex SHA and `tree_state` is `clean` or
    /// `dirty`: the build is pinned. Always [`Provenance::completeness`] of
    /// the other two fields.
    pub complete: bool,
}

/// Whether `revision` is a full 40-hex lowercase git SHA.
fn is_full_sha(revision: &str) -> bool {
    revision.len() == 40
        && revision
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Provenance {
    /// The running binary.
    #[must_use]
    pub fn current() -> Self {
        let build = crate::telemetry::trace::provenance::daemon();
        Provenance {
            version: build.version.to_string(),
            revision: build.revision.to_string(),
            tree_state: build.tree_state.to_string(),
            complete: Self::completeness(build.revision, build.tree_state),
        }
    }

    /// Whether a build with `revision` and `tree_state` is fully pinned.
    #[must_use]
    pub fn completeness(revision: &str, tree_state: &str) -> bool {
        is_full_sha(revision) && matches!(tree_state, "clean" | "dirty")
    }

    /// Whether every field is well formed: a non-empty version, a full
    /// 40-hex revision or the build system's literal `unknown`, a known tree
    /// state, and a `complete` flag that matches them. An ETA record whose
    /// provenance fails this is never emitted. An `unknown` revision or tree
    /// state is well formed (and emitted), but not [`Self::complete`].
    #[must_use]
    pub fn is_valid(&self) -> bool {
        let revision_ok = self.revision == "unknown" || is_full_sha(&self.revision);
        !self.version.trim().is_empty()
            && revision_ok
            && matches!(self.tree_state.as_str(), "clean" | "dirty" | "unknown")
            && self.complete == Self::completeness(&self.revision, &self.tree_state)
    }
}

/// What an estimate is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subject {
    /// `owner/repo`.
    pub repo: String,
    /// GitHub numeric repository id, when known.
    pub repo_id: Option<u64>,
    /// Issue number.
    pub issue: u32,
    /// The issue's PR, once one exists.
    pub pr_number: Option<u32>,
    /// `owner/repo#issue`.
    pub story: String,
    /// The running sweep, when there is one.
    pub sweep_id: Option<String>,
}

impl Subject {
    /// A subject with its `story` filled in.
    #[must_use]
    pub fn new(repo: &str, repo_id: Option<u64>, issue: u32) -> Self {
        Subject {
            repo: repo.to_string(),
            repo_id,
            issue,
            pr_number: None,
            story: format!("{repo}#{issue}"),
            sweep_id: None,
        }
    }
}

/// Where the current stage's entry instant came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgeSource {
    /// A forge label event's timestamp.
    LabelEvent,
    /// A sweep checkpoint phase observation.
    Checkpoint,
    /// An event-bus transition.
    Bus,
    /// The item's `updated_at`: a lower bound on the age.
    UpdatedAtLowerBound,
    /// The ETA tracker's own observation of the transition between two
    /// passes: late by at most one refresh interval.
    TrackerObserved,
}

/// The stage an item is in now, as a resolver saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentStage {
    /// The stage.
    pub stage: Stage,
    /// When the item entered it, when known.
    pub entered_at: Option<DateTime<Utc>>,
    /// Seconds already spent in it (≥ 0).
    pub age_sec: i64,
    /// Where `entered_at` / `age_sec` came from.
    pub age_source: AgeSource,
    /// Judge rejections this PR has already taken.
    pub rework_rounds: u32,
}

/// The resolved present of one item: a stage, or the reason it has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurrentState {
    /// In a modelled stage.
    At(CurrentStage),
    /// Refused before any history is consulted.
    Refused(NoEstimateReason),
}

/// Everything an estimator reads besides history.
#[derive(Debug, Clone, PartialEq)]
pub struct EstimateInput {
    /// What is estimated.
    pub subject: Subject,
    /// The instant the estimate describes.
    pub as_of: DateTime<Utc>,
    /// The current stage.
    pub current: CurrentState,
    /// Recorded context; no v1 heuristic reads it.
    pub features: explanation::Features,
    /// Why features are null.
    pub features_omitted: Vec<explanation::FeatureOmitted>,
    /// The computing build.
    pub provenance: Provenance,
}

/// A registered estimator. Implementations must be pure.
pub trait Heuristic: Send + Sync {
    /// The immutable id, e.g. `land-v1`.
    fn id(&self) -> &'static str;
    /// What it predicts.
    fn kind(&self) -> Kind;
    /// Estimate `input` from `history`. Always returns an explanation; a
    /// refusal sets `no_estimate_reason` and carries no result.
    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation;
}

/// Every shipped heuristic, and which one is `current` per kind.
pub struct Registry {
    heuristics: Vec<Box<dyn Heuristic>>,
}

impl Registry {
    /// The built-in heuristics.
    #[must_use]
    pub fn builtin() -> Self {
        Registry {
            heuristics: vec![Box::new(heuristics::FinishV1), Box::new(heuristics::LandV1)],
        }
    }

    /// Look a heuristic up by id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&dyn Heuristic> {
        self.heuristics
            .iter()
            .find(|h| h.id() == id)
            .map(|h| h.as_ref())
    }

    /// Every registered id.
    #[must_use]
    pub fn ids(&self) -> Vec<&'static str> {
        self.heuristics.iter().map(|h| h.id()).collect()
    }

    /// The default `current` heuristic id for `kind`.
    #[must_use]
    pub fn default_current(kind: Kind) -> &'static str {
        match kind {
            Kind::Finish => heuristics::FINISH_V1,
            Kind::Land => heuristics::LAND_V1,
        }
    }

    /// The current heuristic for `kind`: `configured` when it names a
    /// registered heuristic of that kind, else the default.
    #[must_use]
    pub fn current(&self, kind: Kind, configured: Option<&str>) -> &dyn Heuristic {
        configured
            .and_then(|id| self.get(id))
            .filter(|h| h.kind() == kind)
            .or_else(|| self.get(Self::default_current(kind)))
            .unwrap_or_else(|| unreachable!("built-in heuristics are always registered"))
    }
}

/// The key under which an issue's story is identified in derived ids: the
/// GitHub numeric repo id when known (rename-stable), else the lowercased
/// slug.
#[must_use]
pub fn repo_key(subject: &Subject) -> String {
    match subject.repo_id {
        Some(id) => format!("github:{id}"),
        None => format!("repo:{}", subject.repo.to_ascii_lowercase()),
    }
}

/// The estimate's derived id (trace-identity policy: never random). The same
/// inputs always give the same id; a different `as_of` gives a different one.
#[must_use]
pub fn estimate_id(subject: &Subject, kind: Kind, heuristic: &str, as_of: DateTime<Utc>) -> String {
    let repo = repo_key(subject);
    let issue = subject.issue.to_string();
    let at = crate::telemetry::trace::instant(as_of);
    crate::telemetry::trace::derived_hex(
        &[
            "loom.eta.estimate",
            &repo,
            &issue,
            kind.as_str(),
            heuristic,
            &at,
        ],
        16,
    )
}

/// The Monte Carlo seed for `estimate_id`: the first 8 bytes of
/// `derived_hex(["loom.eta.seed", estimate_id], 16)`.
#[must_use]
pub fn seed_for(estimate_id: &str) -> u64 {
    let hex = crate::telemetry::trace::derived_hex(&["loom.eta.seed", estimate_id], 16);
    u64::from_str_radix(&hex, 16).unwrap_or(0)
}

/// Round to three decimals, for probabilities and shares in the explanation.
#[must_use]
pub(crate) fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}
