//! The point-in-time log of each open PR's changed-file list (#10550), the
//! source of the open-PR file-overlap predictor of [`super::loop_features`]
//! (#10521, predictor 1).
//!
//! # What is logged
//!
//! One [`FileSnapshot`] `{repo, pr, known_at, files}` per **change** of a
//! PR's list, stamped with the instant the read returned. The builder takes
//! the latest snapshot strictly before `as_of`, so a list first read after
//! `as_of` (or a later push) can never reach a row at `as_of`; before the
//! first read the list is unknown, never "no files". The log starts when the
//! reader first runs, so older training rows keep `overlap_known = 0`.
//!
//! # Reads
//!
//! Forge reads only, never `git`: one ETag'd conditional GET of
//! `pulls/{n}/files` per PR through the reader Apps
//! ([`super::pr_features_forge::fetch_pr_files`]), at most
//! [`FILE_READ_BUDGET`] calls per ETA pass (zero while the rate-limit
//! breaker suppresses polling). [`plan`] reads PRs never read first, then the
//! stalest, and only PRs updated since their last read. A first page of
//! [`MAX_LISTED_FILES`] entries may be truncated, and a partial list would
//! read as a confident smaller overlap, so it is **not** logged (the PR stays
//! unknown).
//!
//! # Persistence
//!
//! `pr-files.jsonl` beside the fleet snapshots
//! ([`super::fleet::snapshot_dir`]); its extension keeps
//! [`super::fleet::load_all`]'s `*.json` listing to snapshots alone. The fit
//! (`eta fit`) and the tracker's serving side read the same file through
//! [`load`], so both pass `files: Some(..)` to the one builder.

use super::loop_features::FileSnapshot;
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Most forge calls the file reads of one ETA pass make.
pub const FILE_READ_BUDGET: usize = 6;

/// The page size of the read. A full page may be truncated and is not logged.
pub const MAX_LISTED_FILES: usize = 100;

/// Snapshots older than this are dropped when the log is compacted: well
/// beyond the fit window's reach into row history.
pub const RETAIN_DAYS: i64 = 120;

/// Compact once the log holds more than this many snapshots.
const COMPACT_ABOVE: usize = 20_000;

/// The log's path under `workspace_root`.
#[must_use]
pub fn log_path(workspace_root: &Path) -> PathBuf {
    super::fleet::snapshot_dir(workspace_root).join("pr-files.jsonl")
}

/// Every snapshot in the log, in file order. Absent or unreadable is empty;
/// a line that does not parse is skipped, not fatal.
#[must_use]
pub fn load(workspace_root: &Path) -> Vec<FileSnapshot> {
    let Ok(text) = std::fs::read_to_string(log_path(workspace_root)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Append `snapshots` to the log.
///
/// # Errors
///
/// The directory could not be created or the write failed.
pub fn append(workspace_root: &Path, snapshots: &[FileSnapshot]) -> std::io::Result<()> {
    if snapshots.is_empty() {
        return Ok(());
    }
    let path = log_path(workspace_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = String::new();
    for s in snapshots {
        out.push_str(&serde_json::to_string(s).map_err(std::io::Error::other)?);
        out.push('\n');
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(out.as_bytes())
}

/// Rewrite the log without snapshots older than [`RETAIN_DAYS`] before `now`,
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
    for s in all.iter().filter(|s| s.known_at >= from) {
        out.push_str(&serde_json::to_string(s).map_err(std::io::Error::other)?);
        out.push('\n');
    }
    let path = log_path(workspace_root);
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, path)
}

/// The sorted, distinct paths of one `pulls/{n}/files` page, or `None` when
/// the body is not a file array or the page is full (possibly truncated).
#[must_use]
pub fn parse_files(body: &Value) -> Option<Vec<String>> {
    let rows = body.as_array()?;
    if rows.len() >= MAX_LISTED_FILES {
        return None;
    }
    let names: Option<BTreeSet<String>> = rows
        .iter()
        .map(|r| {
            r.get("filename")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    names.map(|n| n.into_iter().collect())
}

/// An open PR the reader may read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// `owner/repo`.
    pub repo: String,
    /// The PR.
    pub pr: u32,
    /// Its listing's `updated_at`, when known.
    pub updated_at: Option<DateTime<Utc>>,
}

/// When each PR was last read by this process, whether or not its list
/// changed (a `304` appends nothing, so the log alone cannot say).
#[derive(Debug, Clone, Default)]
pub struct ReadClock(BTreeMap<(String, u32), DateTime<Utc>>);

fn key(repo: &str, pr: u32) -> (String, u32) {
    (repo.to_ascii_lowercase(), pr)
}

fn last_read(
    clock: &ReadClock,
    log: &[FileSnapshot],
    repo: &str,
    pr: u32,
) -> Option<DateTime<Utc>> {
    let logged = log
        .iter()
        .filter(|s| s.pr == pr && s.repo.eq_ignore_ascii_case(repo))
        .map(|s| s.known_at)
        .max();
    clock.0.get(&key(repo, pr)).copied().max(logged)
}

/// The reads of this pass, at most `budget` (one call each): PRs never read
/// first, then the stalest; a PR read since its last update is left alone.
#[must_use]
pub fn plan(
    candidates: &[Candidate],
    log: &[FileSnapshot],
    clock: &ReadClock,
    budget: usize,
) -> Vec<Candidate> {
    let mut wanted: Vec<(Option<DateTime<Utc>>, &Candidate)> = candidates
        .iter()
        .filter_map(|c| {
            let last = last_read(clock, log, &c.repo, c.pr);
            let stale = match (last, c.updated_at) {
                (None, _) => true,
                (Some(last), Some(updated)) => updated > last,
                (Some(_), None) => false,
            };
            stale.then_some((last, c))
        })
        .collect();
    wanted.sort_by(|a, b| (a.0, a.1.pr, &a.1.repo).cmp(&(b.0, b.1.pr, &b.1.repo)));
    wanted
        .into_iter()
        .take(budget)
        .map(|(_, c)| c.clone())
        .collect()
}

/// Run one pass: read what [`plan`] picks through `fetch` (the forge read,
/// `None` when it failed) and return the snapshots to append: one per PR
/// whose list differs from the latest known. `now` stamps each read's
/// return. A failed read is retried next pass; a truncated list is marked
/// read but never logged.
pub fn refresh(
    candidates: &[Candidate],
    log: &[FileSnapshot],
    clock: &mut ReadClock,
    budget: usize,
    now: impl Fn() -> DateTime<Utc>,
    fetch: impl Fn(&Candidate) -> Option<Value>,
) -> Vec<FileSnapshot> {
    let mut out: Vec<FileSnapshot> = Vec::new();
    for c in plan(candidates, log, clock, budget) {
        let Some(body) = fetch(&c) else { continue };
        let at = now();
        clock.0.insert(key(&c.repo, c.pr), at);
        let Some(files) = parse_files(&body) else {
            continue;
        };
        let latest = log
            .iter()
            .chain(out.iter())
            .filter(|s| s.pr == c.pr && s.repo.eq_ignore_ascii_case(&c.repo))
            .max_by_key(|s| s.known_at);
        if latest.is_some_and(|s| s.files == files) {
            continue;
        }
        out.push(FileSnapshot {
            repo: c.repo.to_ascii_lowercase(),
            pr: c.pr,
            known_at: at,
            files,
        });
    }
    out
}
