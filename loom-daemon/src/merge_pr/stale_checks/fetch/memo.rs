//! Answers that cannot change, kept so a repeated guard run does not
//! re-download them (#11418).
//!
//! - The base a completed job tested, keyed by `(repo, job id, PR head)`:
//!   a finished job's log never changes, and reading it is the guard's
//!   biggest fan-out (one log per required context).
//! - A `compare/B...tip` answer, keyed by `(repo, B, tip)`: both ends are
//!   commit SHAs, so the diff is fixed.
//!
//! Entries live in the shared ETag store directory
//! ([`crate::forge_etag_store::daemon_store_dir`]), owner-only, and are pruned
//! two days after they were written. With no store nothing is kept.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::ChangedFile;

const BASE_PREFIX: &str = "stale-base-";
const COMPARE_PREFIX: &str = "stale-compare-";
const MAX_AGE: Duration = Duration::from_secs(2 * 24 * 60 * 60);

#[derive(serde::Serialize, serde::Deserialize)]
struct TestedBase {
    base: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct File {
    path: String,
    status: String,
    previous_filename: Option<String>,
    patch: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Compare {
    status: String,
    files: Vec<File>,
}

fn entry(prefix: &str, key: &str) -> Option<PathBuf> {
    let dir = crate::forge_etag_store::daemon_store_dir()?;
    Some(crate::forge_etag_store::entry_path_with_prefix(&dir, prefix, key))
}

fn store<T: serde::Serialize>(path: &Path, value: &T) {
    crate::forge_etag_store::write_private_json(path, value);
    if let Some(dir) = path.parent() {
        prune(dir);
    }
}

/// The base job `job_id` tested for `head`, when an earlier run read it.
pub(super) fn tested_base(nwo: &str, job_id: u64, head: &str) -> Option<String> {
    let path = entry(BASE_PREFIX, &format!("{nwo}#{job_id}#{head}"))?;
    crate::forge_etag_store::read_private_json::<TestedBase>(&path).map(|t| t.base)
}

/// Keep the base job `job_id` tested for `head`.
pub(super) fn keep_tested_base(nwo: &str, job_id: u64, head: &str, base: &str) {
    if let Some(path) = entry(BASE_PREFIX, &format!("{nwo}#{job_id}#{head}")) {
        store(
            &path,
            &TestedBase {
                base: base.to_string(),
            },
        );
    }
}

/// An earlier `compare/{base}...{tip}` answer.
pub(super) fn compare(nwo: &str, base: &str, tip: &str) -> Option<(String, Vec<ChangedFile>)> {
    let path = entry(COMPARE_PREFIX, &format!("{nwo}#{base}...{tip}"))?;
    let c = crate::forge_etag_store::read_private_json::<Compare>(&path)?;
    let files = c
        .files
        .into_iter()
        .map(|f| ChangedFile {
            path: f.path,
            status: f.status,
            previous_filename: f.previous_filename,
            patch: f.patch,
        })
        .collect();
    Some((c.status, files))
}

/// Keep a `compare/{base}...{tip}` answer.
pub(super) fn keep_compare(nwo: &str, base: &str, tip: &str, status: &str, files: &[ChangedFile]) {
    if let Some(path) = entry(COMPARE_PREFIX, &format!("{nwo}#{base}...{tip}")) {
        let files = files
            .iter()
            .map(|f| File {
                path: f.path.clone(),
                status: f.status.clone(),
                previous_filename: f.previous_filename.clone(),
                patch: f.patch.clone(),
            })
            .collect();
        store(
            &path,
            &Compare {
                status: status.to_string(),
                files,
            },
        );
    }
}

fn prune(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for e in rd.filter_map(Result::ok) {
        let ours = e
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(BASE_PREFIX) || n.starts_with(COMPARE_PREFIX));
        let stale = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > MAX_AGE);
        if ours && stale {
            let _ = std::fs::remove_file(e.path());
        }
    }
}
