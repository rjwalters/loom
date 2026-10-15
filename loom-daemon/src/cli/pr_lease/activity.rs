//! The Doctor's activity guard (#12105): `claim` refuses a PR another actor
//! is working on right now, even when no other **Doctor** holds a lease.
//!
//! The role-keyed lease (`claim_state`) only compares Doctor against Doctor.
//! An attended session fixing a `loom:changes-requested` PR is invisible to
//! it. That session's builder holds a `loom:lease` on the PR's closing issue,
//! not a `loom:pr-lease role=doctor`. An operator pushing by hand holds
//! nothing at all. In that incident a fleet Doctor claimed the PR and the two
//! pushed in parallel to the same head. So, before a Doctor posts its own
//! lease, it also stands down (exit [`EXIT_ACTIVE`], nothing written) on:
//!
//! - **a live lease held by anyone else**: another role's live
//!   `loom:pr-lease` on the PR, or a live `loom:lease host=` record on the
//!   PR or on its single closing issue that is not this claimant's own
//!   `(host, sweep)`;
//! - **a recent push by another actor**: a push to the PR's head branch
//!   within [`PUSH_WINDOW_ENV`] minutes (default
//!   [`DEFAULT_PUSH_WINDOW_MINUTES`], `0` turns the check off) by a login
//!   other than this workspace's writer identity. The time and actor come
//!   from the repository activity API, so a rebase that keeps old commit
//!   dates still counts as a push. A write to the branch is any of
//!   [`WRITE_ACTIVITY_TYPES`]: the API reports a force push (a rebase, an
//!   amend, a `--force-with-lease`) as its own `force_push` type and takes
//!   only one `activity_type` filter, so the read asks for every type and
//!   keeps the write types client-side.
//!
//! Both checks are extra evidence on top of the PR's own lease list, which
//! keeps its fail-closed exit 5. So an unreadable closing-issue lease list or
//! activity read proceeds, and says why. A head branch in a fork is not
//! checked: its pushes are not in this repository's activity.

use chrono::{DateTime, Utc};
use loom_daemon::forge_call_stats::{ops, ForgeOp};
use loom_daemon::forge_check_claim::{
    issue_lease_comments_jq, live_leases_by_freshness, parse_lease_rows, LeaseRow,
};
use loom_daemon::gh_invocation::{AccessIntent, GhInvocation};

use super::{live_leases, Ctx, Gh, Identity, LiveLease, Role, Row};

/// `claim`: a Doctor refused because another actor is active on the PR.
/// Like 4, 5 and 6, the prompt's `[[ $? =~ ^[02]$ ]]` reads it as "stand
/// down; next PR".
pub(crate) const EXIT_ACTIVE: i32 = 7;

/// Minutes a push by another actor holds the PR from a Doctor.
pub(crate) const PUSH_WINDOW_ENV: &str = "LOOM_DOCTOR_PUSH_WINDOW_MINUTES";
/// [`PUSH_WINDOW_ENV`]'s default: the lease TTL's 15 minutes, the same
/// "recently active" window a lease gets.
pub(crate) const DEFAULT_PUSH_WINDOW_MINUTES: i64 = 15;

/// The activity types that count as a write to the head branch. The API
/// reports a force push as `force_push`, not `push`; a deleted and
/// re-created branch shows as `branch_creation`.
pub(crate) const WRITE_ACTIVITY_TYPES: [&str; 3] = ["push", "force_push", "branch_creation"];

/// How many recent activity records the read asks for. Only the newest
/// write inside the window matters, and a branch rarely sees more than a few
/// in 15 minutes.
const ACTIVITY_PAGE: u32 = 10;

/// The activity read has no inventory row yet.
const ACTIVITY_OP: ForgeOp =
    ForgeOp::uninventoried("repo activity read by the Doctor's recent-push guard (#12105)");

/// The push window from [`PUSH_WINDOW_ENV`]'s raw value: unset, empty or
/// unparseable is the default, a negative value is `0`. `None` = off. Pure.
pub(crate) fn push_window(raw: Option<&str>) -> Option<chrono::Duration> {
    let minutes = raw
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(DEFAULT_PUSH_WINDOW_MINUTES)
        .max(0);
    (minutes > 0).then(|| chrono::Duration::minutes(minutes))
}

/// One push to the head branch, as the activity API reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Push {
    pub(crate) at: DateTime<Utc>,
    pub(crate) login: String,
}

