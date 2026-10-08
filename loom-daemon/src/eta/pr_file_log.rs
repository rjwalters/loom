//! The point-in-time log of each open PR's changed-file list (#10550), the
//! source of the open-PR file-overlap predictor of [`super::loop_features`]
//! (#10521, predictor 1).
//!
//! # What is logged
//!
//! One [`FileSnapshot`] `{repo, pr, known_at, files, head_sha, complete}`
//! per **change** of a PR's observation, stamped with the instant the read returned. The builder takes
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
//! read as a confident smaller overlap, so its paths are **not** logged:
//! the read logs an explicit incomplete observation (`complete: false`)
//! stamped at that instant instead, so the PR is unknown from then on, and an
//! older complete list (say, before the PR grew past a page) still serves
//! cutoffs before it but is never served as current after it.
//!
//! # Head identity
//!
//! Each snapshot carries the head commit its list describes, read from the
//! page itself (every entry's `contents_url` `?ref=` / `blob_url` names it),
//! so the observation is bound to its head even though the files URL is not.
//! A page whose entries name different heads (a push landed mid-read) is
//! logged incomplete. A change of head is a change, logged even when the
//! paths are equal, so distinct heads keep their identity history.
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

/// The page size of the read. A full page may be truncated: its paths are
/// not logged, only an incomplete observation.
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

/// One parsed `pulls/{n}/files` page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// The sorted, distinct paths, or `None` when the page is not a whole
    /// list: full (possibly truncated) or naming more than one head.
    pub files: Option<Vec<String>>,
    /// The head commit the entries name, when they name exactly one.
    pub head_sha: Option<String>,
    /// Lines added, summed over the entries; `None` unless `files` is a whole
    /// list and every entry carried the stat.
    pub additions: Option<u32>,
    /// Lines deleted; same rule as `additions`.
    pub deletions: Option<u32>,
    /// The number of entries on the page, whole list or not.
    pub listed: u32,
}

/// Sum one numeric field over every entry, `None` if any entry lacks it.
fn sum_field(rows: &[Value], field: &str) -> Option<u32> {
    rows.iter().try_fold(0u32, |acc, r| {
        let n = u32::try_from(r.get(field).and_then(Value::as_u64)?).ok()?;
        acc.checked_add(n)
    })
}

/// The head commit one file entry names: its `contents_url`'s `ref`, else
/// the commit segment of its `blob_url`.
fn entry_head(row: &Value) -> Option<String> {
    let from_contents = row
        .get("contents_url")
        .and_then(Value::as_str)
        .and_then(|u| u.split_once("?ref=").or_else(|| u.split_once("&ref=")))
        .map(|(_, rest)| rest.split('&').next().unwrap_or(rest).to_string());
    let from_blob = || {
        row.get("blob_url")
            .and_then(Value::as_str)
            .and_then(|u| u.split_once("/blob/"))
            .and_then(|(_, rest)| rest.split('/').next())
            .map(str::to_string)
    };
    from_contents.or_else(from_blob).filter(|s| !s.is_empty())
}

/// Parse one `pulls/{n}/files` page, or `None` when the body is not a file
/// array (a failed read, retried next pass).
#[must_use]
pub fn parse_page(body: &Value) -> Option<Page> {
    let rows = body.as_array()?;
    let names: BTreeSet<String> = rows
        .iter()
        .map(|r| {
            r.get("filename")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect::<Option<_>>()?;
    let heads: BTreeSet<String> = rows.iter().filter_map(entry_head).collect();
    let one_head = heads.len() <= 1;
    let head_sha = if one_head {
        heads.into_iter().next()
    } else {
        None
    };
    let whole = rows.len() < MAX_LISTED_FILES && one_head;
    Some(Page {
        files: whole.then(|| names.into_iter().collect()),
        head_sha,
        additions: if whole {
            sum_field(rows, "additions")
        } else {
            None
        },
        deletions: if whole {
            sum_field(rows, "deletions")
        } else {
            None
        },
        listed: u32::try_from(rows.len()).unwrap_or(u32::MAX),
    })
}

/// The sorted, distinct paths of one `pulls/{n}/files` page, or `None` when
/// the body is not a file array or the page is not a whole list
/// ([`parse_page`]).
#[must_use]
pub fn parse_files(body: &Value) -> Option<Vec<String>> {
    parse_page(body)?.files
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

impl ReadClock {
    /// Forget `pr`'s last read, so the next pass reads it again: for a read
    /// whose observation could not be persisted.
    pub fn forget(&mut self, repo: &str, pr: u32) {
        self.0.remove(&key(repo, pr));
    }
}

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
/// whose observation (paths, head, completeness) differs from the latest
/// known. `now` stamps each read's return. A failed or malformed read is
/// retried next pass; a page that is not a whole list is logged as an
/// incomplete observation, so the PR reads unknown from that instant.
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
        let Some(page) = fetch(&c).as_ref().and_then(parse_page) else {
            continue;
        };
        let at = now();
        clock.0.insert(key(&c.repo, c.pr), at);
        let complete = page.files.is_some();
        let files = page.files.unwrap_or_default();
        let latest = log
            .iter()
            .chain(out.iter())
            .filter(|s| s.pr == c.pr && s.repo.eq_ignore_ascii_case(&c.repo))
            .max_by_key(|s| s.known_at);
        if latest.is_some_and(|s| {
            s.complete == complete
                && s.files == files
                && s.head_sha == page.head_sha
                && s.additions == page.additions
                && s.deletions == page.deletions
                && s.listed == Some(page.listed)
        }) {
            continue;
        }
        out.push(FileSnapshot {
            repo: c.repo.to_ascii_lowercase(),
            pr: c.pr,
            known_at: at,
            files,
            head_sha: page.head_sha,
            complete,
            additions: page.additions,
            deletions: page.deletions,
            listed: Some(page.listed),
        });
    }
    out
}
