//! The point-in-time log of observed CI runs (#10737, under #10550), the
//! source of the own-CI predictor of [`super::loop_features`] (#10521,
//! predictor 3) for both `eta fit` and serving.
//!
//! # What is logged
//!
//! One [`CiRecord`] per finished run attempt, keyed `(repo, head_sha)` as
//! the SigNoz `ci.run` row names it ([`CiRecord::from_state`]). It keeps the
//! completion instant (`completed_at`, when the run finished) apart from
//! `known_at` (when this host first saw it, the timeline's `observed_at`), so
//! a run ingested late never reaches a row cut off before it was known.
//! Duplicate delivery of a `(repo, run_id, run_attempt)` keeps the earliest
//! `known_at` ([`fresh`], [`CiLog::new`]).
//!
//! # Mapping to a PR
//!
//! A run names a head, not a PR. [`CiLog::observations`] resolves the PR's
//! head **as known at the cutoff** from the file-list log's head history
//! ([`super::pr_file_log`], read only): the latest snapshot strictly before
//! the cutoff. No snapshot, or a latest snapshot that could not name exactly
//! one head, is an unknown head and the feature stays unknown; runs of any
//! other head (stale heads, branch runs, another repo's same-numbered PR) are
//! never the subject's. Only runs with `known_at` and `completed_at` before
//! the cutoff count.
//!
//! # Meaning of "last CI run"
//!
//! Unchanged from the shipped predictor: the **last completed run**, not
//! "all workflows green". Runs are ordered by `(completed_at, run_id,
//! run_attempt)` (the timeline's `CiState` order), so ties are broken
//! deterministically and a rerun supersedes only once it completes. Outcomes:
//!
//! | conclusion | meaning |
//! |---|---|
//! | `success` | passed |
//! | `failure`, `timed_out`, `startup_failure` | failed |
//! | `cancelled`, `neutral`, `skipped`, `action_required`, `stale`, absent or any other | not an outcome: the run is ignored, never read as a pass or a fail |
//!
//! # Persistence
//!
//! `ci-runs.jsonl` beside the fleet snapshots, like `pr-files.jsonl`; fit and
//! serving read the same file through [`load`].
//!
//! # Not here yet
//!
//! Nothing in the daemon appends to the log yet: the SigNoz timeline reader
//! is not part of the live refresh, and the budgeted forge gap-fill for
//! uncovered windows is not written. Until then the log is absent, every
//! head resolves to no runs, and the predictor stays unknown.

use super::fleet_signoz_timeline::CiRunState;
use super::loop_features::{CiObservation, FileSnapshot};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Records older than this are dropped when the log is compacted.
pub const RETAIN_DAYS: i64 = 120;

/// Compact once the log holds more than this many records.
const COMPACT_ABOVE: usize = 50_000;

/// One finished CI run attempt of one head.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiRecord {
    /// `owner/repo`.
    pub repo: String,
    /// The head commit the run ran at.
    pub head_sha: String,
    /// The workflow run.
    pub run_id: u64,
    /// The attempt (a rerun is a new attempt of the same run).
    pub run_attempt: u32,
    /// The workflow's name.
    pub workflow: String,
    /// When the run finished.
    pub completed_at: DateTime<Utc>,
    /// When this host first observed it.
    pub known_at: DateTime<Utc>,
    /// The run's conclusion, when it carried one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
}

impl CiRecord {
    /// The record of one timeline run, or `None` when it names no head or is
    /// not finished.
    #[must_use]
    pub fn from_state(repo: &str, state: &CiRunState) -> Option<Self> {
        let run = &state.run;
        if run
            .status
            .as_deref()
            .is_some_and(|s| !s.eq_ignore_ascii_case("completed"))
        {
            return None;
        }
        let head_sha = run.head_sha.clone().filter(|h| !h.is_empty())?;
        Some(Self {
            repo: repo.to_ascii_lowercase(),
            head_sha: head_sha.to_ascii_lowercase(),
            run_id: run.run_id,
            run_attempt: run.run_attempt,
            workflow: run.workflow.clone(),
            completed_at: run.completed_at,
            known_at: state.observed_at,
            conclusion: run.conclusion.clone(),
        })
    }

    fn id(&self) -> (String, u64, u32) {
        (self.repo.to_ascii_lowercase(), self.run_id, self.run_attempt)
    }

    /// `Some(failed)` for a conclusion that is an outcome, `None` otherwise.
    #[must_use]
    pub fn failed(&self) -> Option<bool> {
        match self.conclusion.as_deref()?.to_ascii_lowercase().as_str() {
            "success" => Some(false),
            "failure" | "timed_out" | "startup_failure" => Some(true),
            _ => None,
        }
    }
}

/// The log's path under `workspace_root`.
#[must_use]
pub fn log_path(workspace_root: &Path) -> PathBuf {
    super::fleet::snapshot_dir(workspace_root).join("ci-runs.jsonl")
}

