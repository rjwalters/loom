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

/// The one path whose *install-bookkeeping* fields a resync rewrites without
/// touching `VERSION` — see [`strip_validated_restamps`].
pub const INSTALL_METADATA: &str = ".loom/install-metadata.json";

/// Discount the two machine-written restamps from `D`: the release version
/// bump, and the `chore: resync installed Loom surfaces` metadata stamp.
///
/// `main` bumps `VERSION` on nearly every merge and lands ~15 resync commits a
/// day, so without this the base-move diff is never empty and the input-scoped
/// predicate degenerates back to "any move is stale". Both discounts are
/// deliberately narrow, because a version file is also where a real dependency
/// bump lands:
///
/// **The version restamp** (#8919):
///
/// - `VERSION` itself must be in the diff, and must **strictly increase**
///   (a decrease is not a restamp; it is somebody rewriting history).
/// - Only the paths in [`RESTAMP_PATHS`] are candidates.
/// - A candidate is discounted only if **every** changed line in its patch is a
///   version-value line: a removed line carrying `VERSION@B`, or an added line
///   carrying `VERSION@tip`.
/// - A missing patch, a status other than `modified`, or one stray line makes
///   the file a real change. So a `Cargo.lock` dependency bump — whose changed
///   lines carry some *other* package's version — is never mistaken for one.
///
/// **The resync restamp** (#9065): [`INSTALL_METADATA`] may additionally carry
/// [`is_resync_field_line`] lines — the `loom_commit` / `last_resync` /
/// `loom_source_remote` fields `resync-installed.sh`'s `restamp_metadata()`
/// rewrites — and, unlike every other candidate, needs **no** `VERSION` pair to
/// be discounted, because a resync commit does not bump the version. Every
/// resync commit on `main` in the 24 h before #9065 was measured touched that
/// file and nothing else, so this is what makes them stop staling open PRs.
/// Each field is pinned to its own *shape* (hex sha, ISO date, git remote URL),
/// so an arbitrary string edit to one of those keys is still a real change, and
/// a changed `loom_version` line still needs the validated pair. None of the
/// three is an input to any spec in [`super::inputs`] — the only required gate
/// that reads the file at all is the conflict-marker scan over `**`, and no
/// conflict marker has any of those shapes.
///
/// **Input-scoping is preserved.** The discount is per FILE, never per commit:
/// a resync that also rewrote an installed surface (`.loom/scripts/*.sh`,
/// `.loom/docs/*.md`, …) leaves those paths in `D`, where the ordinary clauses
/// judge them exactly as before.
///
/// Returns the surviving files (`D` proper). On any failure to establish a
/// discount, NOTHING is discounted: the fail-closed direction.
#[must_use]
pub fn strip_validated_restamps(files: &[ChangedFile]) -> Vec<ChangedFile> {
    let pair = version_pair(files);
    files
        .iter()
        .filter(|f| !is_discountable(f, pair.as_ref()))
        .cloned()
        .collect()
}

/// Is this file a pure machine restamp — of the version, of the resync
/// metadata, or of both at once?
fn is_discountable(f: &ChangedFile, pair: Option<&(String, String)>) -> bool {
    if f.path == INSTALL_METADATA {
        return is_pure_metadata_restamp(f, pair);
    }
    match pair {
        Some((old, new)) => is_pure_restamp(f, old, new),
        // Every other candidate exists only because the release bump rewrites
        // it; with no validated pair there is nothing to recognise it by.
        None => false,
    }
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

/// The `-`/`+` lines of a candidate's patch, or `None` when there is no patch
/// to read, the change is not an in-place edit, or nothing changed.
fn restamp_lines(f: &ChangedFile) -> Option<(Vec<String>, Vec<String>)> {
    if f.status != "modified" {
        return None;
    }
    // A suppressed patch is an unknown, and unknowns are real changes.
    let (removed, added) = changed_lines(f.patch.as_deref()?);
    (!removed.is_empty() || !added.is_empty()).then_some((removed, added))
}

/// Is every changed line of this file's patch a version-value line for the
/// validated `old` → `new` pair?
fn is_pure_restamp(f: &ChangedFile, old: &str, new: &str) -> bool {
    if !RESTAMP_PATHS.contains(&f.path.as_str()) {
        return false;
    }
    let Some((removed, added)) = restamp_lines(f) else {
        return false;
    };
    removed.iter().all(|l| l.contains(old)) && added.iter().all(|l| l.contains(new))
}

/// [`INSTALL_METADATA`]'s own rule (#9065): every changed line must be either a
/// resync field line — which needs no version pair, because a resync commit
/// does not bump the version — or, when a validated pair exists, a value line
/// for it.
fn is_pure_metadata_restamp(f: &ChangedFile, pair: Option<&(String, String)>) -> bool {
    let Some((removed, added)) = restamp_lines(f) else {
        return false;
    };
    let (old, new) = match pair {
        Some((o, n)) => (o.as_str(), n.as_str()),
        None => ("", ""),
    };
    let version_line = |l: &String, v: &str| !v.is_empty() && l.contains(v);
    removed
        .iter()
        .all(|l| is_resync_field_line(l) || version_line(l, old))
        && added
            .iter()
            .all(|l| is_resync_field_line(l) || version_line(l, new))
}

/// A line rewriting one of the install-metadata fields
/// `resync-installed.sh`'s `restamp_metadata()` writes on every run —
/// `loom_commit`, `last_resync`, `loom_source_remote` — and the only
/// non-version lines a restamp may carry. Each is pinned to its VALUE SHAPE,
/// so an arbitrary string edit to one of those keys is still a real change.
fn is_resync_field_line(line: &str) -> bool {
    is_loom_commit_line(line) || is_last_resync_line(line) || is_source_remote_line(line)
}

/// `"loom_commit": "<hex>"`.
fn is_loom_commit_line(line: &str) -> bool {
    let Some(value) = json_field_value(line, "loom_commit") else {
        return false;
    };
    value.len() >= 7 && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// `"last_resync": "YYYY-MM-DD"` — the date `resync-installed.sh` stamps on
/// every run.
fn is_last_resync_line(line: &str) -> bool {
    let Some(value) = json_field_value(line, "last_resync") else {
        return false;
    };
    let parts: Vec<&str> = value.split('-').collect();
    parts.len() == 3
        && parts[0].len() == 4
        && parts[1].len() == 2
        && parts[2].len() == 2
        && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()))
}

/// `"loom_source_remote": "<git remote URL>"` — re-read from whichever clone
/// ran the resync, so it flips between the SSH and HTTPS spellings of the same
/// repository as the fleet's hosts take turns. Pinned to a git remote URL
/// shape; nothing reads this field as a check input.
fn is_source_remote_line(line: &str) -> bool {
    let Some(value) = json_field_value(line, "loom_source_remote") else {
        return false;
    };
    (value.starts_with("https://") || value.starts_with("http://") || value.starts_with("git@"))
        && value.ends_with(".git")
}

/// The unquoted value of `"<key>": …` on a JSON line, or `None` when the line
/// does not carry that key.
fn json_field_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.split(&format!("\"{key}\"")).nth(1)?;
    let (_, value) = rest.split_once(':')?;
    Some(value.trim().trim_end_matches(',').trim_matches('"'))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod restamp_e2e_tests;
