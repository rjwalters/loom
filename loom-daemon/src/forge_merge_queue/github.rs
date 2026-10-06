//! GitHub implementation of [`QueueApi`] (#10255).
//!
//! Every call goes through the counted [`GhInvocation`] facade (#9985/#10089),
//! so it gets Loom's authenticated forge routing — `gh` is resolved by the
//! single resolver, never hand-picked here.
//!
//! # The GitHub contract this encodes (verified by schema introspection, see
//! the #10255 Curator comment)
//!
//! - `enqueuePullRequest(input: { pullRequestId, expectedHeadOid, jump })` —
//!   takes the PR **node id**, not its number. `expectedHeadOid` is optional
//!   in the schema; this module declares it `GitObjectID!` so it can never be
//!   omitted. `jump` is never sent.
//! - `dequeuePullRequest(input: { id })` — no expected-head field.
//! - Status comes from `PullRequest { id state headRefOid mergeQueueEntry {
//!   state position headCommit { oid } } }`.
//!
//! Error texts are classified best-effort (GitHub publishes no stable GraphQL
//! error codes for these mutations); anything unrecognized is
//! [`QueueError::Forge`], never a success.

use std::time::Duration;

use serde::Deserialize;

use super::ops::{
    DequeueAck, EnqueueAck, PrQueueStatus, PrState, QueueApi, QueueEntry, QueueError,
};
use crate::cmd_out::CmdOutcome;
use crate::forge_egress::report::redact;
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

/// Per-call deadline.
const QUEUE_CALL_TIMEOUT: Duration = Duration::from_secs(60);

// Plain multi-line literals on purpose: a `\` line continuation strips the
// next line's leading whitespace, which fused `headRefOid` and
// `mergeQueueEntry` into one unknown field in the first draft.
pub const STATUS_QUERY: &str = "query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      id state headRefOid
      mergeQueueEntry { state position headCommit { oid } }
    }
  }
}";

pub const ENQUEUE_MUTATION: &str = "mutation($pullRequestId: ID!, $expectedHeadOid: GitObjectID!) {
  enqueuePullRequest(input: { pullRequestId: $pullRequestId, expectedHeadOid: $expectedHeadOid }) {
    mergeQueueEntry { state position }
  }
}";

pub const DEQUEUE_MUTATION: &str = "mutation($id: ID!) {
  dequeuePullRequest(input: { id: $id }) {
    mergeQueueEntry { state }
  }
}";

/// The class a forge failure text falls into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    RateLimited,
    HeadMismatch,
    AlreadyQueued,
    NotQueued,
    Denied,
    QueueUnavailable,
    NotFound,
    Other,
}

/// Classify a `gh` failure text (stderr + stdout). Order matters: rate limits
/// first (a 403 secondary-rate-limit is NOT a permissions verdict), then the
/// specific queue states, then generic denial.
#[must_use]
pub fn classify_failure(text: &str) -> FailureClass {
    let t = text.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| t.contains(n));
    if has(&[
        "rate limit",
        "rate_limited",
        "secondary rate",
        "abuse detection",
    ]) {
        FailureClass::RateLimited
    } else if has(&[
        "expectedheadoid",
        "expected head",
        "head branch was modified",
        "head out of date",
        "head sha",
    ]) {
        FailureClass::HeadMismatch
    } else if has(&[
        "already in the merge queue",
        "already queued",
        "already enqueued",
    ]) {
        FailureClass::AlreadyQueued
    } else if has(&["not in the merge queue", "not queued", "not enqueued"]) {
        FailureClass::NotQueued
    } else if has(&[
        "merge queue is not enabled",
        "merge queue not enabled",
        "does not have a merge queue",
        "no merge queue",
        "merge queue is disabled",
    ]) {
        FailureClass::QueueUnavailable
    } else if has(&[
        "forbidden",
        "resource not accessible",
        "must have",
        "not authorized",
        "permission",
        "http 401",
        "http 403",
        "bad credentials",
    ]) {
        FailureClass::Denied
    } else if has(&["could not resolve to", "not found", "http 404"]) {
        FailureClass::NotFound
    } else {
        FailureClass::Other
    }
}

/// First non-empty line, token-redacted — the only form of forge text that
/// reaches a diagnostic.
#[must_use]
pub fn safe_detail(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no error text");
    redact(line)
}

fn error_for(class: FailureClass, text: &str, pr: u32, approved: Option<&str>) -> QueueError {
    let detail = safe_detail(text);
    match class {
        FailureClass::RateLimited => QueueError::RateLimited { detail },
        FailureClass::HeadMismatch => QueueError::HeadMismatch {
            pr,
            approved: approved.unwrap_or("(none)").to_string(),
            actual: None,
        },
        FailureClass::Denied => QueueError::Denied { detail },
        FailureClass::QueueUnavailable => QueueError::QueueUnavailable { detail },
        FailureClass::NotFound => QueueError::NotFound { pr },
        FailureClass::AlreadyQueued | FailureClass::NotQueued | FailureClass::Other => {
            QueueError::Forge { detail }
        }
    }
}

/// The GitHub [`QueueApi`].
pub struct GhQueueApi {
    gh: String,
    owner: String,
    name: String,
}

impl GhQueueApi {
    /// # Errors
    ///
    /// When `nwo` is not `owner/repo`.
    pub fn new(gh: &str, nwo: &str) -> Result<Self, QueueError> {
        match GhTarget::repo(nwo) {
            Ok(GhTarget::Repo { owner, repo }) => Ok(Self {
                gh: gh.to_string(),
                owner,
                name: repo,
            }),
            _ => Err(QueueError::Forge {
                detail: "repository must be an `owner/repo` slug".to_string(),
            }),
        }
    }

