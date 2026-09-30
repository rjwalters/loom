//! `loom-daemon merge-pr redate-report` (#9746): which checks and paths force
//! the #8508 re-dates, read back from the re-date commits' trailers.
//!
//! Read-only by construction: the only external process is one `git log`
//! against a ref already present locally. Nothing is fetched, written, or
//! posted — run `git fetch` first if the ref may be behind.
//!
//! # What it counts
//!
//! Every commit reachable from `--ref` (default `origin/main`) since `--since`
//! whose subject is exactly the re-date subject
//! ([`super::is_redate_commit_subject`]). Each is either **attributed** (it
//! carries a `Stale-Check:` trailer, see [`super::attribution`]) and counted in
//! the check x clause x base path x PR path table, or **untrailered** (written
//! before #9746, or by a daemon of that vintage, or with a verdict that could
//! not be recomputed at re-date time).
//!
//! # Why `main` is a sufficient source
//!
//! Re-date commits are pushed onto PR branches, so they reach `main` only if
//! the PR merges with its commits intact. This repo merges with merge commits,
//! so they do (verified 2026-09-30). A repo that SQUASH-merges drops them, and
//! the report then undercounts — only re-dates on PRs that never merged, or
//! that squashed, are invisible. The PR's `loom:stale-check-redate` comment is
//! still the budget's durable record either way.

use super::attribution::{NONE, TRAILER_BASE_PATH, TRAILER_CHECK, TRAILER_CLAUSE, TRAILER_PR_PATH};
use super::is_redate_commit_subject;
use serde::Serialize;
use std::collections::BTreeMap;

/// Separates the fields of one `git log` record.
const FIELD_SEP: char = '\u{1f}';
/// Terminates one `git log` record.
const RECORD_SEP: char = '\u{1e}';

/// The `--grep` pattern that pre-filters candidates; [`parse_log`] then keeps
/// only exact subject matches.
const SUBJECT_GREP: &str = "^chore: re-date required checks for PR #";
/// [`SUBJECT_GREP`] without its anchor.
const SUBJECT_PREFIX: &str = "chore: re-date required checks for PR #";

/// The `git log` arguments (after `git`) this report runs.
#[must_use]
pub fn git_log_args(git_ref: &str, since: chrono::DateTime<chrono::Utc>) -> Vec<String> {
    vec![
        "log".to_string(),
        git_ref.to_string(),
        format!("--since={}", since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        format!("--grep={SUBJECT_GREP}"),
        "--format=%H%x1f%s%x1f%(trailers:only,unfold)%x1e".to_string(),
    ]
}

/// Run [`git_log_args`] in `dir`.
///
/// # Errors
/// The reason `git log` could not produce output (not a repo, unknown ref, …).
pub fn run_git_log(
    dir: &std::path::Path,
    git_ref: &str,
    since: chrono::DateTime<chrono::Utc>,
) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(git_log_args(git_ref, since))
        .output()
        .map_err(|e| format!("could not exec git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git log {git_ref} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// How many records matched the `--grep` pre-filter but are NOT the automated
/// remedy's exact subject — hand-made re-dates such as
/// `… (#8248 guard, operator release)`. Reported separately so the table's
/// denominator is never silently short of what `git log --grep` shows.
#[must_use]
pub fn count_other_redates(raw: &str) -> usize {
    raw.split(RECORD_SEP)
        .filter_map(|record| {
            let mut fields = record.trim_start_matches(['\n', '\r']).splitn(3, FIELD_SEP);
            let sha = fields.next()?.trim();
            let subject = fields.next()?.trim_end();
            // `--grep` matches ANY line, so a body merely quoting the subject
            // must not count: the subject itself has to carry the prefix.
            (!sha.is_empty()
                && subject.starts_with(SUBJECT_PREFIX)
                && !is_redate_commit_subject(subject))
            .then_some(())
        })
        .count()
}

/// One re-date commit as read from the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedateCommit {
    pub sha: String,
    /// The PR number the subject names.
    pub pr: String,
    /// `None` when the commit carries no `Stale-Check:` trailer.
    pub attribution: Option<Key>,
}

/// One attribution row's identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Key {
    pub check: String,
    pub clause: String,
    pub base_path: String,
    pub pr_path: String,
}

/// Parse `git log` output in [`git_log_args`]' format. Records whose subject
/// is not exactly the re-date subject are dropped (the `--grep` is only a
/// pre-filter). For a repeated trailer key the first value wins.
#[must_use]
pub fn parse_log(raw: &str) -> Vec<RedateCommit> {
    raw.split(RECORD_SEP)
        .filter_map(|record| {
            let record = record.trim_start_matches(['\n', '\r']);
            let mut fields = record.splitn(3, FIELD_SEP);
            let sha = fields.next()?.trim().to_string();
            let subject = fields.next()?.trim_end();
            let trailers = fields.next().unwrap_or_default();
            if sha.is_empty() || !is_redate_commit_subject(subject) {
                return None;
            }
            let pr = subject
                .strip_prefix(SUBJECT_PREFIX)
                .and_then(|r| r.split(' ').next())
                .unwrap_or_default()
                .to_string();
            let mut map: BTreeMap<&str, &str> = BTreeMap::new();
            for line in trailers.lines() {
                if let Some((k, v)) = line.split_once(':') {
                    map.entry(k.trim()).or_insert(v.trim());
                }
            }
            let get = |k: &str| {
                map.get(k)
                    .map_or(NONE, |v| if v.is_empty() { NONE } else { v })
            };
            let attribution = map.get(TRAILER_CHECK).map(|check| Key {
                check: (*check).to_string(),
                clause: get(TRAILER_CLAUSE).to_string(),
                base_path: get(TRAILER_BASE_PATH).to_string(),
                pr_path: get(TRAILER_PR_PATH).to_string(),
            });
            Some(RedateCommit {
                sha,
                pr,
                attribution,
            })
        })
        .collect()
}

/// One row of the attribution table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Row {
    #[serde(flatten)]
    pub key: Key,
    pub count: usize,
    /// The distinct PRs this row's re-dates were for, ascending.
    pub prs: Vec<String>,
}