/// Parse the activity read's NDJSON (`{timestamp, login, type}` per line),
/// keeping only [`WRITE_ACTIVITY_TYPES`]. Malformed lines, and records of any
/// other type (a merge, a branch deletion), drop. Pure.
pub(crate) fn parse_pushes(ndjson: &str) -> Vec<Push> {
    ndjson
        .lines()
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
            let kind = v.get("type")?.as_str()?;
            if !WRITE_ACTIVITY_TYPES.contains(&kind) {
                return None;
            }
            let at = DateTime::parse_from_rfc3339(v.get("timestamp")?.as_str()?)
                .ok()?
                .with_timezone(&Utc);
            let login = v.get("login")?.as_str()?.trim().to_string();
            (!login.is_empty()).then_some(Push { at, login })
        })
        .collect()
}

/// The newest push inside `window` by an actor `is_self` does not claim.
/// A push stamped in the future (clock skew) counts as recent. Pure.
pub(crate) fn recent_foreign_push(
    pushes: &[Push],
    now: DateTime<Utc>,
    window: chrono::Duration,
    is_self: impl Fn(&str) -> bool,
) -> Option<&Push> {
    pushes
        .iter()
        .filter(|p| now.signed_duration_since(p.at) < window)
        .filter(|p| !is_self(&p.login))
        .max_by_key(|p| p.at)
}

/// Every live `loom:pr-lease` on the PR held under a role other than `me`'s.
/// A peer of `me`'s own role is the role-keyed claim's business. Pure.
pub(crate) fn other_role_leases(
    rows: &[Row],
    me: &Identity,
    now: DateTime<Utc>,
    ttl: chrono::Duration,
) -> Vec<LiveLease> {
    let mut out: Vec<LiveLease> = [Role::Doctor, Role::Judge, Role::Curator, Role::Champion]
        .into_iter()
        .filter(|r| *r != me.role)
        .flat_map(|r| live_leases(rows, r, now, ttl))
        .collect();
    out.sort_by_key(|l| l.id);
    out
}

/// The live issue-family `loom:lease` holders among `rows` that are not
/// `me`'s own `(host, sweep)` (an in-session sweep's builder and Doctor
/// share one). Pure.
pub(crate) fn foreign_issue_leases(
    rows: &[LeaseRow],
    me: &Identity,
    now: DateTime<Utc>,
    ttl: chrono::Duration,
) -> Vec<(String, String)> {
    let ttl_minutes = ttl.num_seconds() as f64 / 60.0;
    live_leases_by_freshness(rows, now, ttl_minutes)
        .into_iter()
        .filter(|(host, sweep)| !(*host == me.host && *sweep == me.sweep))
        .collect()
}

/// Where the PR's head lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Head {
    /// The head branch name.
    pub(crate) branch: String,
    /// The head branch is in this repository (not a fork).
    pub(crate) same_repo: bool,
}

/// Parse `pulls/{pr}`'s `{branch, head_repo, base_repo}` projection. Pure.
pub(crate) fn parse_head(v: &serde_json::Value) -> Option<Head> {
    let branch = v.get("branch")?.as_str()?.trim().to_string();
    if branch.is_empty() {
        return None;
    }
    let base = v.get("base_repo").and_then(serde_json::Value::as_str);
    let head = v.get("head_repo").and_then(serde_json::Value::as_str);
    let same_repo = matches!((head, base), (Some(h), Some(b)) if h.eq_ignore_ascii_case(b));
    Some(Head { branch, same_repo })
}

/// Percent-encode `value` for a query string: unreserved characters and
/// `/` pass, everything else is `%XX`. Pure.
pub(crate) fn query_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// What the guard found, for the refusal line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Activity {
    /// Another role's live PR lease.
    PrLease(LiveLease),
    /// A live issue-family lease on `number` (the PR or its closing issue).
    IssueLease {
        number: u64,
        host: String,
        sweep: String,
    },
    /// A push to the head branch by another actor.
    Push(Push),
}

impl Activity {
    fn describe(&self) -> String {
        match self {
            Self::PrLease(l) => format!(
                "a live {} PR lease (host={} sweep={}, comment {})",
                l.holder.role.as_str(),
                l.holder.host,
                l.holder.sweep,
                l.id
            ),
            Self::IssueLease {
                number,
                host,
                sweep,
            } => format!("a live `loom:lease` on #{number} (host={host} sweep={sweep})"),
            Self::Push(p) => {
                format!("a push to the head branch by `{}` at {}", p.login, p.at.to_rfc3339())
            }
        }
    }
}

