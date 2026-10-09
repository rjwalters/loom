//! PR size and scope predictors from the logged file lists (#10960): the
//! diff stat, the file count, path-category flags, the critical-file flag
//! the Champion's merge hold keys on, and the churn of recently merged PRs.
//! **One** definition for training and serving, the model of
//! [`super::loop_features`].
//!
//! # Not enabled for any shipped heuristic
//!
//! [`SCOPE_FEATURES`] are candidate inputs of the next datestamped shadow
//! (the size/scope block of a future `eta-fit/v4`, shared with the capacity
//! block of #10959). No fit schema lists them, and no coefficient file,
//! explanation or fixture changes. They are recorded: the fit assembles them
//! per row (`fit::rows::Assembled::scope`) and serving records them on
//! `eta.estimate` (`features.scope`), so the walk-forward can be run once the
//! log covers a full window.
//!
//! # Two call sites, one builder
//!
//! - **Fit**: [`super::fit::rows`] records one [`ScopeFeatures`] per row at
//!   `t - lag`, over the fleet snapshots' episodes and the file log.
//! - **Serving**: `Tracker::scope_features_of` reads the same timeline and
//!   file log at `now - lag`.
//!
//! # The predictors
//!
//! | feature | definition |
//! |---|---|
//! | `log_lines`, `lines_known` | `ln(1 + additions + deletions)` of the subject's latest whole list before `as_of` |
//! | `log_files` | `ln(1 + files)`; for a huge PR, `ln(1 + listed)` (a lower bound, never "small") |
//! | `huge` | the latest read listed [`MAX_LISTED_FILES`] or more entries; known even though the paths are not |
//! | `scope_known` | the subject's latest observation is a whole list: every path flag below is then known |
//! | `docs_only` | every path is documentation ([`is_docs`]) |
//! | `tests_only` | every path is a test ([`is_test`]) |
//! | `touches_rust`, `touches_ts`, `touches_shell` | some path ends `.rs`; `.ts` / `.tsx`; `.sh` |
//! | `touches_critical` | some path contains a [`CRITICAL_PATTERNS`] entry (the Champion's critical-file hold) |
//! | `log_churn_7d`, `churn_known` | distinct subject paths touched by a PR of the repo that **merged** in `[as_of - 7 d, as_of)`, from those PRs' lists as known before `as_of` |
//!
//! Path categories (fixed here, matched on the path as listed):
//!
//! | category | rule |
//! |---|---|
//! | docs | ends `.md` or `.txt`, or under a top-level `docs/` |
//! | test | has a `tests` directory component, or the file name ends `_tests.rs` / `.test.ts`, or starts `test-` and ends `.sh` |
//!
//! An empty list (a PR with no file changes) is known, with every flag
//! false: `docs_only` and `tests_only` need at least one path.
//!
//! # Knowability
//!
//! The subject's observation is its latest [`FileSnapshot`] strictly before
//! `as_of`. A later incomplete read makes the paths unknown from then on,
//! never an older complete list served as current, and never "small": an
//! incomplete read that listed a full page is `huge`. The churn reads a
//! merge only once it happened before `as_of`, and only lists known before
//! `as_of`; a merged PR in the window whose list is not known then makes the
//! churn unknown, not lower. Every missing value is 0 in the vector beside a
//! `*_known` indicator of 0.
//!
//! Pure: reads its arguments and nothing else.

use super::episodes::{EpisodeEnd, EpisodeNext, StageEpisode};
use super::loop_features::FileSnapshot;
use super::pr_file_log::MAX_LISTED_FILES;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// The candidate feature names, in [`scope_vector`] order. None is in a
/// shipped fit schema (pinned by a test).
pub const SCOPE_FEATURES: [&str; 13] = [
    "log_lines",
    "lines_known",
    "log_files",
    "huge",
    "scope_known",
    "docs_only",
    "tests_only",
    "touches_rust",
    "touches_ts",
    "touches_shell",
    "touches_critical",
    "log_churn_7d",
    "churn_known",
];

/// `SCOPE_FEATURES.len()`.
pub const N_SCOPE_FEATURES: usize = SCOPE_FEATURES.len();

/// The trailing window of the merged-PR churn, in days.
pub const CHURN_WINDOW_DAYS: i64 = 7;

/// The Champion's critical-file patterns, matched as substrings of a path:
/// the `CRITICAL_PATTERNS` array of `champion-pr-merge.md` §3 (Critical File
/// Exclusion Check). A test parses the prompt's array and asserts equality,
/// so the two cannot drift. The prompt's version-only carve-out is not
/// modelled: a version bump lands post-merge, never in a PR.
pub const CRITICAL_PATTERNS: [&str; 7] = [
    "Cargo.toml",
    "loom-daemon/Cargo.toml",
    "loom-api/Cargo.toml",
    "package.json",
    ".github/workflows/",
    "migrations/",
    "_migration.py",
];

