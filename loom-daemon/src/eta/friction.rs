//! Queue-friction features (#10193): the open-PR count and `pr-open-skip`
//! lockout of a repo, its typical recent CI duration, a PR's CI status and
//! whether it is behind its base or conflicted, and an explicit hold flag.
//!
//! Pure. The forge reads that feed it live in
//! `observability::eta_friction`; this module parses their answers into
//! [`Reading`]s, keeps the latest per repo and per PR in a [`FrictionBook`],
//! and copies them onto an estimate's [`Features`] at `as_of`.
//!
//! # Point in time (#10193 rules 1 and 4)
//!
//! Every reading carries the instant it was read. [`FrictionBook::apply`]
//! copies a reading only when it was read **at or before** `as_of` and is no
//! older than [`MAX_READING_AGE_SECS`]; anything else is listed in
//! `features_omitted` with the reason, never defaulted. The typical CI
//! duration ([`typical_ci_duration`]) uses only runs that had finished
//! before the instant it is computed for, so it is point-in-time by
//! construction.
//!
//! Features never move an estimate: no heuristic reads them.

use super::explanation::{FeatureOmitted, Features};
use super::labels::check_holds;
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use std::collections::BTreeMap;

/// A value, or why it could not be measured (the `features_omitted` reason).
pub type Reading<T> = Result<T, &'static str>;

/// Runs older than this do not count towards the typical CI duration.
pub const CI_WINDOW_DAYS: i64 = 7;

/// Fewest finished runs the typical CI duration needs.
pub const CI_MIN_RUNS: usize = 3;

/// A reading older than this at `as_of` is stale: omitted, not reported.
pub const MAX_READING_AGE_SECS: i64 = 3_600;

/// Rows per page of the open-PR listing: a full page is a lower bound only.
pub const OPEN_PR_PAGE: usize = 100;

/// Check-run conclusions that fail a head (also read by
/// [`super::fleet_state_prs`]).
pub const FAILING: [&str; 5] = [
    "failure",
    "timed_out",
    "cancelled",
    "action_required",
    "startup_failure",
];

/// A PR head's CI, from its check runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiStatus {
    /// Every check run finished, none failed.
    Passing,
    /// At least one check run failed (whatever else is still running).
    Failing,
    /// None failed, at least one still running.
    Pending,
    /// No check runs at all.
    None,
}

impl CiStatus {
    /// The feature value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CiStatus::Passing => "passing",
            CiStatus::Failing => "failing",
            CiStatus::Pending => "pending",
            CiStatus::None => "none",
        }
    }
}

/// One repo's friction, as last read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoFriction {
    /// When it was read: the latest instant of its parts.
    pub observed_at: DateTime<Utc>,
    /// Open PRs.
    pub open_prs: Reading<u32>,
    /// `pr-open-skip` lockout on the work finder's last tick.
    pub lockout: Reading<bool>,
    /// Median recent PR CI run, seconds.
    pub ci_typical_sec: Reading<i64>,
}

/// One PR's friction, as last read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrFriction {
    /// When it was read.
    pub observed_at: DateTime<Utc>,
    /// Its head's CI.
    pub ci: Reading<CiStatus>,
    /// Behind its base branch.
    pub behind: Reading<bool>,
    /// Merge conflicts.
    pub conflict: Reading<bool>,
}

/// The latest friction readings, by lowercased repo slug and PR.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrictionBook {
    repos: BTreeMap<String, RepoFriction>,
    prs: BTreeMap<(String, u32), PrFriction>,
}

/// The repo-level feature names.
const REPO_FEATURES: [&str; 4] = [
    "repo_open_prs",
    "repo_pr_open_lockout",
    "repo_ci_typical_duration_sec",
    "repo_friction_observed_at",
];

/// The PR-level feature names.
const PR_FEATURES: [&str; 4] = [
    "pr_ci_status",
    "pr_behind_main",
    "pr_merge_conflict",
    "pr_friction_observed_at",
];

fn omit(omitted: &mut Vec<FeatureOmitted>, name: &str, reason: &str) {
    if !omitted.iter().any(|o| o.name == name) {
        omitted.push(FeatureOmitted {
            name: name.to_string(),
            reason: reason.to_string(),
        });
    }
}

/// `Some(value)`, or `None` with `name` omitted for the reading's reason.
fn take<T: Clone>(
    reading: &Reading<T>,
    name: &str,
    omitted: &mut Vec<FeatureOmitted>,
) -> Option<T> {
    match reading {
        Ok(v) => Some(v.clone()),
        Err(reason) => {
            omit(omitted, name, reason);
            None
        }
    }
}

