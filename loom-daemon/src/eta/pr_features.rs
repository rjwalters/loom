//! The features that need their own forge reads (#10232): PR size, required
//! checks, and the issue body's markers and author.
//!
//! This module is the pure half: what to read each pass, inside a budget, and
//! what a read means at an estimate's `as_of`. The reads themselves go
//! through the shared ETag store ([`super::pr_features_forge`]).
//!
//! # Budget
//!
//! At most [`FEATURE_READ_BUDGET`] reads per ETA pass, across `pulls/{n}`,
//! `commits/{sha}/check-runs` and `issues/{n}`. This budget is separate from
//! the journal resolver's (`observability::eta::FORGE_READ_BUDGET`): feature
//! reads never delay an outcome read, and the reverse. Reads that do not fit
//! are not lost. They stay wanted, and the oldest-read (never-read first) go
//! first next pass. An item whose wanted read was deferred, and that has no
//! earlier answer, records `budget_exhausted`.
//!
//! # Point in time
//!
//! A read answers with the **current** value, so a value is used at `as_of`
//! only when it was known then:
//!
//! - the read happened before `as_of`; or
//! - it happened later, but the PR or issue was last updated before `as_of`,
//!   so the value has not changed since.
//!
//! A PR that was closed or merged when read never records a size: its final
//! size is not its size at `as_of`. A value read longer ago than its max age
//! is `read_stale`. Check runs change without touching the PR's
//! `updated_at`, so a check read is used only when it happened before
//! `as_of`, and only for the head commit the PR's own read shows.

use super::explanation::{FeatureOmitted, Features};
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use std::collections::BTreeMap;

/// Feature reads allowed per ETA pass, across every repo and read kind.
pub const FEATURE_READ_BUDGET: usize = 12;

/// A `pulls/{n}` or check-runs answer is re-read once it is this old.
pub const PR_REFRESH_SEC: i64 = 15 * 60;

/// An `issues/{n}` answer is re-read once it is this old.
pub const ISSUE_REFRESH_SEC: i64 = 60 * 60;

/// The oldest `pulls/{n}` or check-runs answer an estimate may use.
pub const PR_MAX_AGE_SEC: i64 = 60 * 60;

/// The oldest `issues/{n}` answer an estimate may use.
pub const ISSUE_MAX_AGE_SEC: i64 = 24 * 3600;

/// The PR-size features, in [`Features`] field order.
pub const PR_SIZE_FEATURES: [&str; 4] = [
    "pr_additions",
    "pr_deletions",
    "pr_changed_files",
    "pr_commits",
];

/// The check features, in [`Features`] field order.
pub const CHECK_FEATURES: [&str; 2] = ["checks_pending", "checks_failed"];

/// The issue-body features, in [`Features`] field order.
pub const ISSUE_FEATURES: [&str; 3] = ["complexity_marker", "points_marker", "author"];

/// Omission reasons this module assigns (`features_omitted[].reason`).
pub mod reason {
    /// The read was wanted but did not fit this pass's budget, and there is
    /// no earlier answer.
    pub const BUDGET_EXHAUSTED: &str = "budget_exhausted";
    /// Nothing has asked for the read yet (the item is newer than the last
    /// pass, or its repo was not listed).
    pub const NOT_READ_YET: &str = "not_read_yet";
    /// The last read failed, and there is no earlier answer.
    pub const READ_FAILED: &str = "read_failed";
    /// The newest answer is older than the max age at `as_of`.
    pub const READ_STALE: &str = "read_stale";
    /// The PR was closed or merged when read.
    pub const PR_NOT_OPEN: &str = "pr_not_open";
    /// The PR was read after `as_of` and changed after `as_of`.
    pub const PR_CHANGED_AFTER_AS_OF: &str = "pr_changed_after_as_of";
    /// The issue was read after `as_of` and changed after `as_of`.
    pub const ISSUE_CHANGED_AFTER_AS_OF: &str = "issue_changed_after_as_of";
    /// The check runs were read at or after `as_of`.
    pub const CHECKS_READ_AFTER_AS_OF: &str = "checks_read_after_as_of";
    /// The check runs read are for another commit than the PR's head.
    pub const CHECKS_FOR_OTHER_HEAD: &str = "checks_for_other_head";
    /// The head commit has more check runs than one page returns.
    pub const CHECKS_TRUNCATED: &str = "checks_truncated";
    /// The issue body has no such marker.
    pub const MARKER_ABSENT: &str = "marker_absent";
    /// The `loom:points` marker is outside the points vocabulary.
    pub const MARKER_INVALID: &str = "marker_invalid";
}

