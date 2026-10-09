//! The features that need their own forge reads (#10232): PR size, required
//! checks, and the issue body's markers and author.
//!
//! This module is the pure half: what to read each pass, inside a budget, and
//! what a read means at an estimate's `as_of`. The reads themselves go
//! through the shared ETag store ([`super::pr_features_forge`]).
//!
//! # Budget
//!
//! At most [`FEATURE_READ_BUDGET`] forge calls per ETA pass for PR reads,
//! plus [`ISSUE_READ_BUDGET`] for issue reads and [`CHECKS_READ_BUDGET`] for
//! check-run reads (#11028), across
//! `pulls/{n}`, `commits/{sha}/check-runs` and `commits/{sha}/status`,
//! `issues/{n}` and the base branch's required-context lookup (a ruleset call
//! and a classic branch-protection call). The budget is charged per call
//! ([`ReadKind::cost`]), not per planned read. This budget is separate from
//! the journal resolver's (`observability::eta::FORGE_READ_BUDGET`): feature
//! reads never delay an outcome read, and the reverse. Reads that do not fit
//! are not lost. They stay wanted, and the oldest-attempted (never-attempted
//! first, failed attempts included) go first next pass, so a read that keeps
//! failing cannot starve the others. An item whose wanted read was deferred,
//! and that has no earlier answer, records `budget_exhausted`.
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
//!
//! # Required checks only
//!
//! `checks_pending` and `checks_failed` count the base branch's **required**
//! contexts, never every check run: an optional check that fails does not
//! block a merge. The required set comes from the same lookup `forge
//! wait-checks` uses ([`crate::forge_wait_checks::reads::required_contexts`]),
//! read at most once per base branch per [`REQUIRED_REFRESH_SEC`], and the
//! runs are classified by the same rollup ([`crate::forge_wait_checks::verdict::fold`]).
//! A required context that failed counts as failed; one still running, or
//! not yet registered on the head, as pending. While the set is unknown (not
//! read yet, or its lookup failed with no earlier answer) both features are
//! null with a reason, never a count over all checks.

use super::explanation::{FeatureOmitted, Features};
use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Feature reads allowed per ETA pass, across every repo and read kind.
pub const FEATURE_READ_BUDGET: usize = 12;

/// Issue reads allowed per ETA pass on top of [`FEATURE_READ_BUDGET`] (#11028).
/// An issue body changes rarely and its read is one conditional GET, but it
/// is the only source of `complexity_marker`, `points_marker` and `author`;
/// sharing one pool with PR reads starved it (`budget_exhausted` on 98% of
/// estimates). With its own pool the backlog drains in
/// `ceil(items / ISSUE_READ_BUDGET)` passes, then only refreshes.
pub const ISSUE_READ_BUDGET: usize = 60;

/// Check-run reads (two forge calls each) allowed per ETA pass on top of
/// [`FEATURE_READ_BUDGET`] (#11028). `checks_*` need a fresh check read for the
/// PR's head; sharing the pool with `pulls/{n}` left them null on ~99% of
/// estimates while `pr_ci_status` (a separate reader) was known on 74%.
pub const CHECKS_READ_BUDGET: usize = 40;

/// A `pulls/{n}` or check-runs answer is re-read once it is this old.
pub const PR_REFRESH_SEC: i64 = 15 * 60;

/// An `issues/{n}` answer is re-read once it is this old.
pub const ISSUE_REFRESH_SEC: i64 = 60 * 60;

/// The oldest `pulls/{n}` or check-runs answer an estimate may use.
pub const PR_MAX_AGE_SEC: i64 = 60 * 60;

/// The oldest `issues/{n}` answer an estimate may use.
pub const ISSUE_MAX_AGE_SEC: i64 = 24 * 3600;

/// A base branch's required-context set is re-read once it is this old.
pub const REQUIRED_REFRESH_SEC: i64 = 60 * 60;

/// The oldest required-context set an estimate may use.
pub const REQUIRED_MAX_AGE_SEC: i64 = 6 * 3600;

/// The PR-size features, in [`Features`] field order.
pub const PR_SIZE_FEATURES: [&str; 4] = [
    "pr_additions",
    "pr_deletions",
    "pr_changed_files",
    "pr_commits",
];

