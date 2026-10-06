//! Stage-duration history: every observed stage duration, Judge verdict and
//! sweep path, each stamped with the instant it was observed and the host
//! that recorded it.
//!
//! Sources: the `sweep.outcome` telemetry journal
//! (`.loom/logs/sweep-outcome-telemetry.jsonl` and its `.1` rotation), whose
//! records contribute per-phase durations, Judge verdicts and whether the
//! sweep merged in-sweep, all observed at the envelope's `emitted_at`; and
//! the ETA tracker's own stage-sample journal (`eta::journal`), which adds
//! the post-sweep stages and external verdicts.
//!
//! [`StageSamples::select`] is the only read the estimators make, and it
//! refuses every sample observed at or after `as_of`, so an estimate never
//! sees its own future.
//!
//! # Two scopes: host-local and fleet-wide (#9343)
//!
//! A [`StageSamples`] value carries the [`super::explanation::HistoryScope`]
//! it was built at, and there are two producers.
//!
//! **Local** ([`StageSamples::load_outcome_journal`] plus
//! [`StageSamples::push_journal`]) reads only **this host's** journals. It is:
//!
//! - **Empty** on most hosts. A host that has not run sweeps for a repo (an
//!   operator's laptop, a freshly added worker) has no samples for it, and
//!   every estimate there is `insufficient_samples`.
//! - **Biased** where history exists. A host sees only the sweeps it ran and
//!   the review transitions it happened to observe, so its distributions
//!   describe its own slots, models and hours, not the repo's.
//! - **Inconsistent** across hosts. Two hosts estimating the same issue at
//!   the same instant read different histories and disagree.
//! - Blind to the **human-gated stages** (review waits, approvals, merges by a
//!   person): they happen on the forge, not on any host.
//!
//! **Fleet** ([`super::fleet`]) is the answer to all four: one forge-derived
//! snapshot per repo, identical on every host that holds it, covering exactly
//! the human-gated stages the local view cannot see. It is built **outside**
//! the estimator — `fleet` derives it from PR label timelines that the CLI
//! fetches, caches it on disk, and hands it here as an ordinary
//! [`StageSamples`] value with `scope = Fleet`.
//!
//! Neither producer lives in the estimator, and nothing in this module fetches
//! anything: an estimator stays a pure function of `(history snapshot, input)`,
//! and a snapshot built only from data observed before `t` keeps a backtest
//! leak-free ([`StageSamples::select`] enforces the second half by refusing
//! every sample observed at or after `as_of`).
//!
//! The fleet-wide *in-sweep* half (#9758) is a second fleet producer,
//! [`super::fleet_signoz`]: the fleet's own `sweep.outcome` records, pulled
//! from SigNoz into a cached snapshot outside the estimator and handed in
//! through [`StageSamples::push_outcome_facts`] — the same conversion the local
//! journal uses — attributed to [`SampleSource::SignozOutcome`] and to the host
//! that actually recorded each sweep.

use super::{Stage, MAX_SAMPLES, MIN_SAMPLES, WINDOW_DAYS};
use crate::telemetry::{
    JudgeVerdict, PhaseDuration, SweepOutcomeRecord, SweepResult, TelemetryEnvelope,
    TelemetryRecord,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

/// Bucket a sweep-outcome sample lands in when the record's `repo` slug was
/// unresolved (Issue #9442) — records never carry paths, so per-repo
/// conditioning groups all unresolved workspaces under this one sentinel
/// instead of one pseudo-repo per host path.
const UNRESOLVED_REPO_BUCKET: &str = "(unresolved)";

/// Where a sample came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleSource {
    /// `sweep-outcome-telemetry.jsonl` (in-sweep phases).
    SweepOutcome,
    /// `eta-stage-samples.jsonl` (transitions the ETA tracker observed).
    StageJournal,
    /// The forge's own PR label timeline, derived fleet-wide (#9343).
    ///
    /// The same *kind* of evidence [`SampleSource::StageJournal`] carries —
    /// a post-dispatch stage boundary read off label events — but derived
    /// centrally from the forge instead of from what one host happened to
    /// watch. See [`SampleSource::admits`] for why that distinction is
    /// recorded without changing which samples a shipped heuristic accepts.
    ForgeTimeline,
    /// The fleet's `sweep.outcome` records, read back from SigNoz (#9758).
    ///
    /// The same measurement as [`SampleSource::SweepOutcome`] — one sweep's
    /// phase durations and whether it merged itself — recorded by whichever
    /// host ran the sweep rather than only this one.
    SignozOutcome,
}