/// A `(name, count)` subtotal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Subtotal {
    pub name: String,
    pub count: usize,
}

/// The whole report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub git_ref: String,
    pub since: String,
    /// Re-date commits found.
    pub total: usize,
    /// ...of which carry attribution trailers.
    pub attributed: usize,
    /// ...of which carry none (pre-#9746, or unrecomputable verdict).
    pub untrailered: usize,
    /// Re-date-like commits NOT written by the automated remedy (not in
    /// `total`), see [`count_other_redates`].
    pub other_redate_subjects: usize,
    /// check x clause x base path x PR path, most frequent first.
    pub rows: Vec<Row>,
    /// Per check, most frequent first.
    pub by_check: Vec<Subtotal>,
    /// Per clause, most frequent first.
    pub by_clause: Vec<Subtotal>,
}

/// Aggregate parsed commits into a [`Report`]. Ties sort by name, so the
/// output is a function of the commits alone.
#[must_use]
pub fn aggregate(
    commits: &[RedateCommit],
    other_redate_subjects: usize,
    git_ref: &str,
    since: &str,
) -> Report {
    let mut rows: BTreeMap<Key, (usize, Vec<String>)> = BTreeMap::new();
    let mut by_check: BTreeMap<String, usize> = BTreeMap::new();
    let mut by_clause: BTreeMap<String, usize> = BTreeMap::new();
    let mut untrailered = 0;
    for c in commits {
        let Some(key) = &c.attribution else {
            untrailered += 1;
            continue;
        };
        *by_check.entry(key.check.clone()).or_default() += 1;
        *by_clause.entry(key.clause.clone()).or_default() += 1;
        let entry = rows.entry(key.clone()).or_default();
        entry.0 += 1;
        if !entry.1.contains(&c.pr) {
            entry.1.push(c.pr.clone());
        }
    }
    let mut rows: Vec<Row> = rows
        .into_iter()
        .map(|(key, (count, mut prs))| {
            prs.sort_by_key(|p| p.parse::<u64>().unwrap_or(u64::MAX));
            Row { key, count, prs }
        })
        .collect();
    rows.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.key.cmp(&b.key)));
    Report {
        git_ref: git_ref.to_string(),
        since: since.to_string(),
        total: commits.len(),
        attributed: commits.len() - untrailered,
        untrailered,
        other_redate_subjects,
        rows,
        by_check: subtotals(by_check),
        by_clause: subtotals(by_clause),
    }
}

fn subtotals(m: BTreeMap<String, usize>) -> Vec<Subtotal> {
    let mut v: Vec<Subtotal> = m
        .into_iter()
        .map(|(name, count)| Subtotal { name, count })
        .collect();
    v.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    v
}

/// The human-readable rendering.
#[must_use]
pub fn render_text(r: &Report) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Re-date commits on {} since {}: {} ({} attributed, {} untrailered)",
        r.git_ref, r.since, r.total, r.attributed, r.untrailered
    );
    if r.other_redate_subjects > 0 {
        let _ = writeln!(
            out,
            "(plus {} hand-made re-date commit(s) with a non-automated subject, not counted)",
            r.other_redate_subjects
        );
    }
    if r.rows.is_empty() {
        let _ = writeln!(
            out,
            "\nNo attributed re-dates in this window. Untrailered commits predate #9746's \
trailers (or their verdict could not be recomputed at re-date time)."
        );
        return out;
    }
    let _ = writeln!(out, "\nBy check x base path x PR path (most frequent first):");
    for row in &r.rows {
        let _ = writeln!(
            out,
            "  {:>4}  {}\n        base: {}  |  pr: {}\n        clause: {}\n        PRs: {}",
            row.count,
            row.key.check,
            row.key.base_path,
            row.key.pr_path,
            row.key.clause,
            row.prs
                .iter()
                .map(|p| format!("#{p}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
    let _ = writeln!(out, "\nBy check:");
    for s in &r.by_check {
        let _ = writeln!(out, "  {:>4}  {}", s.count, s.name);
    }
    let _ = writeln!(out, "\nBy clause:");
    for s in &r.by_clause {
        let _ = writeln!(out, "  {:>4}  {}", s.count, s.name);
    }
    out
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;