/// Every record in the log, in file order. Absent or unreadable is empty; a
/// line that does not parse is skipped.
#[must_use]
pub fn load(workspace_root: &Path) -> Vec<CiRecord> {
    let Ok(text) = std::fs::read_to_string(log_path(workspace_root)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// The records of `incoming` the `log` does not hold yet (one per run
/// attempt; the first delivery wins, keeping its `known_at`).
#[must_use]
pub fn fresh(log: &[CiRecord], incoming: &[CiRecord]) -> Vec<CiRecord> {
    let mut seen: BTreeSet<_> = log.iter().map(CiRecord::id).collect();
    incoming
        .iter()
        .filter(|r| seen.insert(r.id()))
        .cloned()
        .collect()
}

/// Append `records` to the log.
///
/// # Errors
///
/// The directory could not be created or the write failed.
pub fn append(workspace_root: &Path, records: &[CiRecord]) -> std::io::Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let path = log_path(workspace_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = String::new();
    for r in records {
        out.push_str(&serde_json::to_string(r).map_err(std::io::Error::other)?);
        out.push('\n');
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(out.as_bytes())
}

/// Rewrite the log without records first known [`RETAIN_DAYS`] before `now`,
/// once it is large. Atomic (temp file and rename).
///
/// # Errors
///
/// The rewrite failed.
pub fn compact(workspace_root: &Path, now: DateTime<Utc>) -> std::io::Result<()> {
    let all = load(workspace_root);
    if all.len() <= COMPACT_ABOVE {
        return Ok(());
    }
    let from = now - Duration::days(RETAIN_DAYS);
    let mut out = String::new();
    for r in all.iter().filter(|r| r.known_at >= from) {
        out.push_str(&serde_json::to_string(r).map_err(std::io::Error::other)?);
        out.push('\n');
    }
    let path = log_path(workspace_root);
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, path)
}

/// A PR's head observations, oldest first: `(known_at, head)`.
type HeadHistory = Vec<(DateTime<Utc>, Option<String>)>;

/// The logged runs and PR head history, indexed for per-row lookups.
#[derive(Debug, Clone, Default)]
pub struct CiLog {
    /// Records by `(repo, head_sha)`, one per run attempt.
    by_head: BTreeMap<(String, String), Vec<CiRecord>>,
    /// Each PR's head observations, oldest first: `(known_at, head)`; a
    /// `None` head is an observation that named no single head.
    heads: BTreeMap<(String, u32), HeadHistory>,
}

impl CiLog {
    /// Index `records` and the head history of the `files` snapshots.
    #[must_use]
    pub fn new(records: &[CiRecord], files: &[FileSnapshot]) -> Self {
        let mut by_head: BTreeMap<(String, String), Vec<CiRecord>> = BTreeMap::new();
        let mut firsts: BTreeMap<(String, u64, u32), CiRecord> = BTreeMap::new();
        for r in records {
            let slot = firsts.entry(r.id()).or_insert_with(|| r.clone());
            if r.known_at < slot.known_at {
                *slot = r.clone();
            }
        }
        for r in firsts.into_values() {
            by_head
                .entry((r.repo.to_ascii_lowercase(), r.head_sha.to_ascii_lowercase()))
                .or_default()
                .push(r);
        }
        let mut heads: BTreeMap<(String, u32), HeadHistory> = BTreeMap::new();
        for s in files {
            heads
                .entry((s.repo.to_ascii_lowercase(), s.pr))
                .or_default()
                .push((s.known_at, s.head_sha.as_ref().map(|h| h.to_ascii_lowercase())));
        }
        for v in heads.values_mut() {
            v.sort_by_key(|(at, _)| *at);
        }
        Self { by_head, heads }
    }

    /// The head of `pr` known strictly before `as_of`, or `None` when none
    /// was observed or the latest observation named no single head.
    #[must_use]
    pub fn head_at(&self, repo: &str, pr: u32, as_of: DateTime<Utc>) -> Option<&str> {
        let history = self.heads.get(&(repo.to_ascii_lowercase(), pr))?;
        let seen = history.partition_point(|(at, _)| *at < as_of);
        history[..seen].last()?.1.as_deref()
    }

    /// The CI observations of `pr` as known at `as_of`, ascending by the
    /// run order, for [`super::loop_features::LoopInputs::ci`]; `None` when
    /// the PR's head is unknown then. Only runs of that head, finished and
    /// known before `as_of`, whose conclusion is an outcome
    /// ([`CiRecord::failed`]), are returned.
    #[must_use]
    pub fn observations(
        &self,
        repo: &str,
        pr: u32,
        as_of: DateTime<Utc>,
    ) -> Option<Vec<CiObservation>> {
        let head = self.head_at(repo, pr, as_of)?;
        let mut runs: Vec<(&CiRecord, bool)> = self
            .by_head
            .get(&(repo.to_ascii_lowercase(), head.to_string()))
            .into_iter()
            .flatten()
            .filter(|r| r.known_at < as_of && r.completed_at < as_of)
            .filter_map(|r| r.failed().map(|f| (r, f)))
            .collect();
        runs.sort_by_key(|(r, _)| (r.completed_at, r.run_id, r.run_attempt));
        Some(
            runs.into_iter()
                .map(|(r, failed)| CiObservation {
                    pr,
                    at: r.completed_at,
                    failed,
                })
                .collect(),
        )
    }
}
