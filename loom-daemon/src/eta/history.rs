//! Stage-duration history: every observed stage duration, Judge verdict and
//! sweep path, each stamped with the instant it was observed.
//!
//! Sources in this slice: the `sweep.outcome` telemetry journal
//! (`.loom/logs/sweep-outcome-telemetry.jsonl` and its `.1` rotation). Each
//! record contributes its per-phase durations, its Judge verdicts, and
//! whether it merged in-sweep, all observed at the envelope's `emitted_at`.
//!
//! [`StageSamples::select`] is the only read the estimators make, and it
//! refuses every sample observed at or after `as_of`, so an estimate never
//! sees its own future.

use super::{Stage, MAX_SAMPLES, MIN_SAMPLES, WINDOW_DAYS};
use crate::telemetry::{SweepOutcomeRecord, SweepResult, TelemetryEnvelope, TelemetryRecord};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Where a sample came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleSource {
    /// `sweep-outcome-telemetry.jsonl` (in-sweep phases).
    SweepOutcome,
    /// `eta-stage-samples.jsonl` (transitions the ETA tracker observed).
    StageJournal,
}

impl SampleSource {
    /// The journal file name, as recorded in explanations.
    #[must_use]
    pub fn journal(self) -> &'static str {
        match self {
            SampleSource::SweepOutcome => "sweep-outcome-telemetry.jsonl",
            SampleSource::StageJournal => "eta-stage-samples.jsonl",
        }
    }
}

/// One observed stage duration.
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
    /// Judge verdicts.
    pub verdicts: Vec<VerdictSample>,
    /// Successful sweep paths.
    pub paths: Vec<SweepPathSample>,
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

/// A stage's samples, ready to summarise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Level the samples came from.
    pub level: Level,
    /// Durations, ascending.
    pub sorted: Vec<i64>,
}

/// The oldest instant a sample may be observed at for an estimate at `as_of`.
#[must_use]
pub fn window_from(as_of: DateTime<Utc>) -> DateTime<Utc> {
    as_of - Duration::days(WINDOW_DAYS)
}

fn in_window(observed_at: DateTime<Utc>, as_of: DateTime<Utc>) -> bool {
    observed_at < as_of && observed_at >= window_from(as_of)
}

fn same_repo(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

impl StageSamples {
    /// Add every sample one `sweep.outcome` record carries, observed at
    /// `observed_at`.
    ///
    /// Phase durations: each entry is the time up to that phase's
    /// completion, so every entry is a completed stage. A record whose only
    /// entry spans the whole sweep is the journal's fallback when no phase
    /// was ever sampled; its label is the latest phase but its duration is
    /// the whole sweep, so it is skipped.
    pub fn push_outcome(&mut self, record: &SweepOutcomeRecord, observed_at: DateTime<Utc>) {
        let fallback = record.phase_durations.len() == 1
            && record.phase_durations[0].duration_sec == record.total_duration_sec
            && record.total_duration_sec > 0;
        if !fallback {
            for phase in &record.phase_durations {
                if let Some(stage) = Stage::from_sweep_phase(&phase.phase) {
                    if phase.duration_sec >= 0 {
                        self.stages.push(StageSample {
                            repo: record.repo.clone(),
                            stage,
                            duration_sec: phase.duration_sec,
                            observed_at,
                            source: SampleSource::SweepOutcome,
                        });
                    }
                }
            }
        }
        for verdict in record.judge_verdicts.iter().flatten() {
            let rejected = match verdict.verdict.as_str() {
                "fail" => true,
                "pass" => false,
                _ => continue,
            };
            self.verdicts.push(VerdictSample {
                repo: record.repo.clone(),
                attempt: verdict.attempt,
                rejected,
                observed_at,
            });
        }
        if record.result == SweepResult::Success {
            self.paths.push(SweepPathSample {
                repo: record.repo.clone(),
                merged_in_sweep: record.phase_durations.iter().any(|p| p.phase == "merge"),
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
                self.push_outcome(record, envelope.emitted_at);
            }
        }
    }

    /// Load the `sweep.outcome` journal at `path` and its `.1` rotation.
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

    /// [`Self::select`] with an explicit floor.
    #[must_use]
    pub fn select_at(
        &self,
        repo: &str,
        stage: Stage,
        as_of: DateTime<Utc>,
        sources: &[SampleSource],
        min_samples: usize,
    ) -> Option<Selection> {
        for level in [Level::Repo, Level::Host] {
            let mut picked: Vec<&StageSample> = self
                .stages
                .iter()
                .filter(|s| {
                    s.stage == stage
                        && sources.contains(&s.source)
                        && in_window(s.observed_at, as_of)
                        && (level == Level::Host || same_repo(&s.repo, repo))
                })
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
            let mut sorted: Vec<i64> = picked.iter().map(|s| s.duration_sec).collect();
            sorted.sort_unstable();
            return Some(Selection { level, sorted });
        }
        None
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