/// The check features, in [`Features`] field order.
pub const CHECK_FEATURES: [&str; 2] = ["checks_pending", "checks_failed"];

/// The all-check counts (#10334), kept apart from [`CHECK_FEATURES`]: every
/// run on the head, required or not. They need no required-context lookup.
pub const ALL_CHECK_FEATURES: [&str; 2] = ["checks_all_pending", "checks_all_failed"];

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
    /// The base branch's required-context set is not known at `as_of`: no
    /// lookup has answered yet (not wanted yet, over the budget, or only
    /// answered at or after `as_of`), or the PR read shows no base branch.
    pub const REQUIRED_UNKNOWN: &str = "required_unknown";
    /// The required-context lookup failed, and there is no earlier answer.
    pub const REQUIRED_LOOKUP_FAILED: &str = "required_lookup_failed";
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
    /// The base branch's required status-check contexts (the `forge
    /// wait-checks` lookup).
    Required,
    /// `GET repos/{o}/{r}/issues/{n}`.
    Issue,
}

impl ReadKind {
    /// Forge calls one read of this kind makes, which is what the budget
    /// charges: a `Checks` read is the check-runs call plus the combined
    /// legacy-status call, and a `Required` lookup is the ruleset REST call
    /// plus the classic-protection GraphQL call. Charged in full even when
    /// the first call fails and the second never runs.
    #[must_use]
    pub const fn cost(self) -> usize {
        match self {
            Self::Checks | Self::Required => 2,
            Self::Pull | Self::Issue => 1,
        }
    }
}

/// Forge calls `reads` make in all: what counts against the budget.
#[must_use]
pub fn total_cost(reads: &[FeatureRead]) -> usize {
    reads.iter().map(|r| r.kind.cost()).sum()
}

/// One planned read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureRead {
    /// `owner/repo`, lowercased.
    pub repo: String,
    /// Which read.
    pub kind: ReadKind,
    /// The PR number (`Pull`, `Checks`) or issue number (`Issue`); `0` for
    /// `Required`, which is per base branch.
    pub number: u32,
    /// The head commit, for `Checks`.
    pub sha: Option<String>,
    /// The base branch, for `Required`.
    pub base: Option<String>,
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
            ReadKind::Required => {
                format!("repos/{repo}/rules/branches/{}", self.base.as_deref().unwrap_or_default())
            }
        }
    }

    /// The combined legacy-status path for a `Checks` read's head.
    #[must_use]
    pub fn status_url(&self) -> String {
        format!(
            "repos/{}/commits/{}/status?per_page=100",
            self.repo,
            self.sha.as_deref().unwrap_or_default()
        )
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
    /// `base.ref`.
    pub base_ref: Option<String>,
    /// `head.ref`: the branch a stacked PR's child is based on (#10526).
    pub head_ref: Option<String>,
    /// `updated_at`.
    pub updated_at: Option<DateTime<Utc>>,
}

/// What one check-runs read said, per check name, as the `forge wait-checks`
/// rollup classifies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChecksSnapshot {
    /// When the read returned.
    pub read_at: DateTime<Utc>,
    /// The commit read.
    pub sha: String,
    /// Names with a run not yet completed.
    pub pending: BTreeSet<String>,
    /// Names with a run completed other than `success`, `neutral` or
    /// `skipped`.
    pub failing: BTreeSet<String>,
    /// Every name with a run on the commit.
    pub seen: BTreeSet<String>,
    /// `total_count` exceeded the runs returned.
    pub truncated: bool,
}

impl ChecksSnapshot {
    /// `(pending, failed)` over every check seen, required or not (#10334).
    #[must_use]
    pub fn all_counts(&self) -> (u32, u32) {
        let n = |c: usize| u32::try_from(c).unwrap_or(u32::MAX);
        let pending = self.pending.difference(&self.failing).count();
        (n(pending), n(self.failing.len()))
    }

