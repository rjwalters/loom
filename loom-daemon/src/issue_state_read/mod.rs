//! Trustworthy issue-state reads for `loom-daemon stashes` (#8876, Part A).
//!
//! `stashes` classifies a quarantine stash as retirable only when the issue
//! named by its label is closed. In a standalone CLI process the daemon's
//! per-owner credentials do not exist (they live in the daemon's in-process
//! registries), so on a managed private repo the read ran under whatever
//! ambient `gh` login the shell had and answered 404.
//!
//! The fix is a narrow, read-only IPC operation ([`crate::types::Request::ReadIssueState`]):
//! the daemon, which owns the credentials and the forge routing, performs the
//! one `gh issue view` through the [`crate::gh_invocation::GhInvocation`]
//! facade and returns only [`ReadOutcome`] plus a non-secret route label. No
//! token, profile directory or endpoint ever crosses the socket, and no
//! credential is published into or discovered from a checkout.
//!
//! - [`daemon`]: the handler (validates root, slug and issue; runs the read).
//! - [`client`]: socket trust checks, the one-frame client, and
//!   [`client::RoutedLookup`], which picks the route once per run.
//! - [`standalone`]: the no-daemon path: the existing ambient facade read.
//!
//! Fail-closed throughout: every refusal, mismatch or failure is
//! [`ReadOutcome::Unknown`], which `stash_retirement` never retires on.

pub mod client;
pub mod daemon;
pub mod standalone;

use serde::{Deserialize, Serialize};

/// What a read concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadOutcome {
    Open,
    Closed,
    /// The state could not be established. Never "safe to retire".
    Unknown,
    /// The daemon does not manage this root: the caller may use its
    /// standalone path. Distinct from `Unknown` so a managed repo whose read
    /// failed can never fall through to ambient credentials.
    NotManaged,
}

impl ReadOutcome {
    /// `Some(true)` closed, `Some(false)` open, `None` otherwise.
    #[must_use]
    pub fn is_closed(self) -> Option<bool> {
        match self {
            Self::Closed => Some(true),
            Self::Open => Some(false),
            Self::Unknown | Self::NotManaged => None,
        }
    }
}

/// The wire reply to `ReadIssueState`. Carries no credential: `route` is a
/// fixed-vocabulary label and `reason` a fixed-vocabulary refusal code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssueStateReply {
    pub outcome: ReadOutcome,
    pub route: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl IssueStateReply {
    #[must_use]
    pub fn new(outcome: ReadOutcome, route: impl Into<String>) -> Self {
        Self {
            outcome,
            route: route.into(),
            reason: None,
        }
    }

    #[must_use]
    pub fn with_reason(mut self, reason: &str) -> Self {
        self.reason = Some(reason.to_string());
        self
    }
}

/// Interpret `gh issue view --json state -q .state` stdout.
#[must_use]
pub fn parse_state(stdout: &[u8]) -> ReadOutcome {
    match String::from_utf8_lossy(stdout)
        .trim()
        .to_uppercase()
        .as_str()
    {
        "CLOSED" => ReadOutcome::Closed,
        "OPEN" => ReadOutcome::Open,
        _ => ReadOutcome::Unknown,
    }
}

/// Strict `owner/repo` shape: two non-empty segments of `[A-Za-z0-9._-]`,
/// neither starting with `-` or `.`, each at most 100 characters. Rejects
/// anything that could smuggle a flag, a path or a host into `gh`.
#[must_use]
pub fn valid_slug(slug: &str) -> bool {
    let mut parts = slug.split('/');
    let (Some(owner), Some(repo), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    [owner, repo].iter().all(|seg| {
        !seg.is_empty()
            && seg.len() <= 100
            && !seg.starts_with(['-', '.'])
            && seg
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
