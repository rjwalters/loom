//! ETA as a Loom primitive (#9289): versioned, explainable estimates of when an
//! issue's sweep starts (`start`), finishes (`finish`) and when its work lands
//! (`land`).
//!
//! # The model
//!
//! Remaining time is a sum over the stages an issue still has to pass, from
//! the stage it is in now, with one branch at every Judge verdict:
//!
//! ```text
//! ready_wait → sweep.curator → sweep.builder → review_wait ─┬─ approved ──→ merge_wait → landed
//!                                              └─ changes_requested → doctor ─┘ (loop, capped)
//!                                                       approved + operator hold: merge_hold ⇄ merge_wait
//! ```
//!
//! `merge_hold` (#10218) is an approved PR held for a human (`loom:pr` plus
//! `loom:operator`, `loom:operator-only` or `loom:operator-decision`). It
//! leaves to `merge_wait` when the hold is lifted, or to `doctor`, a merge or
//! a close. Every path-engine heuristic still refuses it as `blocked` (the
//! shadow `land-2026-10-04-twin-otter` estimates it from its fit's own
//! `merge_hold` stage), and the `merge_wait` samples still run from the
//! approval to the merge, hold
//! included (the **pooled** definition); the hold-free `merge_wait` and the
//! hold itself live in [`episodes`], the record a hold-aware heuristic fits.
//!
//! `ready_wait` (#9326) is the stage a ready (`loom:issue`) issue spends
//! waiting for a dispatch slot. Its distribution is the host's empirical
//! **slot turnover** — the interval between two issue-sweep slots freeing —
//! and a ready item visits it once per turnover it still needs, from its
//! `dispatch_plan` position ([`DispatchInput`]). `start` ends at dispatch;
//! `land` for an unstarted issue prepends it to the post-dispatch chain.
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
//! # History scope: local and fleet (#9343)
//!
//! Host-local history is this host's own journals only, so estimates are empty
//! on hosts that ran no sweeps for a repo, biased where they did, and
//! inconsistent between hosts; the human-gated stages happen on the forge, not
//! on any host. [`fleet`] is the host-independent alternative: one
//! forge-derived stage-boundary snapshot per repo, built outside the estimator,
//! cached on disk, and handed in as an ordinary [`StageSamples`] value. Every
//! explanation records `history.scope` (`local` / `fleet`) and the samples each
//! source and host contributed, so forensics can always tell which view
//! produced a number. See [`history`] for the trade-offs and [`fleet`] for the
//! snapshot's determinism and cost properties.
//!
//! # Fitted models (#10221, #10243)
//!
//! [`fit`] is the pure core of the daily point-in-time fit (`eta-fit/v1`):
//! per-stage exit hazards, a censored log-normal direct model and dwell-path
//! statistics, written as one content-addressed coefficient file that fitted
//! heuristics load instead of reading fleet history themselves.
//!
//! The file is loaded **when the registry is built**, never inside an
//! estimate: [`Registry::load`] reads the newest fit strictly before an
//! instant, and [`Registry::with_fit`] is the pure constructor it wraps.
//! The tracker rebuilds its registry when a later pass finds a fit with a
//! different id, so a daily refit reaches a running daemon within one pass.
//! A fitted heuristic with no usable file refuses `no_model`.
//!
//! # Versioning
//!
//! A heuristic id (`start-v1`, `finish-v1`, `land-v1`) is immutable once shipped: a
//! golden test pins each id's output over a fixed fixture. A behaviour change
//! is a new id, registered next to the old one ([`Registry`]).
//!
//! # Shadow mode and promotion (#9328)
//!
//! Every registered heuristic of a kind is computed and logged for every
//! tracked subject ([`Registry::for_kind`]); exactly one is `current`
//! (`autonomous.eta.current.<kind>`) and only its estimate is the primary one
//! existing consumers read. A candidate becomes `current` only by clearing two
//! gates, in order — the phase-2 [`backtest`] first, then live paired scoring
//! — and the switch that flips the config is [`shadow`].

pub mod backtest;
pub mod calibration_log;
pub mod config;
pub mod emit;
pub mod episodes;
pub mod explanation;
pub mod fit;
pub mod flag_timeline;
pub mod fleet;
pub mod fleet_agreement;
pub mod fleet_events;
pub mod fleet_events_fanout;
pub mod fleet_events_forge;
pub mod fleet_events_pulls;
pub mod fleet_events_reviews;
pub mod fleet_fetch;
pub mod fleet_refresh;
pub mod fleet_state;
pub mod fleet_state_prs;
pub mod friction;
pub mod grid;
pub mod heuristics;
pub mod history;
pub mod journal;
pub mod labels;
pub mod offline;
pub mod queue_features;
pub mod recalibrate;
pub mod score;
pub mod shadow;
pub mod simulate;
pub mod tracker;
pub mod twin_otter;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
pub(crate) mod tests;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::Path;
use std::sync::Arc;

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
    /// A ready issue's sweep is dispatched (#9326).
    Start,
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
            Kind::Start => "start",
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
    /// Ready (`loom:issue`), waiting for a dispatch slot (#9326). One visit
    /// is one slot turnover.
    #[serde(rename = "ready_wait")]
    ReadyWait,
    /// Approved, held for a human (#10218): `loom:pr` plus an operator hold
    /// ([`labels::MERGE_HOLD_LABELS`]). Declared last, so the derived `Ord`
    /// every canonical ordering sorts on is unchanged for the other six.
    #[serde(rename = "merge_hold")]
    MergeHold,
}