    /// `(pending, failed)` over the `required` contexts only. A required
    /// context that failed is failed; one still running, or with no run on
    /// the commit yet, is pending. Every other check is ignored, so with
    /// nothing required both are zero.
    #[must_use]
    pub fn required_counts(&self, required: &[String]) -> (u32, u32) {
        let required: BTreeSet<&String> = required.iter().collect();
        let failed = required.iter().filter(|c| self.failing.contains(**c));
        let pending = required.iter().filter(|c| {
            !self.failing.contains(**c) && (self.pending.contains(**c) || !self.seen.contains(**c))
        });
        let n = |c: usize| u32::try_from(c).unwrap_or(u32::MAX);
        (n(pending.count()), n(failed.count()))
    }
}

/// What one required-context lookup said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredSnapshot {
    /// When the lookup returned.
    pub read_at: DateTime<Utc>,
    /// The base branch's required contexts (possibly none).
    pub contexts: Vec<String>,
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
        base_ref: body["base"]["ref"].as_str().map(str::to_string),
        head_ref: body["head"]["ref"].as_str().map(str::to_string),
        updated_at: time(&body["updated_at"]),
    })
}

/// A check-runs body for `sha`, read at `read_at`, classified by the
/// `forge wait-checks` rollup. The body is the check-runs payload with the
/// head's combined legacy-status payload under `status` (as
/// [`super::pr_features_forge`] builds it), so a required context reported
/// as a commit status is seen like one reported as a check run. `None` for a
/// body outside that contract, or a status payload for another commit.
#[must_use]
pub fn parse_checks(body: &Value, sha: &str, read_at: DateTime<Utc>) -> Option<ChecksSnapshot> {
    let listed = body["check_runs"].as_array()?.len() as u64;
    let status = body.get("status").unwrap_or(&Value::Null);
    if status
        .get("sha")
        .and_then(Value::as_str)
        .is_some_and(|s| s != sha)
    {
        return None;
    }
    let statuses = status["statuses"].as_array().map_or(0, Vec::len) as u64;
    let status_total = status["total_count"].as_u64().unwrap_or(statuses);
    let rollup = crate::forge_wait_checks::verdict::fold(body, status).ok()?;
    Some(ChecksSnapshot {
        read_at,
        sha: sha.to_string(),
        pending: rollup.pending.into_iter().collect(),
        failing: rollup.failing.into_iter().collect(),
        seen: rollup.seen,
        truncated: rollup.total > listed + statuses || status_total > statuses,
    })
}

