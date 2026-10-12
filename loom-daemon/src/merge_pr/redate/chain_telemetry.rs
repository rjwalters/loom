//! Per-PR re-date counts and time-to-land (#10163).
//!
//! 2026-10-04: a Judge-approved chain head was re-dated three times in about
//! 40 minutes without landing, while `main` kept moving under it. Nothing
//! surfaced that: the per-check table in [`super::report`] says *why* re-dates
//! happen, not *which PR* is burning them or how long a PR waited. This module
//! is the per-PR view: how many re-dates each PR took, how long from its first
//! re-date to the merge commit that landed it, and whether it has landed at
//! all. A PR with many re-dates and no landing is the livelock signature.
//!
//! Like [`super::report`], read-only and derived from local `git log` alone:
//! nothing is fetched or posted. Unlike that report, `main` alone is NOT a
//! sufficient source here — a re-date commit reaches `main` only when its PR
//! merges, so a PR that is still livelocked has every one of its re-dates on
//! its own branch. [`timed_log_args`] therefore also reads every local
//! remote-tracking ref (`--remotes`): an unlanded PR's re-dates are seen as
//! far as the last `git fetch` saw them, and a landed one's are deduplicated
//! by `git log` itself. A remote branch whose PR was closed unmerged still
//! reads as unlanded until it is pruned; the window bounds that.
//!
//! The parsing and aggregation are pure so the 3-re-date case is unit-tested
//! without git. Two consumers: `loom-daemon merge-pr redate-report` (the
//! per-PR rows, `chains` in its JSON) and the daemon's
//! `observability::ops::redate_chain` emitter, which exports the aggregate as
//! `metric.points` gauges on the `host.health` cadence.

use super::is_redate_commit_subject;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::BTreeMap;

/// Separates the fields of one `git log` record.
const FIELD_SEP: char = '\u{1f}';
/// Terminates one `git log` record.
const RECORD_SEP: char = '\u{1e}';
const SUBJECT_PREFIX: &str = "chore: re-date required checks for PR #";

/// A PR with at least this many re-dates is flagged (`stuck` when also
/// unlanded). Matches the default re-date budget: a chain that has spent it
/// is the one that escalates to an operator hold.
pub const LIVELOCK_REDATES: usize = super::budget::DEFAULT_BUDGET as usize;

/// One re-date commit with its committer time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedRedate {
    pub sha: String,
    pub pr: String,
    pub at: DateTime<Utc>,
}

/// Most PRs whose landing is looked up per [`collect`] call: one local
/// `git log` each. PRs past the cap (fewest re-dates first) keep their counts
/// and read as not landed.
pub const MAX_LANDING_LOOKUPS: usize = 64;

/// The `git log` arguments (after `git`) that list re-date commits with
/// committer times, from `git_ref` and every remote-tracking ref (see the
/// module docs for why `git_ref` alone would hide a live livelock).
#[must_use]
pub fn timed_log_args(git_ref: &str, since: DateTime<Utc>) -> Vec<String> {
    vec![
        "log".to_string(),
        git_ref.to_string(),
        "--remotes".to_string(),
        format!("--since={}", since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        "--grep=^chore: re-date required checks for PR #".to_string(),
        "--format=%H%x1f%s%x1f%cI%x1e".to_string(),
    ]
}

/// The `git log` arguments that list the merge commits through which `sha`
/// reached `git_ref`, oldest first: the first line is the merge that landed it.
#[must_use]
pub fn landing_log_args(sha: &str, git_ref: &str) -> Vec<String> {
    vec![
        "log".to_string(),
        format!("{sha}..{git_ref}"),
        "--ancestry-path".to_string(),
        "--merges".to_string(),
        "--reverse".to_string(),
        "--format=%cI".to_string(),
    ]
}

/// Parse [`timed_log_args`] output. Records whose subject is not exactly the
/// automated re-date subject, or whose time does not parse, are dropped.
#[must_use]
pub fn parse_timed_log(raw: &str) -> Vec<TimedRedate> {
    raw.split(RECORD_SEP)
        .filter_map(|record| {
            let mut fields = record.trim_start_matches(['\n', '\r']).splitn(3, FIELD_SEP);
            let sha = fields.next()?.trim().to_string();
            let subject = fields.next()?.trim_end();
            let at = DateTime::parse_from_rfc3339(fields.next()?.trim())
                .ok()?
                .with_timezone(&Utc);
            if sha.is_empty() || !is_redate_commit_subject(subject) {
                return None;
            }
            let pr = subject
                .strip_prefix(SUBJECT_PREFIX)?
                .split(' ')
                .next()?
                .to_string();
            Some(TimedRedate { sha, pr, at })
        })
        .collect()
}

/// One PR's re-date history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChainStat {
    pub pr: String,
    /// Re-date commits pushed for this PR in the window.
    pub redates: usize,
    pub first_redate_at: String,
    pub last_redate_at: String,
    /// Newest re-date commit, the one a landing merge is looked up from.
    pub last_sha: String,
    /// When the merge that landed the PR was committed; `None` = not landed.
    pub landed_at: Option<String>,
    /// First re-date to landing, seconds; `None` = not landed.
    pub time_to_land_secs: Option<i64>,
    /// `redates >= LIVELOCK_REDATES` and not landed.
    pub stuck: bool,
}