/// How many [`Stage`] variants there are: the length of per-stage arrays.
pub const STAGE_COUNT: usize = 7;

impl Stage {
    /// Every post-dispatch stage, in path order. `ready_wait` precedes them
    /// on an unstarted issue's path but is deliberately not in this list, so
    /// every output built over it before #9326 is unchanged ([`Self::EVERY`]
    /// has all seven). `merge_hold` (#10218) is not in it either, for the same
    /// reason: no path-engine heuristic visits it.
    pub const ALL: [Stage; 5] = [
        Stage::SweepCurator,
        Stage::SweepBuilder,
        Stage::ReviewWait,
        Stage::Doctor,
        Stage::MergeWait,
    ];

    /// Every stage, in path order, `ready_wait` first and `merge_hold` last
    /// (it is visited only on a path that starts there, before `merge_wait`).
    pub const EVERY: [Stage; STAGE_COUNT] = [
        Stage::ReadyWait,
        Stage::SweepCurator,
        Stage::SweepBuilder,
        Stage::ReviewWait,
        Stage::Doctor,
        Stage::MergeWait,
        Stage::MergeHold,
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
            Stage::ReadyWait => "ready_wait",
            Stage::MergeHold => "merge_hold",
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
            Stage::ReadyWait => 5,
            Stage::MergeHold => 6,
        }
    }

    /// Whether this stage's duration is **an agent attempt doing work**
    /// rather than a wait (Issue #9420).
    ///
    /// True for the three stages whose whole duration is one role running:
    /// `sweep.curator`, `sweep.builder`, `doctor`. False for `review_wait`,
    /// `merge_wait` and `ready_wait` — those measure how long an item *sat*,
    /// which can legitimately be near zero (a PR that enters `merge_wait`
    /// already mergeable, an issue dispatched into a free slot), so the
    /// zero-duration conditioning [`super::history::StageSample::worked`]
    /// applies must not touch them.
    ///
    /// `review_wait` is deliberately on the wait side even though a
    /// `sweep.outcome` `judge` phase feeds it: the same stage also receives
    /// forge-timeline waits, and one rule cannot be right for both. Leaving it
    /// unconditioned keeps every shipped v1 estimate byte-identical.
    #[must_use]
    pub fn is_role_attempt(self) -> bool {
        matches!(self, Stage::SweepCurator | Stage::SweepBuilder | Stage::Doctor)
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
    /// Not started, and the dispatch plan gives it no position (a blocked
    /// row, no plan on this host, or an issue the plan does not cover).
    NoDispatchPlan,
    /// No stage, or contradictory stage labels.
    UnknownStage,
    /// The inputs are too old to describe the present.
    StaleInputs,
    /// A fitted heuristic (#10243) has no usable coefficient file: none is
    /// loaded, it has no direct model, its cutoff is not strictly before
    /// `as_of`, or its coefficients are malformed.
    NoModel,
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
            NoEstimateReason::NoModel => "no_model",
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
    /// When the current **stage episode** began, when it differs from
    /// `entered_at` (#10218): the instant an operator hold was lifted, for a
    /// PR back in `merge_wait` after a `merge_hold`. `entered_at` keeps the
    /// pooled definition (the approval) that every shipped heuristic reads;
    /// a hold-aware heuristic reads the split age from here. `None` means
    /// "the same as `entered_at`". Not copied into the explanation's
    /// `current_stage`, so no shipped explanation changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_entered_at: Option<DateTime<Utc>>,
}

/// The resolved present of one item: a stage, or the reason it has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurrentState {
    /// In a modelled stage.
    At(CurrentStage),
    /// Refused before any history is consulted.
    Refused(NoEstimateReason),
}

