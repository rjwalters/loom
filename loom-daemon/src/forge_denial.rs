//! Why GitHub refused a call: a missing permission or a rate limit (#10633).
//!
//! GitHub answers both "this credential may not do that" and "slow down" with
//! HTTP `403`, so the status alone cannot tell an agent whether to wait and
//! retry or to stop and name a permission an operator must grant. Before
//! #10633 `forge wait-checks` reported every `403` as `HTTP 403 for <url>`
//! and ended the wait, and a Doctor that hit one on `gh run rerun` could only
//! say "my token got 403". [`classify`] reads the response the way GitHub
//! documents it:
//!
//! | Evidence | [`Denial`] |
//! |---|---|
//! | body/stderr names a secondary limit ("secondary rate limit", "abuse detection", "submitted too quickly"), or a `403`/`429` carrying `Retry-After` | [`Denial::SecondaryRateLimit`] |
//! | `429`, a `403` with `x-ratelimit-remaining: 0`, or "API rate limit exceeded" | [`Denial::RateLimit`] |
//! | `401` / "Bad credentials" | [`Denial::Credential`] |
//! | a `403` naming access ("Resource not accessible by integration", "Must have admin rights", "permission"), or with no message at all | [`Denial::Permission`] |
//! | any other `403` (e.g. "This workflow is already running" on a rerun) | [`Denial::Forbidden`] |
//!
//! Rate limits are checked first: a rate-limited `403` is never a permission
//! problem, and treating it as one would send an operator to App settings for
//! a problem that clears by itself.
//!
//! [`permission_for`] names the GitHub App permission an endpoint needs, so a
//! permission denial says exactly what to grant (the fleet Apps hold
//! `checks: read` but not `statuses: read` or `actions: write`, which is the
//! #10633 root cause).

use crate::forge_call_stats::RateLimitHeaders;

/// Why GitHub refused a call (see the module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    /// The credential lacks a permission (or the repo is outside its
    /// installation). Waiting does not help; an operator grant does.
    Permission,
    /// A secondary (abuse / concurrency) rate limit. Clears after
    /// `Retry-After`; retry later.
    SecondaryRateLimit,
    /// The credential's primary rate-limit pool is empty. Clears at
    /// `x-ratelimit-reset`; retry later.
    RateLimit,
    /// The token itself was refused (`401`, bad credentials).
    Credential,
    /// A `403` that names neither access nor a rate limit — GitHub refusing
    /// the action itself (a rerun of a run still in progress). Read
    /// GitHub's message; neither waiting nor a grant is implied.
    Forbidden,
}

impl Denial {
    /// The short, closed-vocabulary token used in sentinels and notes.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::SecondaryRateLimit => "secondary-rate-limit",
            Self::RateLimit => "rate-limit",
            Self::Credential => "credential",
            Self::Forbidden => "forbidden",
        }
    }

    /// Whether the same call can succeed later without anyone acting.
    #[must_use]
    pub fn is_transient(self) -> bool {
        matches!(self, Self::SecondaryRateLimit | Self::RateLimit)
    }
}

/// Classify a refused call. `text` is everything GitHub and `gh` said about
/// it (response body and/or `gh`'s stderr); `headers` are the response's
/// rate-limit headers when the call used `--include`. `None` for a status
/// that is not a refusal (`2xx`, `304`, `404`, `422`, `5xx`, unknown).
#[must_use]
pub fn classify(
    status: Option<u16>,
    text: &str,
    headers: Option<&RateLimitHeaders>,
) -> Option<Denial> {
    let lower = text.to_ascii_lowercase();
    // Without an `--include` status, `gh` still prints `(HTTP 403)` /
    // `HTTP 403:` on stderr.
    let status = status.or_else(|| crate::gh_invocation::accounting::stderr_status(text));
    if !matches!(status, Some(401 | 403 | 429)) {
        return None;
    }
    let retry_after = headers.is_some_and(|h| h.retry_after_secs.is_some());
    if crate::rate_limit_breaker::evidence::is_secondary_limit(text)
        || (matches!(status, Some(403 | 429)) && retry_after)
    {
        return Some(Denial::SecondaryRateLimit);
    }
    let exhausted = headers.is_some_and(|h| h.remaining == Some(0));
    if status == Some(429) || exhausted || lower.contains("rate limit exceeded") {
        return Some(Denial::RateLimit);
    }
    if status == Some(401) || lower.contains("bad credentials") {
        return Some(Denial::Credential);
    }
    let names_access = ["not accessible by integration", "must have", "permission"]
        .iter()
        .any(|m| lower.contains(m));
    // `gh`'s own `gh:` prefix and `(HTTP 403)` marker are not a message.
    let said_something = lower
        .replace("gh:", "")
        .replace("http 403", "")
        .chars()
        .any(|c| c.is_ascii_alphabetic());
    if names_access || !said_something {
        return Some(Denial::Permission);
    }
    Some(Denial::Forbidden)
}

/// The GitHub App permission `endpoint` needs, when known — so a
/// [`Denial::Permission`] can name what to grant. `endpoint` is a REST path
/// (`repos/o/r/commits/<sha>/status`) and `write` says whether the call
/// mutates.
#[must_use]
pub fn permission_for(endpoint: &str, write: bool) -> Option<&'static str> {
    let path = endpoint.split('?').next().unwrap_or(endpoint);
    let mode =
        |read: &'static str, write_perm: &'static str| Some(if write { write_perm } else { read });
    if path.contains("/actions/") {
        return mode("actions:read", "actions:write");
    }
    if path.contains("/check-runs") || path.contains("/check-suites") {
        return mode("checks:read", "checks:write");
    }
    if path.ends_with("/status") || path.contains("/statuses") {
        return mode("statuses:read", "statuses:write");
    }
    if path.contains("/pulls") {
        return mode("pull_requests:read", "pull_requests:write");
    }
    None
}

/// One line describing a denial for a sentinel or note:
/// `permission (needs statuses:read): Resource not accessible by integration`.
#[must_use]
pub fn describe(denial: Denial, endpoint: &str, write: bool, message: Option<&str>) -> String {
    let mut out = denial.as_str().to_string();
    if denial == Denial::Permission {
        if let Some(p) = permission_for(endpoint, write) {
            out.push_str(&format!(" (needs {p})"));
        }
    }
    if let Some(m) = message.map(str::trim).filter(|m| !m.is_empty()) {
        out.push_str(": ");
        out.push_str(&m.replace(['\n', '\r'], " "));
    }
    out
}

/// GitHub's `message` from a JSON error body, if it has one.
#[must_use]
pub fn body_message(body: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(body.trim())
        .ok()?
        .get("message")?
        .as_str()
        .map(String::from)
}

#[cfg(test)]
#[path = "forge_denial_tests.rs"]
mod tests;