/// Which read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReadKind {
    /// `GET repos/{o}/{r}/pulls/{n}`.
    Pull,
    /// `GET repos/{o}/{r}/commits/{sha}/check-runs`.
    Checks,
    /// `GET repos/{o}/{r}/issues/{n}`.
    Issue,
}

/// One planned read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureRead {
    /// `owner/repo`, lowercased.
    pub repo: String,
    /// Which read.
    pub kind: ReadKind,
    /// The PR number (`Pull`, `Checks`) or issue number (`Issue`).
    pub number: u32,
    /// The head commit, for `Checks`.
    pub sha: Option<String>,
}

impl FeatureRead {
    /// The REST path.
    #[must_use]
    pub fn url(&self) -> String {
        let repo = &self.repo;
        match self.kind {
            ReadKind::Pull => format!("repos/{repo}/pulls/{}", self.number),
            ReadKind::Issue => format!("repos/{repo}/issues/{}", self.number),
            ReadKind::Checks => format!(
                "repos/{repo}/commits/{}/check-runs?per_page=100",
                self.sha.as_deref().unwrap_or_default()
            ),
        }
    }
}

/// An item that wants feature reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    /// `owner/repo`, lowercased.
    pub repo: String,
    /// The issue.
    pub issue: u32,
    /// Its PR, when it has one.
    pub pr: Option<u32>,
    /// Its repo has a checkout to read from on this pass. An unreadable item
    /// keeps its answers but plans no read.
    pub readable: bool,
}

/// What one `pulls/{n}` read said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullSnapshot {
    /// When the read returned.
    pub read_at: DateTime<Utc>,
    /// Open (neither closed nor merged) when read.
    pub open: bool,
    /// `additions`.
    pub additions: i64,
    /// `deletions`.
    pub deletions: i64,
    /// `changed_files`.
    pub changed_files: i64,
    /// `commits`.
    pub commits: i64,
    /// `head.sha`.
    pub head_sha: Option<String>,
    /// `updated_at`.
    pub updated_at: Option<DateTime<Utc>>,
}

/// What one check-runs read said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksSnapshot {
    /// When the read returned.
    pub read_at: DateTime<Utc>,
    /// The commit read.
    pub sha: String,
    /// Runs not yet completed.
    pub pending: u32,
    /// Runs completed as failed, timed out, cancelled or needing action.
    pub failed: u32,
    /// `total_count` exceeded the runs returned.
    pub truncated: bool,
}

/// What one `issues/{n}` read said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueSnapshot {
    /// When the read returned.
    pub read_at: DateTime<Utc>,
    /// `user.login`.
    pub author: Option<String>,
    /// The `loom:complexity` marker.
    pub complexity_marker: Option<String>,
    /// The raw `loom:points` marker value.
    pub points_marker: Option<String>,
    /// `updated_at`.
    pub updated_at: Option<DateTime<Utc>>,
}

fn time(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str()?.parse().ok()
}

/// A `pulls/{n}` body, read at `read_at`.
#[must_use]
pub fn parse_pull(body: &Value, read_at: DateTime<Utc>) -> Option<PullSnapshot> {
    Some(PullSnapshot {
        read_at,
        open: body["state"].as_str()? == "open" && body["merged_at"].is_null(),
        additions: body["additions"].as_i64()?,
        deletions: body["deletions"].as_i64()?,
        changed_files: body["changed_files"].as_i64()?,
        commits: body["commits"].as_i64()?,
        head_sha: body["head"]["sha"].as_str().map(str::to_string),
        updated_at: time(&body["updated_at"]),
    })
}

/// A check-runs body for `sha`, read at `read_at`.
#[must_use]
pub fn parse_checks(body: &Value, sha: &str, read_at: DateTime<Utc>) -> Option<ChecksSnapshot> {
    let runs = body["check_runs"].as_array()?;
    let count = |f: &dyn Fn(&Value) -> bool| {
        u32::try_from(runs.iter().filter(|r| f(r)).count()).unwrap_or(u32::MAX)
    };
    let pending = count(&|r| r["status"].as_str() != Some("completed"));
    let failed = count(&|r| {
        r["status"].as_str() == Some("completed")
            && matches!(
                r["conclusion"].as_str(),
                Some("failure" | "timed_out" | "cancelled" | "action_required" | "startup_failure")
            )
    });
    let total = body["total_count"].as_u64().unwrap_or(runs.len() as u64);
    Some(ChecksSnapshot {
        read_at,
        sha: sha.to_string(),
        pending,
        failed,
        truncated: total > runs.len() as u64,
    })
}

