//! How an issue's close is classified (#11398), beside [`super::diff`]: the
//! `issues/{n}` read's shape ([`IssueFacts`]) and the timeline evidence that
//! says a merge — not a hand — closed it (#11402).

use chrono::{DateTime, Utc};

use super::parse_instant;
use crate::merge_pr::refs::has_unnegated_closing_ref_in_repo;
use crate::telemetry::kinds::stage_outcome::StageExit;

/// An issue's state from its `issues/{n}` read and, once closed, its
/// `issues/{n}/events` pages (#11398). Every instant is the issue's own
/// forge `closed_at`, the same on every host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueFacts {
    /// Still open.
    Open,
    /// A merge closed it at `closed_at`: its latest `closed` event carries a
    /// `commit_id`, or a PR merged within [`MERGE_CLOSE_WINDOW_SECS`] of
    /// `closed_at` closes it — through a `Closes #N` (or repository-
    /// qualified `Closes owner/repo#N`) body, or through a Development-
    /// sidebar link (#11402). Either of those merge-closes leaves `commit_id`
    /// null, so the timeline is the evidence.
    Merged(DateTime<Utc>),
    /// No closing commit and no merged PR referencing it near `closed_at`
    /// (whatever its `state_reason`): a hand close, at `closed_at`.
    NotMerged(DateTime<Utc>),
    /// Closed at `closed_at`, but no `closed` event says how.
    ClosedUnresolved(DateTime<Utc>),
}

/// How far a referencing PR's `merged_at` may sit from the issue's
/// `closed_at` and still count as having closed it. GitHub closes the issue
/// within seconds of the merge.
pub const MERGE_CLOSE_WINDOW_SECS: i64 = 120;

/// The linked PR a Development-sidebar `connected`/`disconnected` event
/// carries, mirroring the cross-reference's `source.issue` payload (read
/// `source_issue` too, should the event form differ across API versions).
fn sidebar_source(event: &serde_json::Value) -> Option<&serde_json::Value> {
    let source = if event["source_issue"].is_null() {
        &event["source"]["issue"]
    } else {
        &event["source_issue"]
    };
    (!source.is_null()).then_some(source)
}

/// Whether a `connected` event's sidebar link was later revoked: a
/// `disconnected` event naming the same PR at a later instant. A revoked
/// link closes nothing on merge, so the event is not close evidence
/// (#11402). A disconnect that predates the (re)connect leaves the link
/// standing.
fn link_revoked_after(event: &serde_json::Value, revoked: &[(u64, DateTime<Utc>)]) -> bool {
    if event["event"] != "connected" {
        return false;
    }
    let Some(linked_at) = parse_instant(event["created_at"].as_str()) else {
        return false;
    };
    let Some(number) = sidebar_source(event).and_then(|s| s["number"].as_u64()) else {
        return false;
    };
    revoked.iter().any(|&(n, at)| n == number && at > linked_at)
}

/// Whether a timeline event is evidence that a merged PR in `repo` closed
/// issue `number`: a `cross-referenced` PR whose body closes `number` (a
/// `Closes`/`Fixes`/`Resolves` keyword — a bare `#N` or a repository-
/// qualified `owner/repo#N` naming this repository, not a bare mention), or
/// a Development-sidebar `connected` link to it that no later
/// `disconnected` event revoked, which GitHub closes on merge with no
/// keyword in the body at all. Either way the merge must sit within
/// [`MERGE_CLOSE_WINDOW_SECS`] of `closed_at` (#11402).
fn merged_pr_near(
    closed_at: DateTime<Utc>,
    repo: &str,
    number: u32,
    event: &serde_json::Value,
) -> bool {
    let kind = event["event"].as_str().unwrap_or_default();
    if kind != "cross-referenced" && kind != "connected" {
        return false;
    }
    // A cross-reference carries the PR under `source.issue`; a sidebar
    // link carries it in the shapes [`sidebar_source`] reads.
    let source = if kind == "cross-referenced" {
        &event["source"]["issue"]
    } else {
        let Some(s) = sidebar_source(event) else {
            return false;
        };
        s
    };
    if source["pull_request"].is_null()
        || parse_instant(source["pull_request"]["merged_at"].as_str())
            .is_none_or(|m| (closed_at - m).num_seconds().abs() > MERGE_CLOSE_WINDOW_SECS)
    {
        return false;
    }
    // Only a merge in this repository can close the issue.
    if !source["repository_url"].as_str().is_some_and(|u| {
        u.to_ascii_lowercase()
            .ends_with(&format!("/repos/{}", repo.to_ascii_lowercase()))
    }) {
        return false;
    }
    // A cross-reference then needs the body to close `number`; a sidebar
    // link needs nothing more.
    kind != "cross-referenced"
        || source["body"]
            .as_str()
            .is_some_and(|b| has_unnegated_closing_ref_in_repo(b, repo, u64::from(number)))
}

impl IssueFacts {
    /// Classify issue `number` of `repo`, closed at `closed_at`, from its
    /// events pages and its timeline pages (the source of the PR that closed
    /// it, #11398).
    #[must_use]
    pub fn closed(
        repo: &str,
        number: u32,
        closed_at: DateTime<Utc>,
        events: &[serde_json::Value],
        timeline: &[serde_json::Value],
    ) -> Self {
        // Sidebar links revoked by a later `disconnected` event naming the
        // same PR, as `(pr number, revoked at)` (#11402).
        let revoked_links: Vec<(u64, DateTime<Utc>)> = timeline
            .iter()
            .flat_map(|page| page.as_array().into_iter().flatten())
            .filter(|e| e["event"] == "disconnected")
            .filter_map(|e| {
                let at = parse_instant(e["created_at"].as_str())?;
                let number = sidebar_source(e)?["number"].as_u64()?;
                Some((number, at))
            })
            .collect();
        let merged_by_pr = timeline
            .iter()
            .flat_map(|page| page.as_array().into_iter().flatten())
            .any(|e| {
                merged_pr_near(closed_at, repo, number, e) && !link_revoked_after(e, &revoked_links)
            });
        let last_close = events
            .iter()
            .flat_map(|page| page.as_array().into_iter().flatten())
            .filter(|e| e["event"] == "closed")
            .filter_map(|e| Some((parse_instant(e["created_at"].as_str())?, e)))
            .max_by_key(|(at, _)| *at);
        match last_close {
            Some((_, e)) if e["commit_id"].as_str().is_some_and(|c| !c.is_empty()) => {
                IssueFacts::Merged(closed_at)
            }
            Some(_) if merged_by_pr => IssueFacts::Merged(closed_at),
            Some(_) => IssueFacts::NotMerged(closed_at),
            None => IssueFacts::ClosedUnresolved(closed_at),
        }
    }

    /// The exit and forge instant a vanished row with no resolved PR takes:
    /// `landed` at a merge close, `abandoned` at an unmerged close, `unknown`
    /// at an unresolved close, and `unknown` with no forge instant while open.
    #[must_use]
    pub fn exit(self) -> (StageExit, Option<DateTime<Utc>>) {
        match self {
            IssueFacts::Merged(at) => (StageExit::Landed, Some(at)),
            IssueFacts::NotMerged(at) => (StageExit::Abandoned, Some(at)),
            IssueFacts::ClosedUnresolved(at) => (StageExit::Unknown, Some(at)),
            IssueFacts::Open => (StageExit::Unknown, None),
        }
    }
}