impl SampleSource {
    /// The journal file name, as recorded in explanations.
    #[must_use]
    pub fn journal(self) -> &'static str {
        match self {
            SampleSource::SweepOutcome => "sweep-outcome-telemetry.jsonl",
            SampleSource::StageJournal => "eta-stage-samples.jsonl",
            SampleSource::ForgeTimeline => "forge:pr-timeline",
            SampleSource::SignozOutcome => "signoz:sweep.outcome",
        }
    }

    /// Whether `self`, used as a **filter** entry
    /// ([`super::heuristics::PathRules::sources`]), accepts a sample recorded
    /// with `source`.
    ///
    /// Exact equality, plus one documented equivalence: a filter that asks for
    /// [`Self::StageJournal`] also accepts [`Self::ForgeTimeline`]. Both are
    /// the same measurement — `loom:review-requested` → verdict,
    /// `loom:changes-requested` → push, `loom:pr` → merge, read off forge
    /// label events — and `eta backfill` (#9325) already writes exactly those
    /// rows *into* the stage journal from the same derivation. Without the
    /// equivalence a fleet snapshot would be silently filtered out of every
    /// shipped heuristic and the scope switch would be inert.
    ///
    /// This does **not** make `land-v1` a different heuristic: a local history
    /// never contains a [`Self::ForgeTimeline`] sample, so every local estimate
    /// is byte-identical to before. What changed is the history handed in, not
    /// the function — the property `eta` depends on for backtests.
    ///
    /// The second equivalence (#9758) is the in-sweep twin of the first: a
    /// filter that asks for [`Self::SweepOutcome`] also accepts
    /// [`Self::SignozOutcome`], because a fleet host's exported `sweep.outcome`
    /// is the very record this host's own journal holds for its own sweeps. It
    /// is deliberately one-directional and deliberately **not** extended to
    /// [`Self::StageJournal`]: a stage-journal filter (`start-v1`) asks for
    /// tracker-observed label transitions, and a sweep's phase durations are
    /// not that measurement.
    #[must_use]
    pub fn admits(self, source: SampleSource) -> bool {
        self == source
            || (self == SampleSource::StageJournal && source == SampleSource::ForgeTimeline)
            || (self == SampleSource::SweepOutcome && source == SampleSource::SignozOutcome)
    }
}

/// The facts of one `sweep.outcome` that become history, independent of where
/// the record was read from (#9758): this host's journal
/// ([`StageSamples::push_outcome`]) or the fleet's SigNoz export
/// ([`super::fleet_signoz`]). One conversion, so the two producers cannot
/// drift on which phases count, the whole-sweep fallback, or the worked rule.
#[derive(Debug, Clone, Copy)]
pub struct OutcomeFacts<'a> {
    /// `owner/repo`, `None` when the slug was unresolved.
    pub repo: Option<&'a str>,
    /// The sweep's result.
    pub result: SweepResult,
    /// Whole-sweep seconds.
    pub total_duration_sec: i64,
    /// Per-phase durations.
    pub phase_durations: &'a [PhaseDuration],
    /// Judge verdicts to record, or `None` to record none.
    pub judge_verdicts: Option<&'a [JudgeVerdict]>,
}