/// An `issues/{n}` body, read at `read_at`. The markers use the repo's own
/// parsers, so the ETA reads them exactly as the work finder does.
#[must_use]
pub fn parse_issue(body: &Value, read_at: DateTime<Utc>) -> Option<IssueSnapshot> {
    let text = body["body"].as_str().unwrap_or_default();
    Some(IssueSnapshot {
        read_at,
        author: body["user"]["login"].as_str().map(str::to_string),
        complexity_marker: crate::script_helpers::sweep_experiment::extract_complexity_marker(text)
            .map(str::to_string),
        points_marker: crate::points_marker::extract_points_marker_raw(text).map(str::to_string),
        updated_at: body.get("updated_at").and_then(time),
    })
}

/// How the last attempt at a read went.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Attempt {
    /// Never wanted yet.
    #[default]
    None,
    /// Answered.
    Answered,
    /// Wanted, but over the budget.
    Deferred,
    /// Wanted and issued, but it failed.
    Failed,
}

#[derive(Debug, Clone)]
struct Slot<T> {
    last: Option<T>,
    attempt: Attempt,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Slot {
            last: None,
            attempt: Attempt::None,
        }
    }
}

impl<T> Slot<T> {
    /// Why there is no value, when there is no answer at all.
    fn missing(&self) -> &'static str {
        match self.attempt {
            Attempt::Deferred => reason::BUDGET_EXHAUSTED,
            Attempt::Failed => reason::READ_FAILED,
            Attempt::None | Attempt::Answered => reason::NOT_READ_YET,
        }
    }
}

type Key = (String, u32);

/// Every answer so far, per PR and issue. Bounded by the tracked items:
/// [`PrFeatureStore::plan`] forgets what no item wants any more.
#[derive(Debug, Clone, Default)]
pub struct PrFeatureStore {
    pulls: BTreeMap<Key, Slot<PullSnapshot>>,
    checks: BTreeMap<Key, Slot<ChecksSnapshot>>,
    issues: BTreeMap<Key, Slot<IssueSnapshot>>,
}

/// One candidate read and its priority: never read first, then oldest.
struct Candidate {
    last: Option<DateTime<Utc>>,
    read: FeatureRead,
}

fn due(last: Option<DateTime<Utc>>, now: DateTime<Utc>, refresh_sec: i64) -> bool {
    last.is_none_or(|at| now - at >= Duration::seconds(refresh_sec))
}