/// What the builder reads.
#[derive(Debug, Clone, Copy)]
pub struct ScopeInputs<'a> {
    /// `owner/repo` of the subject.
    pub repo: &'a str,
    /// The subject PR.
    pub pr: u32,
    /// The repo's episodes; only merges in the churn window are read, so a
    /// caller may pass [`churn_context`] instead of the whole history.
    pub repo_episodes: &'a [&'a StageEpisode],
    /// File snapshots of the subject and the repo's other PRs; `None` when
    /// the log is not loaded (every feature unknown).
    pub files: Option<&'a [FileSnapshot]>,
}

/// The path-category flags of one whole list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct PathFlags {
    /// Every path is documentation.
    pub docs_only: bool,
    /// Every path is a test.
    pub tests_only: bool,
    /// Some path is Rust.
    pub touches_rust: bool,
    /// Some path is TypeScript.
    pub touches_ts: bool,
    /// Some path is shell.
    pub touches_shell: bool,
    /// Some path matches a [`CRITICAL_PATTERNS`] entry.
    pub touches_critical: bool,
}

/// The size and scope predictors of one PR at one instant.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScopeFeatures {
    /// Files in the subject's whole list; `None` unless its latest
    /// observation is whole.
    pub files: Option<u32>,
    /// `additions + deletions`; `None` unless the latest observation is a
    /// whole list that carried the stat.
    pub lines: Option<u32>,
    /// Whether the latest read listed a full page or more; `None` when there
    /// is no observation, or an incomplete one (an older line) without the
    /// count. A whole list is always `Some(false)`.
    pub huge: Option<bool>,
    /// Entries the latest read listed, whole list or not.
    pub listed: Option<u32>,
    /// The path flags; `None` unless the latest observation is whole.
    pub paths: Option<PathFlags>,
    /// Distinct subject paths touched by a PR merged in the churn window;
    /// `None` when the subject's paths or any such PR's list is unknown.
    pub churn_7d: Option<u32>,
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The file name: the last `/` component.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Whether `path` is documentation: `*.md`, `*.txt` or under `docs/`.
#[must_use]
pub fn is_docs(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".txt") || lower.starts_with("docs/")
}

/// Whether `path` is a test: a `tests` directory component, or a file named
/// `*_tests.rs`, `*.test.ts` or `test-*.sh`.
#[must_use]
pub fn is_test(path: &str) -> bool {
    let name = file_name(path);
    let in_tests_dir = path.split('/').rev().skip(1).any(|dir| dir == "tests");
    in_tests_dir
        || name.ends_with("_tests.rs")
        || name.ends_with(".test.ts")
        || (name.starts_with("test-") && name.ends_with(".sh"))
}

/// Whether `path` contains a [`CRITICAL_PATTERNS`] entry.
#[must_use]
pub fn is_critical(path: &str) -> bool {
    CRITICAL_PATTERNS.iter().any(|p| path.contains(p))
}

/// The flags of a whole list.
#[must_use]
pub fn path_flags(paths: &[String]) -> PathFlags {
    let any = |f: fn(&str) -> bool| paths.iter().any(|p| f(p));
    let all = |f: fn(&str) -> bool| !paths.is_empty() && paths.iter().all(|p| f(p));
    PathFlags {
        docs_only: all(is_docs),
        tests_only: all(is_test),
        touches_rust: any(|p| p.ends_with(".rs")),
        touches_ts: any(|p| p.ends_with(".ts") || p.ends_with(".tsx")),
        touches_shell: any(|p| p.ends_with(".sh")),
        touches_critical: any(is_critical),
    }
}

/// The latest snapshot of `pr` known strictly before `as_of`, whole or not.
fn latest<'a>(
    files: &'a [FileSnapshot],
    repo: &str,
    pr: u32,
    as_of: DateTime<Utc>,
) -> Option<&'a FileSnapshot> {
    files
        .iter()
        .filter(|s| s.pr == pr && s.repo.eq_ignore_ascii_case(repo) && s.known_at < as_of)
        .max_by_key(|s| s.known_at)
}

/// The instant `e` merged, when it ended in a merge.
fn merged_at(e: &StageEpisode) -> Option<DateTime<Utc>> {
    match e.end {
        EpisodeEnd::Left {
            at,
            next: EpisodeNext::Merged,
        } => Some(at),
        _ => None,
    }
}

