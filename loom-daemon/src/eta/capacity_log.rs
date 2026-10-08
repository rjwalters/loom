//! The persisted fleet capacity series (#10959), beside the fleet snapshots
//! like [`super::pr_file_log`].
//!
//! One [`CapacityRow`] `{known_at, repo, source, features}` per instant and
//! repo. [`Source::Live`] rows are written by the ETA authority each pass
//! (its own token pool / breaker / quota readings,
//! [`super::capacity_features::with_stall`]); [`Source::Backfill`] rows are
//! derived from SigNoz `queue.snapshot` and `ci.*` rows ([`backfill`]) on an
//! hourly grid, keyed by the rows' `observed_timestamp` (so a delayed row
//! reaches only the instants after it was knowable, [`super::point_in_time`]).
//! A reader takes [`latest_before`] `as_of`: strictly earlier than `as_of`,
//! so a row written at or after `as_of` can never reach a row at `as_of`.
//!
//! `capacity.jsonl`'s extension keeps [`super::fleet::load_all`]'s `*.json`
//! listing to snapshots alone. Coverage of the backfill is reported with
//! [`uncovered`] (`Coverage::covers`).

use super::capacity_features::{build, CapacityFeatures};
use super::fleet_signoz_timeline::{Family, Timeline};
use super::fleet_signoz_timeline_rows::Row;
use chrono::{DateTime, Duration, DurationRound, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Rows older than this are dropped when the log is compacted, and the
/// backfill never reaches further back (so compaction and backfill cannot
/// churn the same instants).
///
/// Twice the daily fit's [`super::fit::WINDOW_DAYS`]: an estimate at `as_of`
/// reads one window back, and a walk-forward fold replaying an instant one
/// window ago needs a second window behind *that* (the reasoning of
/// [`super::fleet::RETENTION_DAYS`], at the fit's window). With one live and
/// one backfill row per repo per hour, that bounds the steady state at about
/// `48 × RETAIN_DAYS` rows per repo.
pub const RETAIN_DAYS: i64 = 2 * super::fit::WINDOW_DAYS;

/// Below this size the log is not even read for compaction (one `metadata`
/// call per pass).
pub const COMPACT_ABOVE_BYTES: u64 = 8 * 1024 * 1024;

/// The log is examined for compaction at most this often per process, so a
/// log whose retained rows alone exceed [`COMPACT_ABOVE_BYTES`] is not
/// reparsed every pass.
pub const COMPACT_EVERY_HOURS: i64 = 24;

/// Backfill grid step.
pub const BACKFILL_STEP_SEC: i64 = 3600;

/// Most backfill instants computed per call (each is one timeline build).
pub const BACKFILL_PER_PASS: usize = 96;

/// Where a row came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Live,
    Backfill,
}

/// One logged instant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapacityRow {
    /// When the row's inputs were knowable (a live row: when it was written).
    pub known_at: DateTime<Utc>,
    /// Lowercased `owner/repo`.
    pub repo: String,
    pub source: Source,
    #[serde(default)]
    pub features: CapacityFeatures,
}

/// The log's path under `workspace_root`.
#[must_use]
pub fn log_path(workspace_root: &Path) -> PathBuf {
    super::fleet::snapshot_dir(workspace_root).join("capacity.jsonl")
}