/// One observed stage duration. A sample in [`StageSamples::censored`] is the
/// same shape but its `duration_sec` is a **lower bound**: the stage had not
/// completed when it was observed (#9328).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageSample {
    /// `owner/repo`.
    pub repo: String,
    /// The stage.
    pub stage: Stage,
    /// Whole seconds.
    pub duration_sec: i64,
    /// When the sample became known.
    pub observed_at: DateTime<Utc>,
    /// Where it came from.
    pub source: SampleSource,
    /// The host that recorded it (the envelope's `host_id`, or the local
    /// host for its own stage journal).
    pub host: String,
    /// Whether the attempt this duration measures did the stage's work
    /// (Issue #9420) — the sample-level twin of the `loom.attempt.worked`
    /// span attribute ([`crate::observability::lifecycle::ATTEMPT_WORKED`]).
    ///
    /// Three-valued, and only one value is load-bearing:
    ///
    /// - `Some(false)` — this duration is **not** a measurement of the stage's
    ///   work and [`StageSamples::select`] refuses it.
    /// - `Some(true)` / `None` — admitted. `None` is "the producer did not
    ///   say", which almost every local journal row is: the forge timelines
    ///   and all but the zero-second `sweep.outcome` phase rows are
    ///   worked-only by construction (there is no such thing as a phase
    ///   duration for a phase that did not run), so marking them would be
    ///   ceremony, not information.
    ///
    /// Why the field matters far more than the handful of local rows it
    /// refuses today ([`worked_phase`]): the no-op population #9420 measured
    /// lives in the `loom.role_attempt` span aggregate — 80.4% of this host's
    /// 130,657 role ticks over 2026-09-18…10-02 never launched a session, and
    /// their sub-second closes drag the unconditioned median to ~0. That
    /// aggregate is the feed the remaining half of #9343 (fleet-wide in-sweep
    /// samples from SigNoz) is slated to read into this very reader. The
    /// refusal is the precondition that feed has to satisfy, in place before
    /// the feed, rather than a p50 of milliseconds discovered afterwards.
    pub worked: Option<bool>,
}

impl StageSample {
    /// Whether [`StageSamples::select`] admits this sample's `worked`
    /// conditioning (#9420): everything except an explicit `Some(false)`.
    #[must_use]
    pub fn worked_admitted(&self) -> bool {
        self.worked != Some(false)
    }
}

/// One Judge verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictSample {
    /// `owner/repo`.
    pub repo: String,
    /// 1-based attempt on its PR.
    pub attempt: u32,
    /// `loom:changes-requested`.
    pub rejected: bool,
    /// When the verdict became known.
    pub observed_at: DateTime<Utc>,
}

/// Whether one successful sweep merged its own PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepPathSample {
    /// `owner/repo`.
    pub repo: String,
    /// It recorded a `merge` phase.
    pub merged_in_sweep: bool,
    /// When it finished.
    pub observed_at: DateTime<Utc>,
}

/// All history an estimator may read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StageSamples {
    /// Stage durations.
    pub stages: Vec<StageSample>,
    /// Right-censored stage durations (#9328): the stage was still open when
    /// it was observed, so `duration_sec` is a lower bound, not a duration.
    ///
    /// Deliberately a **separate** vector rather than a flag on
    /// [`StageSample`]: [`Self::select`] is the only read every shipped v1
    /// heuristic makes, and keeping censored samples out of `stages` means no
    /// v1 estimate can change by construction. Only a heuristic that asks for
    /// them ([`Self::select_censored`]) sees them.
    pub censored: Vec<StageSample>,
    /// Judge verdicts.
    pub verdicts: Vec<VerdictSample>,
    /// Successful sweep paths.
    pub paths: Vec<SweepPathSample>,
    /// The base `land` heuristic's past estimates and their landings
    /// (#10207), read only by the recalibrating heuristic's point-in-time
    /// [`super::recalibrate::fit_table`]. Built outside the estimator — the
    /// daemon's ETA pass from its outcome log and pending store, `eta
    /// backtest` from a replay — and empty everywhere else, so no other
    /// heuristic's estimate can change by construction.
    pub calibration: Vec<super::recalibrate::CalibrationObservation>,
    /// Stage episodes (#10218): the split stage record a hold-aware heuristic
    /// fits, `merge_hold` and hold-free `merge_wait` included. Read only
    /// through [`Self::select_episodes`]; no shipped heuristic reads it, so
    /// adding it changes no shipped estimate.
    pub episodes: Vec<super::episodes::StageEpisode>,
    /// Whose history this is (#9343): `Local` when every sample came from
    /// this host's own journals, `Fleet` as soon as one host-independent
    /// (forge-derived) sample is in it. [`Self::merge`] is the only thing that
    /// ever raises it, and it never lowers it.
    pub scope: super::explanation::HistoryScope,
    /// `(host_id, sweep_id)` of every local `sweep.outcome` folded in by
    /// [`Self::push_outcome`] (#9758). Not read by any estimator: it is how
    /// `historyScope = augment` keeps a sweep this host journalled **and** the
    /// fleet exported to SigNoz from contributing twice
    /// ([`super::fleet::apply_scope`]).
    pub outcome_keys: BTreeSet<(String, String)>,
}