/// Whether `e` is a merge in the churn window `[as_of - 7 d, as_of)`.
#[must_use]
pub fn merged_in_window(e: &StageEpisode, as_of: DateTime<Utc>) -> bool {
    let from = as_of - Duration::days(CHURN_WINDOW_DAYS);
    merged_at(e).is_some_and(|at| at >= from && at < as_of)
}

/// The subset of `episodes` [`merged_in_window`] at `as_of`: what a caller
/// evaluating many PRs at one instant computes once per repo.
#[must_use]
pub fn churn_context<'a>(
    episodes: &[&'a StageEpisode],
    as_of: DateTime<Utc>,
) -> Vec<&'a StageEpisode> {
    episodes
        .iter()
        .copied()
        .filter(|e| merged_in_window(e, as_of))
        .collect()
}

/// The size and scope predictors of the subject at `as_of`.
#[must_use]
pub fn scope_features(inputs: &ScopeInputs<'_>, as_of: DateTime<Utc>) -> ScopeFeatures {
    let mut out = ScopeFeatures::default();
    let Some(files) = inputs.files else {
        return out;
    };
    let Some(snap) = latest(files, inputs.repo, inputs.pr, as_of) else {
        return out;
    };
    out.listed = snap.listed;
    out.huge = snap.listed.map(|n| n as usize >= MAX_LISTED_FILES);
    if !snap.complete {
        return out;
    }
    // A whole list is shorter than a page by construction (`parse_page`),
    // so even an older line without `listed` is known not huge.
    out.huge = Some(false);
    out.files = Some(count(snap.files.len()));
    out.lines = snap
        .additions
        .zip(snap.deletions)
        .map(|(a, d)| a.saturating_add(d));
    out.paths = Some(path_flags(&snap.files));

    let subject: BTreeSet<&str> = snap.files.iter().map(String::as_str).collect();
    let merged: BTreeSet<u32> = inputs
        .repo_episodes
        .iter()
        .filter(|e| {
            e.repo.eq_ignore_ascii_case(inputs.repo)
                && e.pr_number != inputs.pr
                && merged_in_window(e, as_of)
        })
        .map(|e| e.pr_number)
        .collect();
    let mut touched: BTreeSet<&str> = BTreeSet::new();
    for pr in merged {
        // An unknown list of a merged PR leaves the churn unknown, not lower.
        let Some(peer) = latest(files, inputs.repo, pr, as_of).filter(|s| s.complete) else {
            return out;
        };
        touched.extend(
            peer.files
                .iter()
                .map(String::as_str)
                .filter(|f| subject.contains(f)),
        );
    }
    out.churn_7d = Some(count(touched.len()));
    out
}

/// How many of a set of rows know each scope input: the coverage a fit or
/// backtest reports next to its numbers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeCoverage {
    /// Rows counted.
    pub rows: usize,
    /// Rows whose subject's paths are known.
    pub scope_known: usize,
    /// Rows with a known diff stat.
    pub lines_known: usize,
    /// Rows known to be huge.
    pub huge: usize,
    /// Rows with a known churn.
    pub churn_known: usize,
}

impl ScopeCoverage {
    /// The coverage of `scope`.
    #[must_use]
    pub fn of(scope: &[ScopeFeatures]) -> Self {
        let n = |f: fn(&ScopeFeatures) -> bool| scope.iter().filter(|s| f(s)).count();
        ScopeCoverage {
            rows: scope.len(),
            scope_known: n(|s| s.paths.is_some()),
            lines_known: n(|s| s.lines.is_some()),
            huge: n(|s| s.huge == Some(true)),
            churn_known: n(|s| s.churn_7d.is_some()),
        }
    }
}

/// The model-ready vector in [`SCOPE_FEATURES`] order: counts as
/// `ln(1 + x)`, flags 0/1, and a `*_known` indicator beside every feature
/// that can be missing (a missing value is 0 with its indicator 0).
#[must_use]
pub fn scope_vector(f: &ScopeFeatures) -> [f64; N_SCOPE_FEATURES] {
    let flag = |b: bool| if b { 1.0 } else { 0.0 };
    let huge = f.huge == Some(true);
    let files = f.files.or(if huge { f.listed } else { None });
    let p = f.paths.unwrap_or_default();
    [
        f64::from(f.lines.unwrap_or(0)).ln_1p(),
        flag(f.lines.is_some()),
        f64::from(files.unwrap_or(0)).ln_1p(),
        flag(huge),
        flag(f.paths.is_some()),
        flag(p.docs_only),
        flag(p.tests_only),
        flag(p.touches_rust),
        flag(p.touches_ts),
        flag(p.touches_shell),
        flag(p.touches_critical),
        f64::from(f.churn_7d.unwrap_or(0)).ln_1p(),
        flag(f.churn_7d.is_some()),
    ]
}