impl Gh {
    /// Trusted issue-family `loom:lease` / `loom:lease-yield` rows on
    /// `number`; `None` when unreadable.
    pub(crate) fn issue_lease_rows(
        &self,
        number: u64,
        policy: &loom_daemon::comment_trust::TrustPolicy,
    ) -> Option<Vec<LeaseRow>> {
        // Built through the gh facade (`Gh::inv` -> `GhInvocation`), so the
        // `--paginate` read is counted and page-walkable; the explicit type
        // also lets the per-file paginate-site lint see the facade.
        let read: GhInvocation =
            self.inv("pr_lease.issue_leases", AccessIntent::Read, ops::COMMENT_LIST);
        let out = read
            .args([
                "api",
                &format!("repos/{{owner}}/{{repo}}/issues/{number}/comments"),
                "--paginate",
                "--jq",
                &issue_lease_comments_jq(),
            ])
            .run();
        let out = out.ok_output()?;
        let trusted = policy.trusted_ndjson(&out.stdout);
        Some(parse_lease_rows(&String::from_utf8_lossy(&trusted)))
    }

    /// The PR's head branch and whether it is in this repository.
    pub(crate) fn pr_head_branch(&self, pr: u64) -> Option<Head> {
        let out = self
            .inv("pr_lease.pr_head_branch", AccessIntent::Read, ops::PR_VIEW_STATE)
            .args([
                "api",
                &format!("repos/{{owner}}/{{repo}}/pulls/{pr}"),
                "--jq",
                "{branch: .head.ref, head_repo: .head.repo.full_name, base_repo: .base.repo.full_name}",
            ])
            .run();
        parse_head(&serde_json::from_slice(&out.ok_output()?.stdout).ok()?)
    }

    /// The newest writes to `branch` (every [`WRITE_ACTIVITY_TYPES`] kind);
    /// `None` when unreadable.
    pub(crate) fn branch_pushes(&self, branch: &str) -> Option<Vec<Push>> {
        let out = self
            .inv("pr_lease.branch_pushes", AccessIntent::Read, ACTIVITY_OP)
            .args([
                "api",
                &format!(
                    "repos/{{owner}}/{{repo}}/activity?ref={}&per_page={ACTIVITY_PAGE}",
                    query_escape(&format!("refs/heads/{branch}"))
                ),
                "--jq",
                ".[] | {timestamp: .timestamp, login: (.actor.login // \"\"), \
                 type: (.activity_type // \"\")}",
            ])
            .run();
        Some(parse_pushes(&String::from_utf8_lossy(&out.ok_output()?.stdout)))
    }
}

impl Ctx {
    /// `claim`'s activity guard: `Some(exit)` when this Doctor must stand
    /// down because another actor is active on the PR.
    pub(crate) fn activity_refusal(&self, rows: &[Row]) -> Option<i32> {
        if self.me.role != Role::Doctor {
            return None;
        }
        let found = self.find_activity(rows)?;
        self.say(&format!(
            "PEER_ACTIVE: {} — another actor is working this PR; stand down without touching \
             the worktree or `loom:treating`, next PR (#12105)",
            found.describe()
        ));
        Some(EXIT_ACTIVE)
    }

    fn find_activity(&self, rows: &[Row]) -> Option<Activity> {
        let now = (self.now)();
        if let Some(l) = other_role_leases(rows, &self.me, now, self.ttl)
            .into_iter()
            .next()
        {
            return Some(Activity::PrLease(l));
        }
        let mut numbers = vec![self.pr()];
        numbers.extend(self.gh.story_issue(self.pr()).map(u64::from));
        for number in numbers {
            let Some(lease_rows) = self.gh.issue_lease_rows(number, &self.policy) else {
                self.say(&format!(
                    "could not read #{number}'s `loom:lease` records; not counted (fail-open)"
                ));
                continue;
            };
            if let Some((host, sweep)) = foreign_issue_leases(&lease_rows, &self.me, now, self.ttl)
                .into_iter()
                .next()
            {
                return Some(Activity::IssueLease {
                    number,
                    host,
                    sweep,
                });
            }
        }
        let window = push_window(std::env::var(PUSH_WINDOW_ENV).ok().as_deref())?;
        let Some(head) = self.gh.pr_head_branch(self.pr()) else {
            self.say("could not read the PR's head branch; recent pushes not checked (fail-open)");
            return None;
        };
        if !head.same_repo {
            return None;
        }
        let Some(pushes) = self.gh.branch_pushes(&head.branch) else {
            self.say("could not read the head branch's pushes; not checked (fail-open)");
            return None;
        };
        recent_foreign_push(&pushes, now, window, |login| self.policy.is_self(login))
            .cloned()
            .map(Activity::Push)
    }
}

#[cfg(test)]
#[path = "activity_tests.rs"]
mod tests;