    fn graphql(&self, op: &'static str, intent: AccessIntent, args: &[String]) -> CmdOutcome {
        let target = GhTarget::Repo {
            owner: self.owner.clone(),
            repo: self.name.clone(),
        };
        GhInvocation::new(Operation::new(op), intent, target, QUEUE_CALL_TIMEOUT)
            .program(&self.gh)
            .args(["api", "graphql"])
            .args(args)
            .run()
    }

    /// Run a GraphQL call to a parsed `data` value, or the failure text.
    fn run(
        &self,
        op: &'static str,
        intent: AccessIntent,
        args: &[String],
    ) -> Result<serde_json::Value, String> {
        let out = self.graphql(op, intent, args);
        let combined = format!("{}\n{}", out.stderr_trimmed(), out.stdout_trimmed());
        if !out.succeeded() {
            return Err(combined);
        }
        let v: serde_json::Value = serde_json::from_str(&out.stdout_trimmed())
            .map_err(|e| format!("unparseable response: {e}"))?;
        if let Some(errs) = v.get("errors").filter(|e| !e.is_null()) {
            return Err(errs.to_string());
        }
        Ok(v.get("data").cloned().unwrap_or(serde_json::Value::Null))
    }
}

#[derive(Deserialize)]
struct RawCommit {
    oid: Option<String>,
}

#[derive(Deserialize)]
struct RawEntry {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    position: Option<u32>,
    #[serde(default, rename = "headCommit")]
    head_commit: Option<RawCommit>,
}

#[derive(Deserialize)]
struct RawPr {
    id: String,
    state: String,
    #[serde(rename = "headRefOid")]
    head_ref_oid: String,
    #[serde(default, rename = "mergeQueueEntry")]
    merge_queue_entry: Option<RawEntry>,
}

/// Decode a status query's `data` into [`PrQueueStatus`].
///
/// # Errors
///
/// [`QueueError::NotFound`] for a null PR, [`QueueError::Forge`] for a shape
/// it cannot read.
pub fn parse_status(pr: u32, data: &serde_json::Value) -> Result<PrQueueStatus, QueueError> {
    let raw = data
        .pointer("/repository/pullRequest")
        .filter(|v| !v.is_null())
        .ok_or(QueueError::NotFound { pr })?;
    let raw: RawPr = serde_json::from_value(raw.clone()).map_err(|e| QueueError::Forge {
        detail: format!("unexpected status shape: {e}"),
    })?;
    let state = match raw.state.as_str() {
        "OPEN" => PrState::Open,
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        other => {
            return Err(QueueError::Forge {
                detail: format!("unknown PR state {other:?}"),
            })
        }
    };
    Ok(PrQueueStatus {
        number: pr,
        node_id: raw.id,
        state,
        head_oid: raw.head_ref_oid,
        entry: raw.merge_queue_entry.map(|e| QueueEntry {
            state: e.state.unwrap_or_default(),
            position: e.position,
            head_oid: e.head_commit.and_then(|c| c.oid),
        }),
    })
}

impl QueueApi for GhQueueApi {
    fn status(&self, pr: u32) -> Result<PrQueueStatus, QueueError> {
        let args = vec![
            "-f".to_string(),
            format!("query={STATUS_QUERY}"),
            "-f".to_string(),
            format!("owner={}", self.owner),
            "-f".to_string(),
            format!("name={}", self.name),
            "-F".to_string(),
            format!("number={pr}"),
        ];
        match self.run("merge_queue.status", AccessIntent::Read, &args) {
            Ok(data) => parse_status(pr, &data),
            Err(text) => Err(error_for(classify_failure(&text), &text, pr, None)),
        }
    }

    fn enqueue(
        &self,
        pr: u32,
        node_id: &str,
        expected_head_oid: &str,
    ) -> Result<EnqueueAck, QueueError> {
        let args = vec![
            "-f".to_string(),
            format!("query={ENQUEUE_MUTATION}"),
            "-f".to_string(),
            format!("pullRequestId={node_id}"),
            "-f".to_string(),
            format!("expectedHeadOid={expected_head_oid}"),
        ];
        match self.run("merge_queue.enqueue", AccessIntent::Write, &args) {
            Ok(data) => Ok(EnqueueAck::Enqueued {
                position: data
                    .pointer("/enqueuePullRequest/mergeQueueEntry/position")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|p| u32::try_from(p).ok()),
            }),
            Err(text) => match classify_failure(&text) {
                FailureClass::AlreadyQueued => Ok(EnqueueAck::AlreadyQueued),
                class => Err(error_for(class, &text, pr, Some(expected_head_oid))),
            },
        }
    }

    fn dequeue(&self, pr: u32, node_id: &str) -> Result<DequeueAck, QueueError> {
        let args = vec![
            "-f".to_string(),
            format!("query={DEQUEUE_MUTATION}"),
            "-f".to_string(),
            format!("id={node_id}"),
        ];
        match self.run("merge_queue.dequeue", AccessIntent::Write, &args) {
            Ok(_) => Ok(DequeueAck::Dequeued),
            Err(text) => match classify_failure(&text) {
                FailureClass::NotQueued => Ok(DequeueAck::NotQueued),
                class => Err(error_for(class, &text, pr, None)),
            },
        }
    }
}
