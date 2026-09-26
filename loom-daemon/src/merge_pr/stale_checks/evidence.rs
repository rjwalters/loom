//! Deriving `B`, `D` and `P` for the input-scoped freshness predicate (#8919).
//!
//! Three pure transforms, split from the forge I/O in [`super::fetch`] so each
//! is unit-testable against a captured payload:
//!
//! 1. [`parse_tested_base`] — read `B` out of a job's checkout log.
//! 2. [`compare_usable`] — decide whether a `compare/B...tip` answer may be
//!    trusted at all.
//! 3. [`strip_validated_restamps`] — discount the version restamp `main` takes
//!    on nearly every merge, so it does not make every check stale.
//!
//! Every failure here returns `Err(reason)`, and every caller's response to an
//! `Err` is "fall back to #8248's `started_at` rule and say so on stderr" —
//! never "assume fresh".

use super::inputs::FileSet;

/// One changed file, as both `compare` and `pulls/{n}/files` report it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    /// `added`, `modified`, `removed`, `renamed`, `copied`, `changed`, …
    pub status: String,
    /// For a rename, the path it moved away from.
    pub previous_filename: Option<String>,
    /// The unified diff hunk, when the API supplied one. `None` for a file
    /// whose patch GitHub suppressed (too large, binary) — which
    /// [`strip_validated_restamps`] treats as a REAL change, never a restamp.
    pub patch: Option<String>,
}

impl ChangedFile {
    #[must_use]
    pub fn is_removal(&self) -> bool {
        self.status == "removed"
    }
}

/// Project changed files onto a [`FileSet`], keeping a rename's BOTH names and
/// recording the vacated one as a removal (the removal clause's input).
#[must_use]
pub fn to_file_set(files: &[ChangedFile]) -> FileSet {
    let mut set = FileSet::default();
    for f in files {
        set.paths.insert(f.path.clone());
        if f.is_removal() {
            set.removed.insert(f.path.clone());
        }
        if let Some(prev) = &f.previous_filename {
            if prev != &f.path {
                set.paths.insert(prev.clone());
                set.removed.insert(prev.clone());
            }
        }
    }
    set
}

/// The maximum `files` length a `compare` answer may have before it must be
/// treated as possibly truncated. GitHub caps `compare`'s file list at 300.
pub const COMPARE_FILE_CAP: usize = 300;

/// Is a `compare/B...tip` answer usable as `D`?
///
/// `status` must be `ahead` (tip is a descendant of `B`) or `identical`. A
/// `diverged` or `behind` answer means the file list is not "what landed on the
/// base since the check ran", and a list at/over [`COMPARE_FILE_CAP`] may be
/// truncated — a truncated `D` can silently drop the very file that makes the
/// check stale, which is the one error direction this guard may not take.
pub fn compare_usable(status: &str, files_len: usize) -> Result<(), String> {
    if status != "ahead" && status != "identical" {
        return Err(format!(
            "the base-move compare reported status '{status}' rather than ahead/identical, so its \
file list is not 'what landed on the base since this check ran'"
        ));
    }
    if files_len >= COMPARE_FILE_CAP {
        return Err(format!(
            "the base-move compare listed {files_len} files (at or over GitHub's {COMPARE_FILE_CAP}\
-file cap), so the list may be truncated and a missing entry could hide a real input change"
        ));
    }
    Ok(())
}

// --- The tested base B -------------------------------------------------------

/// Read the base a run actually tested out of its checkout log.
///
/// `actions/checkout` logs the test merge commit it checked out as
/// `HEAD is now at <abbrev> Merge <head> into <base>`, where `<head>`/`<base>`
/// are GitHub's own SHAs for the PR head and the base it built the merge
/// against. That line is what makes `B` re-run-invariant: a re-run replays the
/// SAME `GITHUB_SHA`, so it logs the SAME merge commit.
///
/// `pr_head` must match the logged head — either may be abbreviated, so a
/// prefix relation in either direction counts. A head mismatch means the log
/// belongs to some other commit's run, and using its base would measure the
/// wrong move: that is an `Err`, not a silent acceptance.
pub fn parse_tested_base(log: &str, pr_head: &str) -> Result<String, String> {
    let mut saw_head: Option<String> = None;
    for line in log.lines() {
        let Some((head, base)) = merge_pair(line) else {
            continue;
        };
        if sha_matches(&head, pr_head) {
            return Ok(base);
        }
        saw_head.get_or_insert(head);
    }
    match saw_head {
        Some(other) => Err(format!(
            "the run's checkout log reports a test merge of head {other}, not this PR's head \
{pr_head} — the base it tested cannot be attributed to this head"
        )),
        None => Err(
            "the run's checkout log has no 'Merge <head> into <base>' line, so the base it tested \
cannot be read"
                .to_string(),
        ),
    }
}

/// Extract `(head, base)` from a `… Merge <head> into <base>…` line, requiring
/// both to look like SHAs so ordinary prose mentioning "merge" cannot match.
fn merge_pair(line: &str) -> Option<(String, String)> {
    let rest = line.split(" Merge ").nth(1)?;
    let (head, rest) = rest.split_once(" into ")?;
    let head = trim_sha(head)?;
    let base = trim_sha(rest.split_whitespace().next()?)?;
    Some((head, base))
}