impl PrFeatureStore {
    /// This pass's reads for `wanted`, at most `budget` of them. Reads over
    /// the budget are marked deferred. Answers no item wants are dropped.
    pub fn plan(
        &mut self,
        wanted: &[Wanted],
        now: DateTime<Utc>,
        budget: usize,
    ) -> Vec<FeatureRead> {
        let key = |w: &Wanted, n: u32| (w.repo.clone(), n);
        let all_issues: Vec<Key> = wanted.iter().map(|w| key(w, w.issue)).collect();
        let all_prs: Vec<Key> = wanted.iter().filter_map(|w| Some(key(w, w.pr?))).collect();
        self.issues.retain(|k, _| all_issues.contains(k));
        self.pulls.retain(|k, _| all_prs.contains(k));
        self.checks.retain(|k, _| all_prs.contains(k));
        let readable = wanted.iter().filter(|w| w.readable);
        let issues: Vec<Key> = readable.clone().map(|w| key(w, w.issue)).collect();
        let prs: Vec<Key> = readable.filter_map(|w| Some(key(w, w.pr?))).collect();

        let mut candidates = Vec::new();
        for key in &issues {
            let last = self
                .issues
                .get(key)
                .and_then(|s| s.last.as_ref())
                .map(|s| s.read_at);
            if due(last, now, ISSUE_REFRESH_SEC) {
                candidates.push(Candidate {
                    last,
                    read: FeatureRead {
                        repo: key.0.clone(),
                        kind: ReadKind::Issue,
                        number: key.1,
                        sha: None,
                    },
                });
            }
        }
        for key in &prs {
            let pull = self.pulls.get(key).and_then(|s| s.last.as_ref());
            let last = pull.map(|s| s.read_at);
            if due(last, now, PR_REFRESH_SEC) {
                candidates.push(Candidate {
                    last,
                    read: FeatureRead {
                        repo: key.0.clone(),
                        kind: ReadKind::Pull,
                        number: key.1,
                        sha: None,
                    },
                });
            }
            // Checks need the head from a PR read, and only matter while open.
            let Some(sha) = pull.filter(|p| p.open).and_then(|p| p.head_sha.clone()) else {
                continue;
            };
            let checks = self.checks.get(key).and_then(|s| s.last.as_ref());
            let last = checks.filter(|c| c.sha == sha).map(|c| c.read_at);
            if due(last, now, PR_REFRESH_SEC) {
                candidates.push(Candidate {
                    last,
                    read: FeatureRead {
                        repo: key.0.clone(),
                        kind: ReadKind::Checks,
                        number: key.1,
                        sha: Some(sha),
                    },
                });
            }
        }
        candidates.sort_by(|a, b| {
            (a.last.is_some(), a.last, a.read.kind, &a.read.repo, a.read.number).cmp(&(
                b.last.is_some(),
                b.last,
                b.read.kind,
                &b.read.repo,
                b.read.number,
            ))
        });
        let mut reads = Vec::new();
        for (i, c) in candidates.into_iter().enumerate() {
            if i < budget {
                reads.push(c.read);
            } else {
                self.defer(&c.read);
            }
        }
        reads
    }

    /// Mark `read` wanted but over the budget.
    fn defer(&mut self, read: &FeatureRead) {
        let key = (read.repo.clone(), read.number);
        match read.kind {
            ReadKind::Pull => self.pulls.entry(key).or_default().attempt = Attempt::Deferred,
            ReadKind::Checks => self.checks.entry(key).or_default().attempt = Attempt::Deferred,
            ReadKind::Issue => self.issues.entry(key).or_default().attempt = Attempt::Deferred,
        }
    }

    /// Record `read`'s answer, returned at `read_at`: the parsed body, or
    /// `None` when the read failed or did not parse.
    pub fn answer(&mut self, read: &FeatureRead, body: Option<&Value>, read_at: DateTime<Utc>) {
        let key = (read.repo.clone(), read.number);
        match read.kind {
            ReadKind::Pull => {
                let parsed = body.and_then(|b| parse_pull(b, read_at));
                settle(self.pulls.entry(key).or_default(), parsed);
            }
            ReadKind::Checks => {
                let sha = read.sha.as_deref().unwrap_or_default();
                let parsed = body.and_then(|b| parse_checks(b, sha, read_at));
                settle(self.checks.entry(key).or_default(), parsed);
            }
            ReadKind::Issue => {
                let parsed = body.and_then(|b| parse_issue(b, read_at));
                settle(self.issues.entry(key).or_default(), parsed);
            }
        }
    }

