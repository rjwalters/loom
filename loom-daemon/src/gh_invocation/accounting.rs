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
//!   row although it may have issued several requests; since W1 the row
//!   carries its page count when `--include` exposes it (`pg`), or flags it
//!   unknown (`pu`), so the per-bucket "charged" figure can count pages.
//! - `gh api rate_limit` is free on GitHub's side and is booked to
//!   [`Pool::Other`] so it never inflates the core pool's "own" figure.
//!
//! # Bucket attribution (W1)
//!
//! Each row also names the credential it spent ([`cred_of`]: account,
//! installation owner, kind), the billed resource, and where its repo came
//! from ([`RepoOrigin`]). A row an App credential served with headers
//! updates [`crate::forge_bucket_book`], and every row adds to the
//! `loom.forge.calls` counter ([`crate::observability::ops::forge_calls`]).
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

#[path = "attribution.rs"]
mod attribution;
pub use attribution::{
    cred_of, cred_of_with, cwd_route_disagrees, pages, remote_repo, CredAttr, RepoOrigin,
};

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
///   (the same `GH_REPO` fallback [`GhInvocation::env_plan`] hands the child),
///   else (W1) the working directory's `origin` remote — a memoised local
///   read, never a forge call ([`remote_repo`]).
/// - role (#9872): `reader` / `writer` / `writer-fallback` — which identity,
///   so which rate-limit pool, served the call.
///
/// Never a credential, a header or a body: only these short tokens, and
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
    resolve_with(inv, gh_host, loom_repo).0
}

/// [`resolved_identity_with`] plus where the repo came from (`ro`).
fn resolve_with(
    inv: &GhInvocation,
    gh_host: Option<String>,
    loom_repo: Option<String>,
) -> (CallIdentity, RepoOrigin) {
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
    let mut ro = RepoOrigin::Site;
    if id.repo.is_none() {
        let filled = inv
            .target
            .slug()
            .map(|r| (r, RepoOrigin::Target))
            .or_else(|| {
                loom_repo
                    .filter(|r| !r.is_empty())
                    .map(|r| (r, RepoOrigin::LoomRepo))
            })
            .or_else(|| {
                inv.cwd
                    .as_deref()
                    .and_then(remote_repo)
                    .map(|r| (r, RepoOrigin::Remote))
            });
        ro = RepoOrigin::None;
        if let Some((repo, origin)) = filled {
            id = id.with_repo(&repo);
            if id.repo.is_some() {
                ro = origin;
            }
        }
    }
    (id.with_role(super::reader_route::role_of(inv).as_str()), ro)
}

/// Whether the argv is the free `gh api rate_limit` endpoint.
fn is_rate_limit_probe(args: &[OsString]) -> bool {
    args.first().is_some_and(|a| a == "api") && static_pool(args) == Pool::Other
}

