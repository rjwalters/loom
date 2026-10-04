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
//! # Call identity (#9831)
//!
//! Each row also carries a [`forge_call_stats::CallIdentity`]: the
//! inventoried operation the site named with [`GhInvocation::forge_op`], the
//! provider family (`github` — this facade only ever drives `gh`), the origin
//! host and the `owner/repo` slug. A site that has not named an inventoried
//! operation records `operation = "unknown"` on purpose: [`super::Operation`]
//! is a telemetry name, not an inventory ID, and guessing a mapping here would
//! make the per-operation view lie. Naming the operation is a per-site
//! decision ([`crate::forge_call_stats::ops`]).
//!
//! Recording is local I/O only (the per-host sink) and can never fail the
//! invocation — [`forge_call_stats::record_with_identity`] already
//! guarantees that.

use super::telemetry::Outcome as InvokeOutcome;
use super::GhInvocation;
use crate::forge_call_stats::{self, CallIdentity, Outcome, Pool};
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

/// The value of `--hostname` in a `gh api` argv, if any.
fn hostname_arg(args: &[OsString]) -> Option<String> {
    args.windows(2)
        .find(|w| w[0] == "--hostname")
        .map(|w| w[1].to_string_lossy().into_owned())
}

/// The identity a completed invocation is accounted under: what the site
/// set, with the gaps filled from what the facade itself knows.
///
/// - provider: `github` — `gh` is the GitHub family's client.
/// - origin: the site's, else `gh api --hostname`, else `GH_HOST`, else
///   `github.com` (`gh`'s own default host resolution, in that order).
/// - repo: the site's, else the typed [`super::GhTarget`], else `LOOM_REPO`
///   (the same `GH_REPO` fallback [`GhInvocation::env_plan`] hands the child).
///
/// Never a credential, a header or a body: only these four short tokens, and
/// each still goes through [`forge_call_stats::sanitize`].
#[must_use]
pub fn resolved_identity(inv: &GhInvocation) -> CallIdentity {
    resolved_identity_with(inv, std::env::var("GH_HOST").ok(), std::env::var("LOOM_REPO").ok())
}

fn resolved_identity_with(
    inv: &GhInvocation,
    gh_host: Option<String>,
    loom_repo: Option<String>,
) -> CallIdentity {
    let mut id = inv.identity.clone();
    if id.provider.is_none() {
        id = id.with_provider("github");
    }
    if id.origin.is_none() {
        let origin = hostname_arg(&inv.args)
            .or(gh_host.filter(|h| !h.is_empty()))
            .unwrap_or_else(|| "github.com".to_string());
        id = id.with_origin(&origin);
    }
    if id.repo.is_none() {
        if let Some(repo) = inv.target.slug().or(loom_repo.filter(|r| !r.is_empty())) {
            id = id.with_repo(&repo);
        }
    }
    id
}

/// Record one completed invocation (see the module docs for what is skipped).
/// `captured` is `(stdout, stderr)` for a captured run, `None` for passthrough.
pub(super) fn record(inv: &GhInvocation, outcome: InvokeOutcome, captured: Option<(&[u8], &[u8])>) {
    let caller = inv.operation.as_str();
    let identity = resolved_identity(inv);
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
            forge_call_stats::record_with_identity(
                caller,
                &identity,
                pool,
                classified,
                Some(&resp.ratelimit),
            );
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
    forge_call_stats::record_with_identity(caller, &identity, pool, classified, None);
}

#[cfg(test)]
#[path = "accounting_tests.rs"]
mod tests;