/// Why a reading read at `observed_at` may not be used at `as_of`, if so.
fn unusable(observed_at: DateTime<Utc>, as_of: DateTime<Utc>) -> Option<&'static str> {
    if observed_at > as_of {
        Some("read_after_as_of")
    } else if as_of - observed_at > Duration::seconds(MAX_READING_AGE_SECS) {
        Some("stale_reading")
    } else {
        None
    }
}

impl FrictionBook {
    /// Record `repo`'s friction.
    pub fn set_repo(&mut self, repo: &str, friction: RepoFriction) {
        self.repos.insert(repo.to_ascii_lowercase(), friction);
    }

    /// Record PR `number`'s friction.
    pub fn set_pr(&mut self, repo: &str, number: u32, friction: PrFriction) {
        self.prs
            .insert((repo.to_ascii_lowercase(), number), friction);
    }

    /// `repo`'s last reading.
    #[must_use]
    pub fn repo(&self, repo: &str) -> Option<&RepoFriction> {
        self.repos.get(&repo.to_ascii_lowercase())
    }

    /// PR `number`'s last reading.
    #[must_use]
    pub fn pr(&self, repo: &str, number: u32) -> Option<&PrFriction> {
        self.prs.get(&(repo.to_ascii_lowercase(), number))
    }

    /// The repos of `candidates` with no reading, or one older than
    /// `refresh_secs` at `at`, in input order.
    #[must_use]
    pub fn due_repos(
        &self,
        candidates: &[String],
        at: DateTime<Utc>,
        refresh_secs: i64,
    ) -> Vec<String> {
        candidates
            .iter()
            .filter(|r| {
                self.repo(r)
                    .is_none_or(|f| at - f.observed_at >= Duration::seconds(refresh_secs))
            })
            .cloned()
            .collect()
    }

    /// At most `budget` PRs of `candidates` due a read (no reading, or one
    /// older than `refresh_secs` at `at`), never-read first, then oldest.
    #[must_use]
    pub fn due_prs(
        &self,
        candidates: &[(String, u32)],
        at: DateTime<Utc>,
        refresh_secs: i64,
        budget: usize,
    ) -> Vec<(String, u32)> {
        type Due<'a> = (Option<DateTime<Utc>>, &'a (String, u32));
        let mut due: Vec<Due<'_>> = candidates
            .iter()
            .map(|c| (self.pr(&c.0, c.1).map(|f| f.observed_at), c))
            .filter(|(seen, _)| seen.is_none_or(|t| at - t >= Duration::seconds(refresh_secs)))
            .collect();
        due.sort_by_key(|(seen, c)| (*seen, (*c).clone()));
        due.into_iter()
            .take(budget)
            .map(|(_, c)| c.clone())
            .collect()
    }

    /// Forget every PR of `repo` not in `open`.
    pub fn retain_prs(&mut self, repo: &str, open: &[u32]) {
        let repo = repo.to_ascii_lowercase();
        self.prs.retain(|(r, n), _| *r != repo || open.contains(n));
    }

    /// Copy the friction known at `as_of` onto `features`, and name every
    /// friction feature left null in `omitted` with its reason. `labels` are
    /// the item's labels, `None` when it has not been listed yet.
    pub fn apply(
        &self,
        repo: &str,
        pr: Option<u32>,
        labels: Option<&[String]>,
        as_of: DateTime<Utc>,
        features: &mut Features,
        omitted: &mut Vec<FeatureOmitted>,
    ) {
        match labels {
            // Any `labels::hold_labels()` entry (`check_holds`), deliberately
            // broader than `pr_flags`' `FLAG_OP_HOLD`; see #10278.
            Some(labels) => features.operator_hold = Some(check_holds(labels).is_err()),
            None => omit(omitted, "operator_hold", "labels_not_listed"),
        }
        match self.repo(repo) {
            None => REPO_FEATURES
                .iter()
                .for_each(|n| omit(omitted, n, "not_read_yet")),
            Some(r) => match unusable(r.observed_at, as_of) {
                Some(why) => REPO_FEATURES.iter().for_each(|n| omit(omitted, n, why)),
                None => {
                    features.repo_open_prs = take(&r.open_prs, "repo_open_prs", omitted);
                    features.repo_pr_open_lockout =
                        take(&r.lockout, "repo_pr_open_lockout", omitted);
                    features.repo_ci_typical_duration_sec =
                        take(&r.ci_typical_sec, "repo_ci_typical_duration_sec", omitted);
                    features.repo_friction_observed_at = Some(r.observed_at);
                }
            },
        }
        let Some(number) = pr else {
            PR_FEATURES.iter().for_each(|n| omit(omitted, n, "no_pr"));
            return;
        };
        match self.pr(repo, number) {
            None => PR_FEATURES
                .iter()
                .for_each(|n| omit(omitted, n, "not_read_yet")),
            Some(p) => match unusable(p.observed_at, as_of) {
                Some(why) => PR_FEATURES.iter().for_each(|n| omit(omitted, n, why)),
                None => {
                    features.pr_ci_status =
                        take(&p.ci, "pr_ci_status", omitted).map(|c| c.as_str().to_string());
                    features.pr_behind_main = take(&p.behind, "pr_behind_main", omitted);
                    features.pr_merge_conflict = take(&p.conflict, "pr_merge_conflict", omitted);
                    features.pr_friction_observed_at = Some(p.observed_at);
                }
            },
        }
    }
}

