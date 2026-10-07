//! HTTP truth and credential identity for the `invoke github` span (#10343).
//!
//! [`super::accounting::record`] already works out, for every facade
//! execution, what GitHub saw: the HTTP status of a `gh api --include` call,
//! whether it was a free `304`, how many requests it stood for, the billed
//! resource and the credential it spent. Before #10343 all of that went to
//! the ledger and `loom.forge.calls` only, and the span carried process truth
//! (`github.outcome`, the exit code). [`Billing`] is the same facts, handed
//! to the span so a trace query can join a span to the per-bucket
//! `github.ratelimit.*` gauges and count what a bucket was billed.
//!
//! # Never guessed
//!
//! - `github.http.status` comes from the first `--include` status block
//!   (`headers`), else an HTTP marker `gh` printed on stderr — `(HTTP 404)`
//!   or `HTTP 403: …` (`stderr`) — else it is `unknown` (`none`). A
//!   passthrough or credential-helper run never has one.
//! - `github.http.requests` is the page count of a `--paginate --include`
//!   call, `1` for a single `--include` block, `2` for `run download`, `0`
//!   when nothing was sent, and `unknown` otherwise.
//!
//! Every value is a short closed-vocabulary token, a number, or a label the
//! ledger already sanitizes (`app-<id>`, a validated owner) — never a token,
//! token hash or path.

use super::accounting::CredAttr;
use crate::forge_call_stats::{self, Outcome};

/// Where [`Billing::status`] came from (`github.http.source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillingSource {
    /// The `gh api --include` status line on stdout.
    Headers,
    /// `gh`'s formatted error on stderr.
    Stderr,
    /// No status is known.
    None,
}

impl BillingSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Headers => "headers",
            Self::Stderr => "stderr",
            Self::None => "none",
        }
    }
}

/// What GitHub billed for the call (`github.billing`): the ledger's
/// classification, plus `not_sent` for an execution that never reached it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BillingClass {
    Ok,
    NotModified,
    RateLimited,
    Error,
    NotSent,
}

impl BillingClass {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NotModified => "not_modified",
            Self::RateLimited => "rate_limited",
            Self::Error => "error",
            Self::NotSent => "not_sent",
        }
    }

    /// Every value, for vocabulary tests and docs.
    pub const ALL: [Self; 5] = [
        Self::Ok,
        Self::NotModified,
        Self::RateLimited,
        Self::Error,
        Self::NotSent,
    ];
}

impl From<Outcome> for BillingClass {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Ok => Self::Ok,
            Outcome::NotModified => Self::NotModified,
            Outcome::RateLimited => Self::RateLimited,
            Outcome::Error => Self::Error,
            Outcome::Shed => Self::NotSent,
        }
    }
}

/// One execution's billing facts (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Billing {
    pub status: Option<u16>,
    pub source: BillingSource,
    pub requests: Option<u32>,
    pub class: BillingClass,
    /// `core` / `graphql` / `search` / `other`.
    pub resource: String,
    /// `app-<id>`, `app-unknown`, `env-token` or `ambient`.
    pub account: String,
    /// The installation owner (lowercased), or `-`.
    pub cred_owner: String,
    /// `reader`, `writer`, `writer-fallback` or `unknown`.
    pub role: String,
    /// The `owner/repo` the ledger booked the call under (#10752): the
    /// site's, the typed target, `LOOM_REPO` or the working directory's
    /// remote ([`super::accounting::resolved_identity`]). `None` when nothing
    /// named one, or nothing was sent. The span's `github.repo`.
    pub repo: Option<String>,
}

