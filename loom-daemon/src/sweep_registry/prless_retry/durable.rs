//! The durable half of the PR-less retry tally (Issue #10642).
//!
//! # The gap
//!
//! On `2AMLogic/2am`, 278 sweeps in one day ended `failure` with
//! `failure_class = unclassified:no-phase-signal` — about 90 s each, no
//! checkpoint, no classifier label — and one issue was re-dispatched 47 times.
//! The PR-less bound (#7972) does count such a death when it is reaped: none
//! of the reaper's carve-outs can fire for it (a pre-flight or pool death
//! would have carried that label as its `failure_class`; see
//! `no_phase_tests`). The count still never reached the threshold, because
//! the tally only lives in this process's memory:
//!
//! - a **daemon restart** (self-update restarts on nearly every merge to
//!   `main`) starts every issue's count again at zero, and
//! - a streak whose last release is more than `max_backoff` (1 h) old goes
//!   **cold** and restarts at 1 — so attempts spread out by the backoff
//!   ladder itself, or by other hosts winning the claim in between, never add
//!   up on any one host.
//!
//! # The floor
//!
//! This host's `sweep.outcome` journal already records every one of these
//! deaths, durably and in order, and the reaper writes the current sweep's
//! record before it classifies the outcome for this bound. So when the
//! current outcome is a `no-phase-signal` failure, the count of such
//! failures for the same repo#issue is read back from the journal and used as
//! a lower bound on the in-memory consecutive count:
//!
//! - only records inside [`NO_PHASE_STREAK_WINDOW_SECS`] (a day) count, so
//!   the bound reads "at most `threshold` of these per issue per day";
//! - a record carrying a PR (a landing) ends the streak;
//! - so does any in-process clear ([`SweepRegistry::clear_prless_retry`]: an
//!   open linked PR, an observed merge, a self-reported no-op) — its instant
//!   is kept in [`PrlessTally`] and also persisted to a small sidecar file
//!   next to the journal ([`clear_marks_path`]), so records older than it are
//!   not counted even after a daemon restart.
//!
//! Other PR-less outcomes in between (a substantive failure, a no-op exit)
//! are skipped rather than counted or treated as a clear, so the floor never
//! exceeds what the in-memory tally would have counted on a process that
//! never restarted. Only the measured class is floored; every other outcome
//! is recorded exactly as before.

use super::*;
use crate::telemetry::{
    NoPhaseCause, SweepDisposition, SweepOutcomeRecord, SweepResult, TelemetryEnvelope,
    TelemetryRecord, NO_PHASE_SIGNAL_CLASS,
};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};

/// How far back the durable floor counts `no-phase-signal` deaths (Issue
/// #10642): one day.
pub const NO_PHASE_STREAK_WINDOW_SECS: i64 = 86_400;

/// The registry's PR-less tally: the per-issue states the rest of
/// [`super`] reads and writes (through `Deref`), plus the instant of each
/// issue's most recent clear (Issue #10642).
#[derive(Debug, Default)]
pub(crate) struct PrlessTally {
    states: HashMap<u32, PrlessRetryState>,
    cleared_at: HashMap<u32, DateTime<Utc>>,
}

impl Deref for PrlessTally {
    type Target = HashMap<u32, PrlessRetryState>;

    fn deref(&self) -> &Self::Target {
        &self.states
    }
}

impl DerefMut for PrlessTally {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.states
    }
}

impl PrlessTally {
    /// Record that `issue`'s tally was cleared at `at`, dropping clear marks
    /// older than the floor's window (they can no longer exclude anything).
    pub(crate) fn note_cleared(&mut self, issue: u32, at: DateTime<Utc>) {
        let horizon = at - chrono::Duration::seconds(NO_PHASE_STREAK_WINDOW_SECS);
        self.cleared_at.retain(|_, t| *t >= horizon);
        self.cleared_at.insert(issue, at);
    }

    fn cleared_at(&self, issue: u32) -> Option<DateTime<Utc>> {
        self.cleared_at.get(&issue).copied()
    }
}

/// The sidecar that persists each issue's clear instant: the journal's file
/// name plus `.prless-clears.json`, in the same directory.
fn clear_marks_path(journal: &Path) -> PathBuf {
    let mut name = journal.file_name().unwrap_or_default().to_os_string();
    name.push(".prless-clears.json");
    journal.with_file_name(name)
}

/// Read the persisted clear instants; a missing or unreadable file is empty
/// (the floor then falls back to the window alone, never errors).
fn read_clear_marks(path: &Path) -> HashMap<u32, DateTime<Utc>> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// The durable count for one outcome, and the cause its record carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoPhaseStreak {
    /// `no-phase-signal` failures for the issue since the last landing/clear,
    /// within the window, including the current one.
    pub(crate) count: u32,
    /// The current record's [`NoPhaseCause`], for the reason string.
    pub(crate) cause: Option<NoPhaseCause>,
}