/// Accept a hex SHA of at least 7 nibbles, tolerating the trailing ellipsis
/// GitHub's own renderings add (`803f0c7d…`, `803f0c7d...`).
fn trim_sha(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .trim()
        .trim_end_matches('…')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if cleaned.len() >= 7 && cleaned.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(cleaned)
    } else {
        None
    }
}

/// Do two (possibly abbreviated) SHAs name the same commit?
fn sha_matches(a: &str, b: &str) -> bool {
    let (a, b) = (a.to_ascii_lowercase(), b.to_ascii_lowercase());
    let n = a.len().min(b.len());
    n >= 7 && a[..n] == b[..n]
}

// --- Validated version restamps ---------------------------------------------

/// The files a post-merge release bump rewrites, and the ONLY paths a restamp
/// may be discounted on. `scripts/version.sh list` names the same set.
pub const RESTAMP_PATHS: &[&str] = &[
    "VERSION",
    "Cargo.toml",
    "Cargo.lock",
    "package.json",
    "mcp-loom/package.json",
    "mcp-loom/package-lock.json",
    ".loom/install-metadata.json",
];

/// Discount the version restamp from `D`.
///
/// `main` bumps `VERSION` on nearly every merge, so without this the base-move
/// diff is never empty and the input-scoped predicate degenerates back to "any
/// move is stale". The discount is deliberately narrow, because a version file
/// is also where a real dependency bump lands:
///
/// - `VERSION` itself must be in the diff, and must **strictly increase**
///   (a decrease is not a restamp; it is somebody rewriting history).
/// - Only the paths in [`RESTAMP_PATHS`] are candidates.
/// - A candidate is discounted only if **every** changed line in its patch is a
///   version-value line: a removed line carrying `VERSION@B`, or an added line
///   carrying `VERSION@tip`. A `loom_commit` hex line counts too, but **only**
///   in `.loom/install-metadata.json`.
/// - A missing patch, a status other than `modified`, or one stray line makes
///   the file a real change. So a `Cargo.lock` dependency bump — whose changed
///   lines carry some *other* package's version — is never mistaken for one.
///
/// Returns the surviving files (`D` proper). On any failure to establish the
/// version pair, NOTHING is discounted: the fail-closed direction.
#[must_use]
pub fn strip_validated_restamps(files: &[ChangedFile]) -> Vec<ChangedFile> {
    let Some((old, new)) = version_pair(files) else {
        return files.to_vec();
    };
    files
        .iter()
        .filter(|f| !is_pure_restamp(f, &old, &new))
        .cloned()
        .collect()
}

/// `(VERSION@B, VERSION@tip)` when the diff carries a single, strictly
/// increasing `VERSION` edit; `None` otherwise (which disables all discounting).
fn version_pair(files: &[ChangedFile]) -> Option<(String, String)> {
    let v = files.iter().find(|f| f.path == "VERSION")?;
    if v.status != "modified" {
        return None;
    }
    let (removed, added) = changed_lines(v.patch.as_deref()?);
    if removed.len() != 1 || added.len() != 1 {
        return None;
    }
    let old = removed[0].trim().to_string();
    let new = added[0].trim().to_string();
    let (a, b) = (semver(&old)?, semver(&new)?);
    if b > a {
        Some((old, new))
    } else {
        None
    }
}

/// `major.minor.patch` as a comparable tuple; `None` for anything else.
fn semver(raw: &str) -> Option<(u64, u64, u64)> {
    let core = raw.split(['-', '+']).next()?;
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    let patch = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// The `-`/`+` content lines of a unified diff, headers excluded.
fn changed_lines(patch: &str) -> (Vec<String>, Vec<String>) {
    let mut removed = Vec::new();
    let mut added = Vec::new();
    for line in patch.lines() {
        if line.starts_with("@@")
            || line.starts_with("---")
            || line.starts_with("+++")
            || line.starts_with("diff ")
            || line.starts_with('\\')
        {
            continue;
        }
        if let Some(rest) = line.strip_prefix('-') {
            removed.push(rest.to_string());
        } else if let Some(rest) = line.strip_prefix('+') {
            added.push(rest.to_string());
        }
    }
    (removed, added)
}

/// Is every changed line of this file's patch a version-value line for the
/// validated `old` → `new` pair?
fn is_pure_restamp(f: &ChangedFile, old: &str, new: &str) -> bool {
    if !RESTAMP_PATHS.contains(&f.path.as_str()) || f.status != "modified" {
        return false;
    }
    let Some(patch) = f.patch.as_deref() else {
        return false; // A suppressed patch is an unknown, and unknowns are real.
    };
    let (removed, added) = changed_lines(patch);
    if removed.is_empty() && added.is_empty() {
        return false;
    }
    let commit_ok = f.path == ".loom/install-metadata.json";
    removed
        .iter()
        .all(|l| l.contains(old) || (commit_ok && is_loom_commit_line(l)))
        && added
            .iter()
            .all(|l| l.contains(new) || (commit_ok && is_loom_commit_line(l)))
}

/// `"loom_commit": "<hex>"` — the install-metadata field a resync rewrites
/// alongside the version, and the only non-version line a restamp may carry.
fn is_loom_commit_line(line: &str) -> bool {
    let Some(rest) = line.split("\"loom_commit\"").nth(1) else {
        return false;
    };
    let Some(rest) = rest.split_once(':') else {
        return false;
    };
    let value = rest.1.trim().trim_end_matches(',').trim_matches('"');
    value.len() >= 7 && value.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests;