/// A label value through the ledger's sanitizer and narrowed to a short
/// `[a-z0-9_-]` token (no `/`, so never a path), else `fallback`.
fn clean(value: &str, fallback: &str) -> String {
    forge_call_stats::sanitize(value)
        .map(|v| v.to_ascii_lowercase())
        .filter(|v| {
            v.len() <= 64
                && v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
        .unwrap_or_else(|| fallback.to_string())
}

impl Billing {
    /// An execution that sent nothing (spawn failure, a launcher refusal).
    #[must_use]
    pub fn not_sent(resource: &str, cred: &CredAttr, role: &str) -> Self {
        Self::new(None, None, Some(0), BillingClass::NotSent, resource, cred, role)
    }

    /// A completed execution. `header_status` is the `--include` status
    /// line's, if any; `stderr` is the captured stderr (empty for a
    /// passthrough run, which therefore never yields a status).
    #[must_use]
    pub fn sent(
        header_status: Option<u16>,
        stderr: &str,
        requests: Option<u32>,
        class: BillingClass,
        resource: &str,
        cred: &CredAttr,
        role: &str,
    ) -> Self {
        Self::new(
            header_status,
            super::accounting::stderr_status(stderr),
            requests,
            class,
            resource,
            cred,
            role,
        )
    }

    fn new(
        header_status: Option<u16>,
        stderr_status: Option<u16>,
        requests: Option<u32>,
        class: BillingClass,
        resource: &str,
        cred: &CredAttr,
        role: &str,
    ) -> Self {
        let (status, source) = match (header_status, stderr_status) {
            (Some(s), _) => (Some(s), BillingSource::Headers),
            (None, Some(s)) => (Some(s), BillingSource::Stderr),
            (None, None) => (None, BillingSource::None),
        };
        Self {
            status,
            source,
            requests,
            class,
            resource: clean(resource, "other"),
            account: clean(&cred.account, "unknown"),
            cred_owner: clean(cred.owner.as_deref().unwrap_or("-"), "-"),
            role: clean(role, "unknown"),
            repo: None,
        }
    }

    /// These facts with the ledger's resolved `owner/repo` (#10752). A value
    /// that is not a plain `owner/repo` slug is dropped, never exported.
    #[must_use]
    pub fn with_repo(mut self, repo: Option<&str>) -> Self {
        self.repo = repo.and_then(forge_call_stats::sanitize).filter(|r| {
            let mut parts = r.split('/');
            let slug_part = |p: Option<&str>| {
                p.is_some_and(|p| {
                    !p.is_empty()
                        && p.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
                })
            };
            r.len() <= 140
                && slug_part(parts.next())
                && slug_part(parts.next())
                && parts.next().is_none()
        });
        self
    }

    /// The span attributes, every key always present (`unknown` when not
    /// known) — the nine `github.*` keys of
    /// [`super::telemetry::SPAN_ATTRIBUTE_KEYS`] this module owns.
    #[must_use]
    pub fn attributes(&self) -> [(&'static str, String); 9] {
        let unknown = || "unknown".to_string();
        [
            ("github.http.status", self.status.map_or_else(unknown, |s| s.to_string())),
            (
                "github.http.not_modified",
                self.status.map_or_else(unknown, |s| (s == 304).to_string()),
            ),
            ("github.http.requests", self.requests.map_or_else(unknown, |n| n.to_string())),
            ("github.http.source", self.source.as_str().to_string()),
            ("github.billing", self.class.as_str().to_string()),
            ("github.resource", self.resource.clone()),
            ("github.account", self.account.clone()),
            ("github.cred_owner", self.cred_owner.clone()),
            ("github.role", self.role.clone()),
        ]
    }
}

/// The request count a completed call stands for, from the ledger's page
/// facts: `(pg, pu)` from [`super::accounting::pages`], and whether a single
/// `--include` status block was parsed.
#[must_use]
pub fn requests(pg: Option<u32>, pu: Option<bool>, header_block: bool) -> Option<u32> {
    if pu == Some(true) {
        return None;
    }
    pg.or_else(|| header_block.then_some(1))
}

#[cfg(test)]
#[path = "billing_tests.rs"]
mod tests;