/// A required-context answer (`{"required_contexts": [...]}`, as
/// [`super::pr_features_forge`] builds it), read at `read_at`.
#[must_use]
pub fn parse_required(body: &Value, read_at: DateTime<Utc>) -> Option<RequiredSnapshot> {
    let contexts = body["required_contexts"]
        .as_array()?
        .iter()
        .map(|c| c.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()?;
    Some(RequiredSnapshot { read_at, contexts })
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
    /// When a read last returned, answered or failed. Scheduling priority
    /// uses it, so a read that keeps failing cannot hold its place at the
    /// front of the queue and starve healthy reads behind it.
    tried_at: Option<DateTime<Utc>>,
}

impl<T> Default for Slot<T> {
    fn default() -> Self {
        Slot {
            last: None,
            attempt: Attempt::None,
            tried_at: None,
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

/// `(repo, base branch)`.
type BaseKey = (String, String);

/// Every answer so far, per PR and issue. Bounded by the tracked items:
/// [`PrFeatureStore::plan`] forgets what no item wants any more.
#[derive(Debug, Clone, Default)]
pub struct PrFeatureStore {
    pulls: BTreeMap<Key, Slot<PullSnapshot>>,
    checks: BTreeMap<Key, Slot<ChecksSnapshot>>,
    issues: BTreeMap<Key, Slot<IssueSnapshot>>,
    required: BTreeMap<BaseKey, Slot<RequiredSnapshot>>,
}

/// One candidate read and its priority: never attempted first, then the
/// oldest attempt, whether it answered or failed.
struct Candidate {
    /// When the read was last attempted (answered or failed), for ordering.
    tried: Option<DateTime<Utc>>,
    read: FeatureRead,
}

/// The later of the last answer and the last attempt.
fn tried<T>(slot: Option<&Slot<T>>, answered: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    answered.max(slot.and_then(|s| s.tried_at))
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
        let all_bases: BTreeSet<BaseKey> = self
            .pulls
            .iter()
            .filter_map(|((repo, _), s)| Some((repo.clone(), s.last.as_ref()?.base_ref.clone()?)))
            .collect();
        self.required.retain(|k, _| all_bases.contains(k));
        let readable = wanted.iter().filter(|w| w.readable);
        let issues: Vec<Key> = readable.clone().map(|w| key(w, w.issue)).collect();
        let prs: Vec<Key> = readable.filter_map(|w| Some(key(w, w.pr?))).collect();

        let mut candidates = Vec::new();
        let mut bases = BTreeSet::new();
        for key in &issues {
            let last = self
                .issues
                .get(key)
                .and_then(|s| s.last.as_ref())
                .map(|s| s.read_at);
            if due(last, now, ISSUE_REFRESH_SEC) {
                candidates.push(Candidate {
                    tried: tried(self.issues.get(key), last),
                    read: FeatureRead {
                        repo: key.0.clone(),
                        kind: ReadKind::Issue,
                        number: key.1,
                        sha: None,
                        base: None,
                    },
                });
            }
        }
        for key in &prs {
            let pull = self.pulls.get(key).and_then(|s| s.last.as_ref());
            let last = pull.map(|s| s.read_at);
            if due(last, now, PR_REFRESH_SEC) {
                candidates.push(Candidate {
                    tried: tried(self.pulls.get(key), last),
                    read: FeatureRead {
                        repo: key.0.clone(),
                        kind: ReadKind::Pull,
                        number: key.1,
                        sha: None,
                        base: None,
                    },
                });
            }
            // Checks need the head from a PR read, and only matter while open.
            let open = pull.filter(|p| p.open);
            if let Some(base) = open.and_then(|p| p.base_ref.clone()) {
                bases.insert((key.0.clone(), base));
            }
            let Some(sha) = open.and_then(|p| p.head_sha.clone()) else {
                continue;
            };
            let checks = self.checks.get(key).and_then(|s| s.last.as_ref());
            let last = checks.filter(|c| c.sha == sha).map(|c| c.read_at);
            if due(last, now, PR_REFRESH_SEC) {
                candidates.push(Candidate {
                    tried: tried(self.checks.get(key), last),
                    read: FeatureRead {
                        repo: key.0.clone(),
                        kind: ReadKind::Checks,
                        number: key.1,
                        sha: Some(sha),
                        base: None,
                    },
                });
            }
        }
        // One required-context lookup per base branch the open PRs target.
        for key in bases {
            let last = self
                .required
                .get(&key)
                .and_then(|s| s.last.as_ref())
                .map(|s| s.read_at);
            if due(last, now, REQUIRED_REFRESH_SEC) {
                candidates.push(Candidate {
                    tried: tried(self.required.get(&key), last),
                    read: FeatureRead {
                        repo: key.0,
                        kind: ReadKind::Required,
                        number: 0,
                        sha: None,
                        base: Some(key.1),
                    },
                });
            }
        }
        candidates.sort_by(|a, b| {
            (a.tried.is_some(), a.tried, a.read.kind, &a.read.repo, a.read.number).cmp(&(
                b.tried.is_some(),
                b.tried,
                b.read.kind,
                &b.read.repo,
                b.read.number,
            ))
        });
        let mut reads = Vec::new();
        // Three pools (#11028): issue bodies and check reads each have their
        // own, so PR reads cannot starve them; `pulls/{n}` and the
        // required-context lookup share `budget`.
        let mut spent = [0_usize; 3];
        let caps = if budget == 0 {
            [0; 3]
        } else {
            [budget, ISSUE_READ_BUDGET, CHECKS_READ_BUDGET]
        };
        for c in candidates {
            let cost = c.read.kind.cost();
            let pool = match c.read.kind {
                ReadKind::Pull | ReadKind::Required => 0,
                ReadKind::Issue => 1,
                ReadKind::Checks => 2,
            };
            if spent[pool] + cost <= caps[pool] {
                spent[pool] += cost;
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
            ReadKind::Required => {
                self.required.entry(base_key(read)).or_default().attempt = Attempt::Deferred;
            }
        }
    }

    /// Record `read`'s answer, returned at `read_at`: the parsed body, or
    /// `None` when the read failed or did not parse.
    pub fn answer(&mut self, read: &FeatureRead, body: Option<&Value>, read_at: DateTime<Utc>) {
        let key = (read.repo.clone(), read.number);
        match read.kind {
            ReadKind::Pull => {
                let parsed = body.and_then(|b| parse_pull(b, read_at));
                settle(self.pulls.entry(key).or_default(), parsed, read_at);
            }
            ReadKind::Checks => {
                let sha = read.sha.as_deref().unwrap_or_default();
                let parsed = body.and_then(|b| parse_checks(b, sha, read_at));
                settle(self.checks.entry(key).or_default(), parsed, read_at);
            }
            ReadKind::Issue => {
                let parsed = body.and_then(|b| parse_issue(b, read_at));
                settle(self.issues.entry(key).or_default(), parsed, read_at);
            }
            ReadKind::Required => {
                let parsed = body.and_then(|b| parse_required(b, read_at));
                settle(self.required.entry(base_key(read)).or_default(), parsed, read_at);
            }
        }
    }

    /// `(head, base)` branch names of `repo`'s PR `pr` from its last answered
    /// `pulls/{n}` read, when that read saw it open (#10526). No read of
    /// its own: the dependency pass reuses what the feature pass fetched.
    #[must_use]
    pub fn open_pull_refs(&self, repo: &str, pr: u32) -> Option<(Option<String>, Option<String>)> {
        let snap = self.pulls.get(&(repo.to_string(), pr))?.last.as_ref()?;
        snap.open
            .then(|| (snap.head_ref.clone(), snap.base_ref.clone()))
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

    /// `repo`'s `base` required-context set usable at `as_of`, or why there
    /// is none. Like check runs, it must have been read before `as_of`.
    fn required_at(
        &self,
        repo: &str,
        base: Option<&str>,
        as_of: DateTime<Utc>,
    ) -> Result<&RequiredSnapshot, &'static str> {
        let base = base.ok_or(reason::REQUIRED_UNKNOWN)?;
        let slot = self
            .required
            .get(&(repo.to_string(), base.to_string()))
            .ok_or(reason::REQUIRED_UNKNOWN)?;
        let snap = slot.last.as_ref().ok_or(match slot.attempt {
            Attempt::Failed => reason::REQUIRED_LOOKUP_FAILED,
            _ => reason::REQUIRED_UNKNOWN,
        })?;
        if snap.read_at >= as_of {
            return Err(reason::REQUIRED_UNKNOWN);
        }
        if as_of - snap.read_at > Duration::seconds(REQUIRED_MAX_AGE_SEC) {
            return Err(reason::READ_STALE);
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
                let base = snap.base_ref.as_deref();
                let all = self.checks_at(repo, pr, as_of);
                match &all {
                    Ok(c) => {
                        let (pending, failed) = c.all_counts();
                        features.checks_all_pending = Some(pending);
                        features.checks_all_failed = Some(failed);
                    }
                    Err(why) => omit(omitted, &ALL_CHECK_FEATURES, why),
                }
                let checks = all.and_then(|c| {
                    self.required_at(repo, base, as_of)
                        .map(|r| c.required_counts(&r.contexts))
                });
                match checks {
                    Ok((pending, failed)) => {
                        features.checks_pending = Some(pending);
                        features.checks_failed = Some(failed);
                    }
                    Err(why) => omit(omitted, &CHECK_FEATURES, why),
                }
            }
            Err(why) => {
                omit(omitted, &PR_SIZE_FEATURES, why);
                omit(omitted, &CHECK_FEATURES, why);
                omit(omitted, &ALL_CHECK_FEATURES, why);
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

fn base_key(read: &FeatureRead) -> BaseKey {
    (read.repo.clone(), read.base.clone().unwrap_or_default())
}

/// A failed read keeps the last answer: it is still the newest known value,
/// and the max age bounds how long it is used. The attempt time is recorded
/// either way, so a failing read moves behind the reads not yet attempted.
fn settle<T>(slot: &mut Slot<T>, parsed: Option<T>, read_at: DateTime<Utc>) {
    slot.tried_at = Some(read_at);
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