/// Which level a selection was made at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// The subject's own repo.
    Repo,
    /// Every repo on this host.
    Host,
}

impl Level {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Repo => "repo",
            Level::Host => "host",
        }
    }
}

/// A recency-weighted mean duration ([`StageSamples::weighted_mean`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WeightedMean {
    /// Level the samples came from.
    pub level: Level,
    /// Samples read.
    pub n: usize,
    /// Weighted mean, seconds.
    pub mean_sec: f64,
}

/// A stage's samples, ready to summarise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Level the samples came from.
    pub level: Level,
    /// Durations, ascending.
    pub sorted: Vec<i64>,
    /// How many of them each journal contributed.
    pub by_source: std::collections::BTreeMap<String, usize>,
    /// How many of them each recording host contributed.
    pub by_host: std::collections::BTreeMap<String, usize>,
    /// How many in-window samples of this stage were refused for
    /// `worked = Some(false)` at the level this selection resolved at
    /// (#9420). `0` on every selection a local history produces today.
    ///
    /// Not on the `eta-explanation/v1` wire: the record is a frozen v1
    /// schema, and a count that is always `0` is not worth a schema change.
    /// It is here so the refusal is observable to a test and to a caller that
    /// wants to log it.
    pub excluded_unworked: usize,
}

/// The oldest instant a sample may be observed at for an estimate at `as_of`.
#[must_use]
pub fn window_from(as_of: DateTime<Utc>) -> DateTime<Utc> {
    as_of - Duration::days(WINDOW_DAYS)
}

fn in_window(observed_at: DateTime<Utc>, as_of: DateTime<Utc>) -> bool {
    observed_at < as_of && observed_at >= window_from(as_of)
}

/// The [`Selection`] over `picked`, whose durations (ascending) are `sorted`.
pub(super) fn selection_of(
    level: Level,
    sorted: Vec<i64>,
    picked: &[&StageSample],
    excluded_unworked: usize,
) -> Selection {
    let mut by_source = std::collections::BTreeMap::new();
    let mut by_host = std::collections::BTreeMap::new();
    for sample in picked {
        *by_source
            .entry(sample.source.journal().to_string())
            .or_insert(0) += 1;
        *by_host.entry(sample.host.clone()).or_insert(0) += 1;
    }
    Selection {
        level,
        sorted,
        by_source,
        by_host,
        excluded_unworked,
    }
}