/// Where a ready item stands in the dispatch plan (#9288), as `start-v1`
/// and an unstarted `land-v1` read it. Recorded verbatim in the
/// explanation's `path.dispatch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchInput {
    /// 1-based plan position.
    pub position: u32,
    /// `next` or `queued`.
    pub plan_state: String,
    /// The admission gate holding it, when one does.
    pub gate: Option<String>,
    /// Waiting rows (`next`/`queued`) ahead of it in plan order.
    pub ahead: u32,
    /// Free slots when the tick finished.
    pub free_slots: u32,
    /// Admissions per tick, when capped.
    pub max_admissions_per_tick: Option<u32>,
    /// The work finder's tick interval.
    pub tick_interval_secs: u64,
    /// Whether the saturation brake held every admission this tick.
    pub saturation_held: bool,
    /// The tick the plan came from.
    pub plan_at: DateTime<Utc>,
}

impl DispatchInput {
    /// Slot turnovers that must happen before it can be admitted: one per
    /// waiting row ahead of it, plus its own, less the slots already free.
    #[must_use]
    pub fn turnovers(&self) -> u32 {
        (self.ahead + 1).saturating_sub(self.free_slots)
    }

    /// Fixed seconds from its admitting slot freeing to the dispatch: half a
    /// tick (the mean wait for the next tick), plus one whole tick per full
    /// admission batch ahead of it when it needs no turnover at all.
    #[must_use]
    pub fn admission_delay_sec(&self) -> i64 {
        let tick = i64::try_from(self.tick_interval_secs).unwrap_or(i64::MAX / 4);
        let batches = match (self.turnovers(), self.max_admissions_per_tick) {
            (0, Some(cap)) if cap > 0 => i64::from(self.ahead / cap),
            _ => 0,
        };
        tick / 2 + batches.saturating_mul(tick)
    }
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
    /// The dispatch plan's view of a ready item; `None` for every started
    /// one.
    pub dispatch: Option<DispatchInput>,
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
    /// Whether it models an operator hold (`merge_hold`, #10218) rather than
    /// refusing it `blocked`. The tracker hands such a heuristic the held
    /// item's **modeled** input, with its stage-dependent queue features
    /// counted from `merge_hold` as training counts them, and refreshes its
    /// series while held (#10284). Every other heuristic keeps the described
    /// `blocked` input, byte for byte.
    fn models_hold(&self) -> bool {
        false
    }
}

/// Every shipped heuristic, and which one is `current` per kind.
pub struct Registry {
    heuristics: Vec<Box<dyn Heuristic>>,
    /// The coefficient file the fitted heuristics were built with.
    fit: Option<Arc<fit::CoefficientFile>>,
}

impl Registry {
    /// The built-in heuristics, with no coefficient file: reads nothing, so
    /// every fitted heuristic refuses `no_model`.
    #[must_use]
    pub fn builtin() -> Self {
        Self::with_fit(None)
    }

    /// The built-in heuristics, the fitted ones built with `fit`. Pure.
    /// `land-2026-10-04-twin-otter` (and its pre-PR composition `-b`, last)
    /// are registered **always**, with
    /// or without a file, so its refusals are on the record too.
    #[must_use]
    pub fn with_fit(fit: Option<Arc<fit::CoefficientFile>>) -> Self {
        Registry {
            heuristics: vec![
                Box::new(heuristics::StartV1),
                Box::new(heuristics::FinishV1),
                Box::new(heuristics::LandV1),
                Box::new(heuristics::LandV2),
                Box::new(heuristics::LandV3),
                Box::new(heuristics::LandAmberHeron),
                Box::new(heuristics::LandTwinOtter::new(fit.clone())),
                Box::new(heuristics::LandTwinOtterB::new(fit.clone())),
            ],
            fit,
        }
    }

    /// The built-in heuristics with the newest coefficient file under
    /// `workspace_root` whose cutoff is strictly before `before`
    /// ([`fit::load_latest`]). The registry's only I/O.
    #[must_use]
    pub fn load(workspace_root: &Path, before: DateTime<Utc>) -> Self {
        Self::with_fit(fit::load_latest(workspace_root, before).map(Arc::new))
    }

    /// The coefficient file the fitted heuristics were built with.
    #[must_use]
    pub fn fit(&self) -> Option<&fit::CoefficientFile> {
        self.fit.as_deref()
    }

    /// That file's id, when there is one.
    #[must_use]
    pub fn fit_id(&self) -> Option<&str> {
        self.fit.as_deref().map(|f| f.id.as_str())
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

    /// Every registered heuristic that predicts `kind`, in registration
    /// order — the shadow-mode input (#9328).
    ///
    /// This is what makes a candidate observable at all: the tracker computes
    /// and logs an estimate for each of these, while only
    /// [`Self::current`]'s is the primary one existing consumers read.
    pub fn for_kind(&self, kind: Kind) -> impl Iterator<Item = &dyn Heuristic> {
        self.heuristics
            .iter()
            .filter(move |h| h.kind() == kind)
            .map(std::convert::AsRef::as_ref)
    }

    /// The default `current` heuristic id for `kind`.
    #[must_use]
    pub fn default_current(kind: Kind) -> &'static str {
        match kind {
            Kind::Start => heuristics::START_V1,
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