/// The open-PR count from one `pulls?state=open&per_page=100` page. A full
/// page is only a lower bound, so it is refused rather than understated.
pub fn open_pr_count(page: &Value) -> Reading<u32> {
    let rows = page.as_array().ok_or("unreadable_listing")?;
    if rows.len() >= OPEN_PR_PAGE {
        return Err("over_one_page");
    }
    u32::try_from(rows.len()).map_err(|_| "over_one_page")
}

/// `(behind, conflict)` from one `pulls/{n}` read.
///
/// `mergeable` decides conflicts (`null` while GitHub is still computing it).
/// `mergeable_state` decides behind-ness where it can: `behind` is behind;
/// `clean`, `unstable` and `has_hooks` are not; `blocked`, `draft`, `dirty`
/// and `unknown` mask it, so it is not claimed either way.
pub fn pr_mergeability(pull: &Value) -> (Reading<bool>, Reading<bool>) {
    let conflict = match pull["mergeable"].as_bool() {
        Some(mergeable) => Ok(!mergeable),
        None => Err("mergeability_unknown"),
    };
    let behind = match pull["mergeable_state"].as_str() {
        Some("behind") => Ok(true),
        Some("clean" | "unstable" | "has_hooks") => Ok(false),
        Some("unknown") | None => Err("mergeability_unknown"),
        Some(_) => Err("masked_by_merge_state"),
    };
    (behind, conflict)
}

/// The head's CI from one `commits/{sha}/check-runs` read.
pub fn ci_status(payload: &Value) -> Reading<CiStatus> {
    let runs = payload["check_runs"]
        .as_array()
        .ok_or("unreadable_check_runs")?;
    if runs.is_empty() {
        return Ok(CiStatus::None);
    }
    let failing = runs.iter().any(|r| {
        r["conclusion"]
            .as_str()
            .is_some_and(|c| FAILING.contains(&c))
    });
    if failing {
        return Ok(CiStatus::Failing);
    }
    // A failure could be on a page this read did not fetch.
    let total = payload["total_count"].as_u64().unwrap_or(0);
    if total > runs.len() as u64 {
        return Err("over_one_page");
    }
    let pending = runs
        .iter()
        .any(|r| r["status"].as_str() != Some("completed"));
    Ok(if pending {
        CiStatus::Pending
    } else {
        CiStatus::Passing
    })
}

fn parse_time(raw: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw.as_str()?)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// The median duration, seconds, of the pull-request workflow runs in one
/// `actions/runs` page that **finished before `at`** and started within
/// [`CI_WINDOW_DAYS`] of it (#10193 rule 4: nothing after `at` counts). The
/// lower median, so the value is always an observed duration.
pub fn typical_ci_duration(page: &Value, at: DateTime<Utc>) -> Reading<i64> {
    let runs = page["workflow_runs"].as_array().ok_or("unreadable_runs")?;
    let from = at - Duration::days(CI_WINDOW_DAYS);
    let mut durations: Vec<i64> = runs
        .iter()
        .filter(|r| r["status"].as_str() == Some("completed"))
        .filter(|r| r["event"].as_str() == Some("pull_request"))
        .filter_map(|r| {
            let started = parse_time(&r["run_started_at"])?;
            let finished = parse_time(&r["updated_at"])?;
            let secs = (finished - started).num_seconds();
            (started >= from && finished < at && secs > 0).then_some(secs)
        })
        .collect();
    if durations.len() < CI_MIN_RUNS {
        return Err("too_few_ci_runs");
    }
    durations.sort_unstable();
    Ok(durations[(durations.len() - 1) / 2])
}