fn same_repo(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Whether any filter entry in `sources` admits a sample recorded with
/// `source` ([`SampleSource::admits`]).
fn admitted(sources: &[SampleSource], source: SampleSource) -> bool {
    sources.iter().any(|filter| filter.admits(source))
}

/// [`StageSample::worked`] for one `sweep.outcome` phase duration (#9420).
///
/// `Some(false)` for a **role-attempt** stage recorded as zero whole seconds:
/// a Curator, Builder or Doctor phase cannot spawn a session, run it and close
/// inside one second, so such a row is an artefact (a phase marked complete
/// without having run), and admitting it is exactly how a median collapses to
/// ~0. Every other row is `None` — "the producer did not say" — because a wait
/// stage can legitimately be ~0 (an already-approved PR entering `merge_wait`)
/// and a positive role phase duration carries no evidence either way beyond
/// being positive.
///
/// Measured almost inert, deliberately: across this host's 2,810 in-window
/// `sweep.outcome` phase samples (every workspace root, 2026-09-18…10-02)
/// exactly **2** are refused — both `curator` rows of 0 s — against nonzero
/// minima of 2 s (builder), 1 s (curator), 3 s (judge) and 89 s (doctor), so
/// the conditioning moves a real estimate by nothing measurable today. Its
/// purpose is the shape the `loom.role_attempt` aggregate actually has —
/// builder p50 of literally 0 ms — which must not enter the estimator
/// unnoticed if that aggregate ever becomes a sample source.
fn worked_phase(stage: Stage, duration_sec: i64) -> Option<bool> {
    (stage.is_role_attempt() && duration_sec == 0).then_some(false)
}

impl StageSamples {
    /// Absorb every sample of `other`.
    ///
    /// The result's [`Self::scope`] is [`HistoryScope::Fleet`] when **either**
    /// side is: a history that contains one host-independent sample is no
    /// longer a description of this host, and saying otherwise in the
    /// explanation would be the exact mislabelling #9343 was filed about.
    /// Scope never goes back down.
    ///
    /// Order is `self`'s samples then `other`'s. That is not load-bearing:
    /// [`Self::select`] sorts by `(observed_at desc, duration asc)`, a total
    /// order on the values it then reads, so a selection does not depend on
    /// the order samples were merged in.
    pub fn merge(&mut self, other: StageSamples) {
        use super::explanation::HistoryScope;
        self.stages.extend(other.stages);
        self.censored.extend(other.censored);
        self.verdicts.extend(other.verdicts);
        self.paths.extend(other.paths);
        self.calibration.extend(other.calibration);
        self.episodes.extend(other.episodes);
        self.outcome_keys.extend(other.outcome_keys);
        if other.scope == HistoryScope::Fleet {
            self.scope = HistoryScope::Fleet;
        }
    }

    /// Add every sample one `sweep.outcome` record carries, observed at
    /// `observed_at`.
    ///
    /// Phase durations: each entry is the time up to that phase's
    /// completion, so every entry is a completed stage. A record whose only
    /// entry spans the whole sweep is the journal's fallback when no phase
    /// was ever sampled; its label is the latest phase but its duration is
    /// the whole sweep, so it is skipped.
    pub fn push_outcome(
        &mut self,
        record: &SweepOutcomeRecord,
        observed_at: DateTime<Utc>,
        host: &str,
    ) {
        self.outcome_keys
            .insert((host.to_string(), record.sweep_id.clone()));
        let facts = OutcomeFacts {
            repo: record.repo.as_deref(),
            result: record.result,
            total_duration_sec: record.total_duration_sec,
            phase_durations: &record.phase_durations,
            judge_verdicts: record.judge_verdicts.as_deref(),
        };
        self.push_outcome_facts(&facts, observed_at, host, SampleSource::SweepOutcome);
    }

    /// Add every sample one outcome's `facts` carry, observed at
    /// `observed_at`, recorded by `host`, attributed to `source` — the one
    /// conversion both outcome producers share (#9758).
    pub fn push_outcome_facts(
        &mut self,
        facts: &OutcomeFacts<'_>,
        observed_at: DateTime<Utc>,
        host: &str,
        source: SampleSource,
    ) {
        let phases = facts.phase_durations;
        let fallback = phases.len() == 1
            && phases[0].duration_sec == facts.total_duration_sec
            && facts.total_duration_sec > 0;
        // Issue #9442: a record whose slug could not be resolved carries no
        // `repo`. The ETA samples condition per-repo, so they bucket under one
        // shared sentinel — strictly better than the pre-#9442 behavior, where
        // the leaked workspace PATH made every host its own pseudo-repo.
        let repo = facts
            .repo
            .map_or_else(|| UNRESOLVED_REPO_BUCKET.to_string(), str::to_string);
        if !fallback {
            for phase in phases {
                if let Some(stage) = Stage::from_sweep_phase(&phase.phase) {
                    if phase.duration_sec >= 0 {
                        self.stages.push(StageSample {
                            repo: repo.clone(),
                            stage,
                            duration_sec: phase.duration_sec,
                            observed_at,
                            source,
                            host: host.to_string(),
                            worked: worked_phase(stage, phase.duration_sec),
                        });
                    }
                }
            }
        }
        for verdict in facts.judge_verdicts.into_iter().flatten() {
            let rejected = match verdict.verdict.as_str() {
                "fail" => true,
                "pass" => false,
                _ => continue,
            };
            self.verdicts.push(VerdictSample {
                repo: repo.clone(),
                attempt: verdict.attempt,
                rejected,
                observed_at,
            });
        }
        if facts.result == SweepResult::Success {
            self.paths.push(SweepPathSample {
                repo,
                merged_in_sweep: phases.iter().any(|p| p.phase == "merge"),
                observed_at,
            });
        }
    }

    /// Add every `sweep.outcome` envelope in `envelopes`.
    pub fn push_envelopes<'a>(
        &mut self,
        envelopes: impl IntoIterator<Item = &'a TelemetryEnvelope>,
    ) {
        for envelope in envelopes {
            if let TelemetryRecord::SweepOutcome(record) = &envelope.record {
                self.push_outcome(record, envelope.emitted_at, &envelope.host_id);
            }
        }
    }

    /// Load the `sweep.outcome` journal at `path` and its `.1` rotation.
    ///
    /// Host-local by construction: this reads one host's file (#9343).
    #[must_use]
    pub fn load_outcome_journal(path: &Path) -> Self {
        let mut samples = StageSamples::default();
        let rotated = path.with_file_name(format!(
            "{}.1",
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
        ));
        for file in [rotated.as_path(), path] {
            let envelopes = crate::sweep_outcomes::read_all_outcome_telemetry(file);
            samples.push_envelopes(&envelopes);
        }
        samples
    }

    /// The samples of `stage` for `repo`, observed in the window before
    /// `as_of`, from `sources`: the most recent [`MAX_SAMPLES`], at repo
    /// level when there are at least [`MIN_SAMPLES`], else host-wide. `None`
    /// when neither level has enough.
    #[must_use]
    pub fn select(
        &self,
        repo: &str,
        stage: Stage,
        as_of: DateTime<Utc>,
        sources: &[SampleSource],
    ) -> Option<Selection> {
        self.select_at(repo, stage, as_of, sources, MIN_SAMPLES)
    }

    /// The recency-weighted mean duration of `stage` (#10208): the samples
    /// [`Self::select`] would read (same level, window, worked-only filter and
    /// [`MAX_SAMPLES`] most recent), each weighted `2^(-age / half_life_sec)`
    /// where `age` is `as_of` minus its `observed_at`.
    #[must_use]
    pub fn weighted_mean(
        &self,
        repo: &str,
        stage: Stage,
        as_of: DateTime<Utc>,
        sources: &[SampleSource],
        half_life_sec: i64,
    ) -> Option<WeightedMean> {
        let level = self.select(repo, stage, as_of, sources)?.level;
        let mut picked: Vec<&StageSample> = self
            .stages
            .iter()
            .filter(|s| {
                s.stage == stage
                    && admitted(sources, s.source)
                    && in_window(s.observed_at, as_of)
                    && (level == Level::Host || same_repo(&s.repo, repo))
                    && s.worked_admitted()
            })
            .collect();
        picked.sort_by(|a, b| {
            b.observed_at
                .cmp(&a.observed_at)
                .then(a.duration_sec.cmp(&b.duration_sec))
        });
        picked.truncate(MAX_SAMPLES);
        let (mut num, mut den) = (0.0_f64, 0.0_f64);
        for s in &picked {
            let age = (as_of - s.observed_at).num_seconds().max(0) as f64;
            let w = (-age / half_life_sec as f64 * std::f64::consts::LN_2).exp();
            num += w * s.duration_sec as f64;
            den += w;
        }
        (den > 0.0).then(|| WeightedMean {
            level,
            n: picked.len(),
            mean_sec: num / den,
        })
    }

    /// [`Self::select`] with an explicit floor.
    ///
    /// # Worked-only conditioning (#9420)
    ///
    /// A sample marked `worked = Some(false)` ([`StageSample::worked`]) is
    /// refused: its duration does not measure an attempt that did the stage's
    /// work, so admitting it would be the millisecond-median defect #9420 was
    /// filed about. The refusal happens *before* `min_samples` is checked, so
    /// a stage whose only evidence is unworked attempts falls through to the
    /// host level and then out entirely — `None`, which the estimators already
    /// report as [`super::NoEstimateReason::InsufficientSamples`]. That is the
    /// "too few conditioned samples ⇒ explicitly unmeasured, never a
    /// fabricated value" fallback, and it is deliberately the **existing**
    /// floor rather than a second mechanism bolted beside it.
    #[must_use]
    pub fn select_at(
        &self,
        repo: &str,
        stage: Stage,
        as_of: DateTime<Utc>,
        sources: &[SampleSource],
        min_samples: usize,
    ) -> Option<Selection> {
        let (level, picked, excluded_unworked) =
            self.pick_observed(repo, stage, as_of, sources, min_samples)?;
        let mut sorted: Vec<i64> = picked.iter().map(|s| s.duration_sec).collect();
        sorted.sort_unstable();
        Some(selection_of(level, sorted, &picked, excluded_unworked))
    }

    /// The observed samples [`Self::select_at`] summarises, before they are
    /// reduced to durations: the level they resolved at, the picked samples
    /// (most recent first, capped at [`MAX_SAMPLES`]) and the unworked count.
    /// Shared with the recency-weighted selection (#10209), which needs each
    /// sample's `observed_at` to weigh it.
    pub(super) fn pick_observed(
        &self,
        repo: &str,
        stage: Stage,
        as_of: DateTime<Utc>,
        sources: &[SampleSource],
        min_samples: usize,
    ) -> Option<(Level, Vec<&StageSample>, usize)> {
        for level in [Level::Repo, Level::Host] {
            let in_scope = |s: &&StageSample| {
                s.stage == stage
                    && admitted(sources, s.source)
                    && in_window(s.observed_at, as_of)
                    && (level == Level::Host || same_repo(&s.repo, repo))
            };
            let excluded_unworked = self
                .stages
                .iter()
                .filter(|s| in_scope(s) && !s.worked_admitted())
                .count();
            let mut picked: Vec<&StageSample> = self
                .stages
                .iter()
                .filter(|s| in_scope(s) && s.worked_admitted())
                .collect();
            if picked.len() < min_samples {
                continue;
            }
            // Most recent first; ties broken by value so the choice is total.
            picked.sort_by(|a, b| {
                b.observed_at
                    .cmp(&a.observed_at)
                    .then(a.duration_sec.cmp(&b.duration_sec))
            });
            picked.truncate(MAX_SAMPLES);
            return Some((level, picked, excluded_unworked));
        }
        None
    }

    /// The **censored** lower bounds of `stage` for `repo` at `level`,
    /// ascending: same window, same source and leak-free rules as
    /// [`Self::select`], but read from [`Self::censored`] (#9328).
    ///
    /// `level` is the level [`Self::select`] already resolved for the same
    /// stage, so the censored set describes the same population as the
    /// observed one — never a repo-level observed grid paired with host-wide
    /// censoring.
    ///
    /// Capped at [`MAX_SAMPLES`] most-recent, exactly as the observed side is:
    /// a censored sample is one sample's worth of evidence either way.
    #[must_use]
    pub fn select_censored(
        &self,
        repo: &str,
        stage: Stage,
        as_of: DateTime<Utc>,
        sources: &[SampleSource],
        level: Level,
    ) -> Vec<i64> {
        let picked = self.pick_censored(repo, stage, as_of, sources, level);
        let mut sorted: Vec<i64> = picked.iter().map(|s| s.duration_sec).collect();
        sorted.sort_unstable();
        sorted
    }

    /// The censored samples [`Self::select_censored`] summarises, most recent
    /// first and capped, before they are reduced to durations.
    pub(super) fn pick_censored(
        &self,
        repo: &str,
        stage: Stage,
        as_of: DateTime<Utc>,
        sources: &[SampleSource],
        level: Level,
    ) -> Vec<&StageSample> {
        let mut picked: Vec<&StageSample> = self
            .censored
            .iter()
            .filter(|s| {
                s.stage == stage
                    && admitted(sources, s.source)
                    && in_window(s.observed_at, as_of)
                    && (level == Level::Host || same_repo(&s.repo, repo))
                    // #9420: same conditioning as the observed side, so the
                    // censored lower bounds describe the same population.
                    && s.worked_admitted()
            })
            .collect();
        picked.sort_by(|a, b| {
            b.observed_at
                .cmp(&a.observed_at)
                .then(a.duration_sec.cmp(&b.duration_sec))
        });
        picked.truncate(MAX_SAMPLES);
        picked
    }

    /// The stage episodes (#10218) of `stage` for `repo`, as a derivation cut
    /// at `as_of` would have produced them
    /// ([`super::episodes::StageEpisode::view_at`]), in canonical order.
    ///
    /// The window rule of [`Self::select`], applied to episodes: every
    /// returned fact was observed strictly before `as_of`. An episode that had
    /// not started is absent; one that ended before `as_of` is as stored; one
    /// still running at `as_of` comes back open (censored) at `as_of`, never
    /// with its later exit. A completed or cut-short episode that ended
    /// before the window opens is dropped; one still running is kept, however
    /// old its entry, because it is live evidence at `as_of`.
    #[must_use]
    pub fn select_episodes(
        &self,
        repo: &str,
        stage: Stage,
        as_of: DateTime<Utc>,
    ) -> Vec<super::episodes::StageEpisode> {
        let from = window_from(as_of);
        let mut picked: Vec<super::episodes::StageEpisode> = self
            .episodes
            .iter()
            .filter(|e| e.stage == stage && same_repo(&e.repo, repo))
            .filter_map(|e| e.view_at(as_of))
            .filter(|e| e.ended_at().is_none_or(|at| at >= from))
            .collect();
        picked.sort_by_key(super::episodes::StageEpisode::key);
        picked.dedup();
        picked
    }

    /// Verdict counts `(n, rejected)` per attempt `1..=cap` at `level`,
    /// observed in the window before `as_of`.
    #[must_use]
    pub fn verdict_counts(
        &self,
        repo: &str,
        level: Level,
        as_of: DateTime<Utc>,
        cap: u32,
    ) -> Vec<(usize, usize)> {
        let mut counts = vec![(0_usize, 0_usize); cap as usize];
        for v in &self.verdicts {
            if !in_window(v.observed_at, as_of)
                || (level == Level::Repo && !same_repo(&v.repo, repo))
                || v.attempt == 0
                || v.attempt > cap
            {
                continue;
            }
            let slot = &mut counts[(v.attempt - 1) as usize];
            slot.0 += 1;
            if v.rejected {
                slot.1 += 1;
            }
        }
        counts
    }

    /// `(n, approved)` over the first Judge verdicts (attempt 1) of `repo`
    /// observed in the window before `as_of`: the numerator of the repo's
    /// first-pass approval rate (#10231). Strictly before `as_of`, by each
    /// verdict's own `observed_at` (when it was recorded), never by when the
    /// PR merged. Counts every feeder of [`StageSamples::verdicts`]:
    /// `sweep.outcome` in-sweep verdicts, external Judge verdicts from the
    /// stage journal (tracker label transitions and `eta backfill` rows), and
    /// fleet-snapshot verdicts under fleet history scope. Samples carry no
    /// source tag, so this cannot (and does not) restrict to one feeder.
    #[must_use]
    pub fn first_pass_approval(&self, repo: &str, as_of: DateTime<Utc>) -> Option<(usize, usize)> {
        let (n, approved) = self
            .verdicts
            .iter()
            .filter(|v| {
                v.attempt == 1 && same_repo(&v.repo, repo) && in_window(v.observed_at, as_of)
            })
            .fold((0_usize, 0_usize), |(n, ok), v| (n + 1, ok + usize::from(!v.rejected)));
        (n > 0).then_some((n, approved))
    }

    /// `(merged-in-sweep share, n)` of the successful sweeps observed in the
    /// window before `as_of`, at repo level when there are at least
    /// [`MIN_SAMPLES`], else host-wide.
    #[must_use]
    pub fn merge_share(&self, repo: &str, as_of: DateTime<Utc>) -> Option<(f64, usize)> {
        for level in [Level::Repo, Level::Host] {
            let picked: Vec<&SweepPathSample> = self
                .paths
                .iter()
                .filter(|p| {
                    in_window(p.observed_at, as_of)
                        && (level == Level::Host || same_repo(&p.repo, repo))
                })
                .collect();
            if picked.len() >= MIN_SAMPLES {
                let merged = picked.iter().filter(|p| p.merged_in_sweep).count();
                return Some((merged as f64 / picked.len() as f64, picked.len()));
            }
        }
        None
    }
}