/// Group `commits` per PR, oldest first within a PR. `landed` resolves a PR's
/// newest re-date sha to its landing time. Output is most re-dates first, ties
/// by PR number, so it is a function of the inputs alone.
#[must_use]
pub fn chain_stats(
    commits: &[TimedRedate],
    landed: impl Fn(&str) -> Option<DateTime<Utc>>,
) -> Vec<ChainStat> {
    let mut by_pr: BTreeMap<&str, Vec<&TimedRedate>> = BTreeMap::new();
    for c in commits {
        by_pr.entry(&c.pr).or_default().push(c);
    }
    let fmt = |t: DateTime<Utc>| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut stats: Vec<ChainStat> = by_pr
        .into_iter()
        .map(|(pr, mut cs)| {
            cs.sort_by_key(|c| c.at);
            let (first, last) = (cs[0], cs[cs.len() - 1]);
            let landed_at = landed(&last.sha);
            ChainStat {
                pr: pr.to_string(),
                redates: cs.len(),
                first_redate_at: fmt(first.at),
                last_redate_at: fmt(last.at),
                last_sha: last.sha.clone(),
                landed_at: landed_at.map(fmt),
                time_to_land_secs: landed_at.map(|t| (t - first.at).num_seconds().max(0)),
                stuck: landed_at.is_none() && cs.len() >= LIVELOCK_REDATES,
            }
        })
        .collect();
    stats.sort_by(|a, b| {
        b.redates.cmp(&a.redates).then_with(|| {
            a.pr.parse::<u64>()
                .unwrap_or(u64::MAX)
                .cmp(&b.pr.parse::<u64>().unwrap_or(u64::MAX))
        })
    });
    stats
}

/// Run [`timed_log_args`] in `dir`.
///
/// # Errors
/// The reason `git log` could not produce output.
pub fn run_timed_log(
    dir: &std::path::Path,
    git_ref: &str,
    since: DateTime<Utc>,
) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(timed_log_args(git_ref, since))
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

/// Resolve the landing time of `sha` on `git_ref` in `dir` (first line of
/// [`landing_log_args`]); `None` when it has not landed or git failed.
#[must_use]
pub fn landing_time(dir: &std::path::Path, sha: &str, git_ref: &str) -> Option<DateTime<Utc>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(landing_log_args(sha, git_ref))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().next()?.trim();
    DateTime::parse_from_rfc3339(first)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Per-PR stats for the re-dates visible in `dir` since `since`, landings
/// resolved against `git_ref` (at most [`MAX_LANDING_LOOKUPS`] lookups).
///
/// # Errors
/// The reason the re-date `git log` could not run.
pub fn collect(
    dir: &std::path::Path,
    git_ref: &str,
    since: DateTime<Utc>,
) -> Result<Vec<ChainStat>, String> {
    let commits = parse_timed_log(&run_timed_log(dir, git_ref, since)?);
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for c in &commits {
        *counts.entry(c.pr.as_str()).or_default() += 1;
    }
    // Look up the most re-dated PRs first: they are the livelock candidates.
    let mut ranked: Vec<(&str, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let allowed: std::collections::BTreeSet<&str> = ranked
        .iter()
        .take(MAX_LANDING_LOOKUPS)
        .map(|(pr, _)| *pr)
        .collect();
    let newest: BTreeMap<&str, &TimedRedate> = commits.iter().fold(BTreeMap::new(), |mut m, c| {
        let slot = m.entry(c.pr.as_str()).or_insert(c);
        if c.at > slot.at {
            *slot = c;
        }
        m
    });
    let lookup: std::collections::BTreeSet<&str> = newest
        .iter()
        .filter(|(pr, _)| allowed.contains(*pr))
        .map(|(_, c)| c.sha.as_str())
        .collect();
    Ok(chain_stats(&commits, |sha| {
        if lookup.contains(sha) {
            landing_time(dir, sha, git_ref)
        } else {
            None
        }
    }))
}

/// Human rendering, appended to the report text.
#[must_use]
pub fn render_text(stats: &[ChainStat]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if stats.is_empty() {
        return out;
    }
    let _ = writeln!(out, "\nPer PR (re-dates, time from first re-date to landing):");
    for s in stats {
        let state = match s.time_to_land_secs {
            Some(secs) => format!("landed after {}m", secs / 60),
            None if s.stuck => "NOT LANDED (livelock suspected)".to_string(),
            None => "not landed".to_string(),
        };
        let _ = writeln!(out, "  {:>4}  #{}  {state}", s.redates, s.pr);
    }
    out
}

#[cfg(test)]
#[path = "chain_telemetry_tests.rs"]
mod tests;