/// Every row in file order. Absent or unreadable is empty; a bad line is
/// skipped.
#[must_use]
pub fn load(workspace_root: &Path) -> Vec<CapacityRow> {
    let Ok(text) = std::fs::read_to_string(log_path(workspace_root)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Append `rows` to the log.
///
/// # Errors
///
/// The directory could not be created or the write failed.
pub fn append(workspace_root: &Path, rows: &[CapacityRow]) -> std::io::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let path = log_path(workspace_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = String::new();
    for r in rows {
        out.push_str(&serde_json::to_string(r).map_err(std::io::Error::other)?);
        out.push('\n');
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(out.as_bytes())
}

/// Rewrite the log without rows older than [`RETAIN_DAYS`], once it is larger
/// than [`COMPACT_ABOVE_BYTES`]. `None` when the size gate skipped it without
/// reading the file; else the rows dropped (0 leaves the file untouched).
///
/// # Errors
///
/// The rewrite failed.
pub fn compact(workspace_root: &Path, now: DateTime<Utc>) -> std::io::Result<Option<usize>> {
    compact_above(workspace_root, now, COMPACT_ABOVE_BYTES)
}

/// [`compact`] at an explicit size gate.
///
/// # Errors
///
/// The rewrite failed.
pub fn compact_above(
    workspace_root: &Path,
    now: DateTime<Utc>,
    above_bytes: u64,
) -> std::io::Result<Option<usize>> {
    let path = log_path(workspace_root);
    match std::fs::metadata(&path) {
        Ok(m) if m.len() > above_bytes => {}
        _ => return Ok(None),
    }
    let all = load(workspace_root);
    let from = now - Duration::days(RETAIN_DAYS);
    let kept: Vec<&CapacityRow> = all.iter().filter(|r| r.known_at >= from).collect();
    let dropped = all.len() - kept.len();
    if dropped == 0 {
        return Ok(Some(0));
    }
    let mut out = String::new();
    for r in kept {
        out.push_str(&serde_json::to_string(r).map_err(std::io::Error::other)?);
        out.push('\n');
    }
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, path)?;
    Ok(Some(dropped))
}

/// The authority's per-process write cadence for the log: one live row per
/// repo per [`BACKFILL_STEP_SEC`] grid hour (the backfill's cadence; the fit
/// reads [`latest_before`], so the five-minute passes in between add no
/// information), and a compaction examined at most every
/// [`COMPACT_EVERY_HOURS`].
#[derive(Debug, Default)]
pub struct WriteClock {
    live_hour: Option<DateTime<Utc>>,
    examined_at: Option<DateTime<Utc>>,
}

impl WriteClock {
    /// Whether this pass writes live rows: the first pass of each grid hour.
    pub fn take_live(&mut self, now: DateTime<Utc>) -> bool {
        let Ok(hour) = now.duration_trunc(Duration::seconds(BACKFILL_STEP_SEC)) else {
            return false;
        };
        if self.live_hour.is_some_and(|h| h >= hour) {
            return false;
        }
        self.live_hour = Some(hour);
        true
    }

    /// [`compact`], unless the log was examined within
    /// [`COMPACT_EVERY_HOURS`].
    ///
    /// # Errors
    ///
    /// The rewrite failed.
    pub fn compact(
        &mut self,
        workspace_root: &Path,
        now: DateTime<Utc>,
    ) -> std::io::Result<Option<usize>> {
        if self
            .examined_at
            .is_some_and(|at| now - at < Duration::hours(COMPACT_EVERY_HOURS))
        {
            return Ok(None);
        }
        let got = compact(workspace_root, now);
        if !matches!(got, Ok(None)) {
            self.examined_at = Some(now);
        }
        got
    }
}

/// The latest row of `repo` known strictly before `as_of` (a live row beats a
/// backfill row of the same instant), or `None` before the first.
#[must_use]
pub fn latest_before<'a>(
    rows: &'a [CapacityRow],
    repo: &str,
    as_of: DateTime<Utc>,
) -> Option<&'a CapacityRow> {
    rows.iter()
        .filter(|r| r.known_at < as_of && r.repo.eq_ignore_ascii_case(repo))
        .max_by_key(|r| (r.known_at, r.source == Source::Live))
}

/// The grid instants of `repo` already backfilled.
#[must_use]
pub fn backfilled_instants(rows: &[CapacityRow], repo: &str) -> BTreeSet<DateTime<Utc>> {
    rows.iter()
        .filter(|r| r.source == Source::Backfill && r.repo.eq_ignore_ascii_case(repo))
        .map(|r| r.known_at)
        .collect()
}

/// The hourly grid instants in `(from, to]` not in `have`, oldest first,
/// at most [`BACKFILL_PER_PASS`].
#[must_use]
pub fn missing_instants(
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    have: &BTreeSet<DateTime<Utc>>,
) -> Vec<DateTime<Utc>> {
    let step = Duration::seconds(BACKFILL_STEP_SEC);
    let Ok(mut at) = from.duration_trunc(step) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    while at <= to && out.len() < BACKFILL_PER_PASS {
        if at > from && !have.contains(&at) {
            out.push(at);
        }
        at += step;
    }
    out
}

/// The grid instants of the `history_days` window (at most [`RETAIN_DAYS`])
/// ending at `listed_at` that are worth backfilling: [`missing_instants`] from the first instant at which
/// `timeline` knows queue or CI, so a long empty prefix cannot fill every pass's
/// [`BACKFILL_PER_PASS`] budget and pin the scan there. Empty when neither is
/// known.
#[must_use]
pub fn backfill_instants(
    timeline: &Timeline,
    listed_at: DateTime<Utc>,
    history_days: i64,
    have: &BTreeSet<DateTime<Utc>>,
) -> Vec<DateTime<Utc>> {
    let Some(first) = [Family::Queue, Family::Ci]
        .iter()
        .filter_map(|f| timeline.coverage.first(*f))
        .min()
    else {
        return Vec::new();
    };
    let window = listed_at - Duration::days(history_days.min(RETAIN_DAYS));
    let from = window.max(first - Duration::seconds(BACKFILL_STEP_SEC));
    missing_instants(from, listed_at, have)
}

/// The families of `timeline` that do not reach back `window_days` before
/// `cutoff` ([`super::fleet_signoz_timeline::Coverage::covers`]).
#[must_use]
pub fn uncovered(timeline: &Timeline, cutoff: DateTime<Utc>, window_days: i64) -> Vec<Family> {
    [Family::Queue, Family::Ci]
        .into_iter()
        .filter(|f| !timeline.coverage.covers(*f, cutoff, window_days))
        .collect()
}

/// Backfill rows for `repo` at each of `instants`, from the walked SigNoz
/// `rows`: the timeline as knowable at the instant, through the one builder.
/// An instant at which SigNoz knows neither queue nor CI yields no row.
#[must_use]
pub fn backfill(rows: &[Row], repo: &str, instants: &[DateTime<Utc>]) -> Vec<CapacityRow> {
    instants
        .iter()
        .filter_map(|at| {
            let timeline = Timeline::build(rows, *at);
            let known = [Family::Queue, Family::Ci]
                .iter()
                .any(|f| timeline.coverage.first(*f).is_some());
            known.then(|| CapacityRow {
                known_at: *at,
                repo: repo.to_ascii_lowercase(),
                source: Source::Backfill,
                features: build(&timeline, repo),
            })
        })
        .collect()
}
