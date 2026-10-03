//! Own-call accounting at the choke point (#10089).
//!
//! Before this, an `invoke github` span was the only trace a facade execution
//! left: [`crate::forge_call_stats`] — the ledger `loom-daemon status` and the
//! rate-limit breaker's own-versus-external attribution line read — only saw
//! the three hand-instrumented ETag callers, so on a busy host ~98% of the
//! daemon's own spend was booked as "external". Every [`GhInvocation`]
//! execution that reached `gh` now records exactly one row, keyed by its
//! stable [`super::Operation`] name, so a migrated site is counted the moment
//! it routes through the facade.
//!
//! # What is (not) recorded
//!
//! - A **spawn failure** and a **launcher routing refusal** are not recorded:
//!   neither sent a request, so neither spent budget.
//! - Every other completion is one row. A `--paginate` execution is still one
//!   row although it may have issued several requests — the ledger is a lower
//!   bound for paginated callers, never an over-count.
//! - `gh api rate_limit` is free on GitHub's side and is booked to
//!   [`Pool::Other`] so it never inflates the core pool's "own" figure.
//!
//! Recording is local I/O only (the per-host sink) and can never fail the
//! invocation — [`forge_call_stats::record`] already guarantees that.

use super::telemetry::Outcome as InvokeOutcome;
use super::GhInvocation;
use crate::forge_call_stats::{self, Outcome, Pool};
use std::ffi::OsString;

/// The pool an invocation spends, from its argv alone (no response needed).
///
/// `gh issue …` / `gh pr …` / `gh repo …` are GraphQL-backed; `gh api` is
/// REST (core) unless it is `api graphql`, a `search/…` path (search pool),
/// or the free `rate_limit` endpoint; `gh search …` spends the search pool.
#[must_use]
pub fn static_pool(args: &[OsString]) -> Pool {
    let words: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    match words.first().map(String::as_str) {
        Some("api") => api_pool(&words[1..]),
        Some("issue" | "pr" | "repo" | "project") => Pool::Graphql,
        Some("search") => Pool::Search,
        Some("release" | "run" | "workflow" | "label" | "secret" | "variable" | "cache") => {
            Pool::Core
        }
        _ => Pool::Other,
    }
}

/// The endpoint is the first `gh api` argument that is not a flag or a flag's
/// value. Flags taking a value are skipped with it.
fn api_pool(rest: &[String]) -> Pool {
    const VALUED: &[&str] = &[
        "-H",
        "--header",
        "-f",
        "--raw-field",
        "-F",
        "--field",
        "-q",
        "--jq",
        "-t",
        "--template",
        "-X",
        "--method",
        "--hostname",
        "--input",
        "--cache",
        "-p",
        "--preview",
    ];
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        if VALUED.contains(&a) {
            i += 2;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        let path = a.trim_start_matches('/');
        return if path == "graphql" {
            Pool::Graphql
        } else if path == "rate_limit" {
            Pool::Other
        } else if path.starts_with("search/") {
            Pool::Search
        } else {
            Pool::Core
        };
    }
    Pool::Other
}

/// Whether the invocation asked `gh api` to print the response headers.
fn wants_headers(args: &[OsString]) -> bool {
    args.first().is_some_and(|a| a == "api") && args.iter().any(|a| a == "-i" || a == "--include")
}

/// Record one completed invocation (see the module docs for what is skipped).
/// `captured` is `(stdout, stderr)` for a captured run, `None` for passthrough.
pub(super) fn record(inv: &GhInvocation, outcome: InvokeOutcome, captured: Option<(&[u8], &[u8])>) {
    let caller = inv.operation.as_str();
    let pool = static_pool(&inv.args);
    let stderr = captured.map(|(_, e)| String::from_utf8_lossy(e).into_owned());
    let stderr = stderr.as_deref().unwrap_or_default();

    // `gh api --include`: the status line and the free `x-ratelimit-*`
    // headers are on stdout — the most precise classification available
    // (a `304` is free, and gh exits non-zero on it).
    if wants_headers(&inv.args) {
        let response = captured.and_then(|(out, _)| {
            crate::forge_listing::parse_http_response(&String::from_utf8_lossy(out))
        });
        if let Some(resp) = response {
            let (_, classified) =
                forge_call_stats::classify(Some(&resp), outcome == InvokeOutcome::Ok, stderr);
            let pool = resp
                .ratelimit
                .resource
                .as_deref()
                .map_or(pool, Pool::from_resource);
            forge_call_stats::record(caller, pool, classified, Some(&resp.ratelimit));
            return;
        }
    }

    let classified = match outcome {
        InvokeOutcome::SpawnFailed | InvokeOutcome::RoutingRefused => return,
        InvokeOutcome::Ok => Outcome::Ok,
        InvokeOutcome::ExitNonzero if crate::rate_limit_breaker::indicates_rate_limit(stderr) => {
            Outcome::RateLimited
        }
        InvokeOutcome::ExitNonzero
        | InvokeOutcome::Signaled
        | InvokeOutcome::Timeout
        | InvokeOutcome::CollectFailed => Outcome::Error,
    };
    forge_call_stats::record(caller, pool, classified, None);
}

#[cfg(test)]
#[path = "accounting_tests.rs"]
mod tests;