/// Record one completed invocation (see the module docs for what is skipped).
/// `captured` is `(stdout, stderr)` for a captured run, `None` for passthrough.
pub(super) fn record(inv: &GhInvocation, outcome: InvokeOutcome, captured: Option<(&[u8], &[u8])>) {
    if matches!(outcome, InvokeOutcome::SpawnFailed | InvokeOutcome::RoutingRefused) {
        return;
    }
    let caller = inv.operation.as_str();
    let (identity, ro) =
        resolve_with(inv, std::env::var("GH_HOST").ok(), std::env::var("LOOM_REPO").ok());
    let static_p = static_pool(&inv.args);
    let stderr = captured.map(|(_, e)| String::from_utf8_lossy(e).into_owned());
    let stderr = stderr.as_deref().unwrap_or_default();
    let include = wants_headers(&inv.args);
    let probe = is_rate_limit_probe(&inv.args);

    // `gh api --include`: the status line and the free `x-ratelimit-*`
    // headers are on stdout — the most precise classification available
    // (a `304` is free, and gh exits non-zero on it).
    let response = captured.filter(|_| include).and_then(|(out, _)| {
        crate::forge_listing::parse_http_response(&String::from_utf8_lossy(out))
    });
    let (pool, classified, headers) = if let Some(resp) = &response {
        let (_, classified) =
            forge_call_stats::classify(Some(resp), outcome == InvokeOutcome::Ok, stderr);
        // `rate_limit` is free whatever resource its headers name.
        let pool = match resp.ratelimit.resource.as_deref() {
            Some(r) if !probe => Pool::from_resource(r),
            _ => static_p,
        };
        (pool, classified, Some(&resp.ratelimit))
    } else {
        let classified = match outcome {
            InvokeOutcome::Ok => Outcome::Ok,
            InvokeOutcome::ExitNonzero
                if crate::rate_limit_breaker::indicates_rate_limit(stderr) =>
            {
                Outcome::RateLimited
            }
            _ => Outcome::Error,
        };
        (static_p, classified, None)
    };

    let cred = cred_of(inv);
    let resource = headers
        .and_then(|h| h.resource.clone())
        .filter(|_| !probe)
        .map_or_else(|| pool.as_str().to_string(), |r| r.trim().to_ascii_lowercase());
    let (pg, pu) = pages(&inv.args, include, captured.map(|(out, _)| out));
    let rd = match (ro, inv.cwd.as_deref(), identity.repo.as_deref()) {
        (RepoOrigin::Remote, Some(cwd), Some(repo)) if cwd_route_disagrees(cwd, repo) => {
            forge_call_stats::buckets::bump_cwd_route_disagree();
            Some(true)
        }
        _ => None,
    };
    let attribution = forge_call_stats::CallAttribution {
        ro: Some(ro.as_str().to_string()),
        ca: forge_call_stats::sanitize(&cred.account),
        co: cred.owner.as_deref().and_then(forge_call_stats::sanitize),
        tk: Some(cred.kind.to_string()),
        rr: forge_call_stats::sanitize(&resource),
        pg,
        pu,
        rd,
    };
    forge_call_stats::record_attributed(caller, &identity, pool, classified, headers, &attribution);

    if let (Some(h), Some(owner), false) = (headers, cred.owner.as_deref(), probe) {
        let bucket = h
            .resource
            .as_deref()
            .and_then(crate::forge_bucket_book::Resource::parse);
        if let (true, Some(resource)) = (cred.is_app(), bucket) {
            crate::forge_bucket_book::observe(
                crate::forge_bucket_book::BucketKey::new(&cred.account, owner, resource),
                h,
                crate::forge_bucket_book::Source::Header,
            );
        }
    }

    record_metric(caller, &identity, &cred, &resource, classified, pg);
}

/// Add the row to `loom.forge.calls` (a no-op without an ops sink).
fn record_metric(
    caller: &'static str,
    identity: &CallIdentity,
    cred: &CredAttr,
    resource: &str,
    outcome: Outcome,
    pg: Option<u32>,
) {
    use crate::observability::ops::forge_calls::{self, CallLabels, CallOutcome};
    let unknown = || forge_call_stats::UNKNOWN_OPERATION.to_string();
    let target_owner = identity
        .repo
        .as_deref()
        .map(crate::credential_preflight::owner_of_nwo)
        .filter(|o| !o.is_empty())
        .map_or_else(unknown, str::to_ascii_lowercase);
    let labels = CallLabels {
        caller: caller.to_string(),
        op: identity.operation.clone().unwrap_or_else(unknown),
        role: identity.role.clone().unwrap_or_else(unknown),
        account: cred.account.clone(),
        cred_owner: cred.owner.clone().unwrap_or_else(unknown),
        target_owner,
        resource: resource.to_string(),
        outcome: match outcome {
            Outcome::Ok => CallOutcome::Ok,
            Outcome::NotModified => CallOutcome::NotModified,
            Outcome::RateLimited => CallOutcome::RateLimited,
            Outcome::Error => CallOutcome::Error,
        },
    };
    forge_calls::record(labels, u64::from(pg.unwrap_or(1).max(1)));
}

#[cfg(test)]
#[path = "accounting_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "accounting_w1_tests.rs"]
mod w1_tests;