    /// The PR snapshot usable at `as_of`, or why there is none.
    fn pull_at(
        &self,
        repo: &str,
        pr: u32,
        as_of: DateTime<Utc>,
    ) -> Result<&PullSnapshot, &'static str> {
        let slot = self
            .pulls
            .get(&(repo.to_string(), pr))
            .ok_or(reason::NOT_READ_YET)?;
        let snap = slot.last.as_ref().ok_or_else(|| slot.missing())?;
        if !snap.open {
            return Err(reason::PR_NOT_OPEN);
        }
        if snap.read_at >= as_of && snap.updated_at.is_none_or(|u| u >= as_of) {
            return Err(reason::PR_CHANGED_AFTER_AS_OF);
        }
        if as_of - snap.read_at > Duration::seconds(PR_MAX_AGE_SEC) {
            return Err(reason::READ_STALE);
        }
        Ok(snap)
    }

    /// The check counts usable at `as_of`, or why there are none.
    fn checks_at(
        &self,
        repo: &str,
        pr: u32,
        as_of: DateTime<Utc>,
    ) -> Result<&ChecksSnapshot, &'static str> {
        let pull = self.pull_at(repo, pr, as_of)?;
        let slot = self
            .checks
            .get(&(repo.to_string(), pr))
            .ok_or(reason::NOT_READ_YET)?;
        let snap = slot.last.as_ref().ok_or_else(|| slot.missing())?;
        if pull.head_sha.as_deref() != Some(snap.sha.as_str()) {
            return Err(reason::CHECKS_FOR_OTHER_HEAD);
        }
        if snap.read_at >= as_of {
            return Err(reason::CHECKS_READ_AFTER_AS_OF);
        }
        if as_of - snap.read_at > Duration::seconds(PR_MAX_AGE_SEC) {
            return Err(reason::READ_STALE);
        }
        if snap.truncated {
            return Err(reason::CHECKS_TRUNCATED);
        }
        Ok(snap)
    }

    /// The issue snapshot usable at `as_of`, or why there is none.
    fn issue_at(
        &self,
        repo: &str,
        issue: u32,
        as_of: DateTime<Utc>,
    ) -> Result<&IssueSnapshot, &'static str> {
        let slot = self
            .issues
            .get(&(repo.to_string(), issue))
            .ok_or(reason::NOT_READ_YET)?;
        let snap = slot.last.as_ref().ok_or_else(|| slot.missing())?;
        if snap.read_at >= as_of && snap.updated_at.is_none_or(|u| u >= as_of) {
            return Err(reason::ISSUE_CHANGED_AFTER_AS_OF);
        }
        if as_of - snap.read_at > Duration::seconds(ISSUE_MAX_AGE_SEC) {
            return Err(reason::READ_STALE);
        }
        Ok(snap)
    }

    /// Write `repo#issue`'s read features at `as_of` into `features`, and a
    /// reason for each one left null. `pr` is the item's PR, or the reason
    /// the PR features do not apply (e.g. `no_pr_yet`).
    pub fn write_to(
        &self,
        repo: &str,
        issue: u32,
        pr: Result<u32, &'static str>,
        as_of: DateTime<Utc>,
        features: &mut Features,
        omitted: &mut Vec<FeatureOmitted>,
    ) {
        let omit = |omitted: &mut Vec<FeatureOmitted>, names: &[&str], why: &str| {
            omitted.extend(names.iter().map(|n| FeatureOmitted {
                name: (*n).to_string(),
                reason: why.to_string(),
            }));
        };
        match pr.and_then(|pr| self.pull_at(repo, pr, as_of).map(|s| (pr, s))) {
            Ok((pr, snap)) => {
                features.pr_additions = Some(snap.additions);
                features.pr_deletions = Some(snap.deletions);
                features.pr_changed_files = Some(snap.changed_files);
                features.pr_commits = Some(snap.commits);
                match self.checks_at(repo, pr, as_of) {
                    Ok(c) => {
                        features.checks_pending = Some(c.pending);
                        features.checks_failed = Some(c.failed);
                    }
                    Err(why) => omit(omitted, &CHECK_FEATURES, why),
                }
            }
            Err(why) => {
                omit(omitted, &PR_SIZE_FEATURES, why);
                omit(omitted, &CHECK_FEATURES, why);
            }
        }
        match self.issue_at(repo, issue, as_of) {
            Ok(snap) => {
                features.complexity_marker = snap.complexity_marker.clone();
                if snap.complexity_marker.is_none() {
                    omit(omitted, &["complexity_marker"], reason::MARKER_ABSENT);
                }
                match snap.points_marker.as_deref() {
                    None => omit(omitted, &["points_marker"], reason::MARKER_ABSENT),
                    Some(raw) => match raw
                        .parse::<u32>()
                        .ok()
                        .filter(|_| crate::points_marker::POINTS_VALUES.contains(&raw))
                    {
                        Some(points) => features.points_marker = Some(points),
                        None => omit(omitted, &["points_marker"], reason::MARKER_INVALID),
                    },
                }
                features.author = snap.author.clone();
                if snap.author.is_none() {
                    omit(omitted, &["author"], reason::READ_FAILED);
                }
            }
            Err(why) => omit(omitted, &ISSUE_FEATURES, why),
        }
    }
}

/// A failed read keeps the last answer: it is still the newest known value,
/// and the max age bounds how long it is used.
fn settle<T>(slot: &mut Slot<T>, parsed: Option<T>) {
    match parsed {
        Some(snap) => {
            slot.last = Some(snap);
            slot.attempt = Attempt::Answered;
        }
        None => slot.attempt = Attempt::Failed,
    }
}

#[cfg(test)]
#[path = "pr_features_tests.rs"]
mod tests;