impl SweepRegistry {
    /// Note that `issue`'s tally was cleared at `at`: in memory, and in the
    /// sidecar so the floor still honours it after a daemon restart.
    pub(crate) fn note_prless_cleared(&mut self, issue: u32, at: DateTime<Utc>) {
        self.prless_retry.note_cleared(issue, at);
        let path = clear_marks_path(&self.config.resolve_outcome_telemetry_path());
        let horizon = at - chrono::Duration::seconds(NO_PHASE_STREAK_WINDOW_SECS);
        let mut marks = read_clear_marks(&path);
        marks.retain(|_, t| *t >= horizon);
        marks.insert(issue, at);
        let tmp = path.with_extension("json.tmp");
        let written = serde_json::to_vec(&marks)
            .map_err(std::io::Error::other)
            .and_then(|bytes| {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&tmp, bytes)?;
                std::fs::rename(&tmp, &path)
            });
        if let Err(e) = written {
            log::warn!(
                "could not persist PR-less clear mark for issue #{issue} to {}: {e}",
                path.display()
            );
        }
    }

    /// Record a PR-less release for `sweep_id`'s outcome, floored by the
    /// durable `no-phase-signal` count when that outcome is one (Issue
    /// #10642). Any other outcome is recorded exactly as
    /// [`Self::record_prless_release`] always has.
    pub(crate) fn record_prless_release_for(&mut self, issue: u32, sweep_id: &str, reason: &str) {
        if !self.prless_retry_config.enabled {
            return;
        }
        match self.durable_no_phase_streak(issue, sweep_id, Utc::now()) {
            Some(streak) => {
                let reason = match &streak.cause {
                    Some(cause) => format!("{reason}; recorded cause: {}", cause.summary()),
                    None => reason.to_string(),
                };
                self.record_prless_release_floored(issue, &reason, streak.count);
            }
            None => self.record_prless_release(issue, reason),
        }
    }

    /// The durable streak for `sweep_id`'s outcome at `now`, or `None` when
    /// its record is not in the journal or is not a `no-phase-signal` failure.
    pub(crate) fn durable_no_phase_streak(
        &self,
        issue: u32,
        sweep_id: &str,
        now: DateTime<Utc>,
    ) -> Option<NoPhaseStreak> {
        let window_start = now - chrono::Duration::seconds(NO_PHASE_STREAK_WINDOW_SECS);
        let path = self.config.resolve_outcome_telemetry_path();
        let persisted = read_clear_marks(&clear_marks_path(&path))
            .get(&issue)
            .copied();
        let since = [self.prless_retry.cleared_at(issue), persisted]
            .into_iter()
            .flatten()
            .fold(window_start, std::cmp::max);
        let envelopes = crate::sweep_outcomes::read_all_outcome_telemetry(&path);
        streak_from(&envelopes, issue, sweep_id, since)
    }
}

fn is_no_phase_failure(record: &SweepOutcomeRecord) -> bool {
    record.result == SweepResult::Failure
        && record.failure_class.as_deref() == Some(NO_PHASE_SIGNAL_CLASS)
}

/// The pure count over the journal's envelopes, in append order. See the
/// module doc for the rules.
fn streak_from(
    envelopes: &[TelemetryEnvelope],
    issue: u32,
    sweep_id: &str,
    since: DateTime<Utc>,
) -> Option<NoPhaseStreak> {
    let outcomes: Vec<(DateTime<Utc>, &SweepOutcomeRecord)> = envelopes
        .iter()
        .filter_map(|e| match &e.record {
            TelemetryRecord::SweepOutcome(r) => Some((e.emitted_at, r)),
            _ => None,
        })
        .collect();
    let current = outcomes
        .iter()
        .rev()
        .find(|(_, r)| r.sweep_id == sweep_id)
        .map(|(_, r)| *r)?;
    if current.issue != issue || !is_no_phase_failure(current) {
        return None;
    }
    let mut count = 0u32;
    for (at, record) in outcomes.iter().rev() {
        if record.issue != issue || record.repo != current.repo {
            continue;
        }
        if *at < since
            || record.pr_number.is_some()
            || record.disposition == SweepDisposition::Landed
        {
            break;
        }
        if is_no_phase_failure(record) {
            count = count.saturating_add(1);
        }
    }
    Some(NoPhaseStreak {
        count,
        cause: current.no_phase_cause.clone(),
    })
}
