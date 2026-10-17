//! The reader-only conditional read chain (W4-C's shed, for the ETag store).
//!
//! **The writer is never used for reads** (#12394), so every
//! [`super::fetch_conditional`] — whatever its [`ReadClass`] — gets the same
//! contract as a read through the `gh` facade (`gh_invocation`'s chain):
//!
//! 1. it runs on the reader [`crate::forge_identity::route_read`] chose, with
//!    the site's class, so the router's headroom reserve applies;
//! 2. a rate-limited or refused reader is withdrawn and the router is asked
//!    again, so the next eligible reader serves the read;
//! 3. when no reader is left the read is **shed**: no request, an `o=shed`
//!    accounting row, and the error [`ReadShed`]; the caller skips its pass or
//!    serves from its cache;
//! 4. a 403/404 is an answer only for an item-scoped read from a reader that
//!    recently served the same repo's endpoint family (W6,
//!    [`served::judge_coverage`]). A 403, or a refusal from a reader with no
//!    evidence it sees the repo, withdraws the reader for the repo and goes
//!    on to the next, as in 2; a plain 404 from a reader that sees the repo
//!    is shed. Either way a reader's refusal never turns into an "absent";
//! 5. only a host with no reader pool at all (a target with no repo, or
//!    [`RouteDecision::NoPool`] before any reader was tried) reads on the
//!    writer: it is the only credential there is.

use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;

use super::{reader_failure, run_fetch_with, ConditionalRead, FetchResult, Target};
use crate::forge_bucket_book::Resource;
use crate::forge_identity::{served, Failure, IdentityRole, RouteDecision, RouteRequest};
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

/// A conditional read that was not sent: no reader can serve the repo until
/// about `until` (out of budget, withdrawn, or unable to see it). The caller
/// skips its pass; nothing went to the writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReadShed {
    pub(crate) until: SystemTime,
}

impl std::fmt::Display for ReadShed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let until = chrono::DateTime::<chrono::Utc>::from(self.until)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        write!(
            f,
            "read shed: no reader can serve it until {until}; deferred, not sent to the \
             writer {}",
            crate::gh_invocation::SHED_MARKER
        )
    }
}

impl std::error::Error for ReadShed {}

/// A conditional read that was not sent because the work-finder tick stopped
/// it (#12641). Not a [`ReadShed`]: no reader budget is involved. The caller
/// skips its pass, as for a shed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TickDeadlineReached;

impl std::fmt::Display for TickDeadlineReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "deferred: tick deadline reached")
    }
}

impl std::error::Error for TickDeadlineReached {}

/// Readers one read may try before it stops asking the router.
const MAX_READERS_TRIED: usize = 8;

/// One deferrable conditional read (see the module docs).
///
/// # Errors
/// [`ReadShed`] when no reader can serve the read; otherwise as
/// [`super::fetch_conditional`].
pub(super) fn fetch(
    site: ConditionalRead,
    gh_bin: &Path,
    cwd: Option<&Path>,
    target: &Target,
    url: &str,
    etag: Option<&str>,
    route: &dyn Fn(&RouteRequest<'_>) -> RouteDecision,
) -> Result<FetchResult> {
    let on_writer = || {
        run_fetch_with(
            site,
            gh_bin,
            cwd,
            target,
            url,
            etag,
            None,
            false,
            IdentityRole::Writer,
            None,
        )
    };
    let Some(repo) = target.repo.as_deref() else {
        // An unresolved repo has no reader pool to be out of budget.
        return on_writer();
    };
    let affinity = crate::gh_invocation::url_affinity_key(url);
    let request = RouteRequest {
        owner_repo: repo,
        host: target.host.as_deref(),
        resource: Resource::Core,
        affinity_key: Some(&affinity),
        class: site.class,
    };
    let family = served::endpoint_family(url);
    let answered = |r: &FetchResult| {
        r.0.success() || matches!(r.1.as_ref().map(|h| h.status), Some(200 | 304))
    };
    let mut tried: Vec<String> = Vec::new();
    loop {
        let offered = route(&request);
        let offered = if tried
            .iter()
            .any(|a| a == crate::forge_gateway::GATEWAY_APP_ID)
        {
            crate::forge_gateway::past_gateway(offered, &request, SystemTime::now())
        } else {
            offered
        };
        let (dir, app_id) = match offered {
            RouteDecision::Reader { dir, app_id, .. }
                if !tried.contains(&app_id) && tried.len() < MAX_READERS_TRIED =>
            {
                (dir, app_id)
            }
            // The router offered a reader this read already failed on: no
            // reader is left.
            RouteDecision::Reader { app_id, .. } => {
                let until = SystemTime::now() + crate::forge_read_pool::DEFAULT_WITHDRAWAL;
                return Err(shed(site, gh_bin, target, repo, until, Some(&app_id)));
            }
            RouteDecision::Exhausted { until, .. } => {
                let app = tried.last().map(String::as_str);
                return Err(shed(site, gh_bin, target, repo, until, app));
            }
            // The pool went away after a reader (the gateway) was tried: a
            // withdrawal, not "no pool".
            RouteDecision::NoPool if !tried.is_empty() => {
                let until = SystemTime::now() + crate::forge_read_pool::DEFAULT_WITHDRAWAL;
                let app = tried.last().map(String::as_str);
                return Err(shed(site, gh_bin, target, repo, until, app));
            }
            // No reader pool on this host: the writer is the only credential.
            RouteDecision::NoPool => return on_writer(),
        };
        let bucket = crate::forge_identity::reader_bucket(&app_id, repo);
        let first = run_fetch_with(
            site,
            gh_bin,
            cwd,
            target,
            url,
            etag,
            Some(&dir),
            true,
            IdentityRole::Reader,
            Some(&bucket),
        )?;
        if answered(&first) {
            if matches!(first.1.as_ref().map(|h| h.status), Some(200 | 304)) {
                served::note_reader_served(&app_id, repo, family, SystemTime::now());
            }
            return Ok(first);
        }
        let fault = super::gateway_fault(Some(&app_id), &first);
        if fault.reruns() {
            // 2am#4367 P4: the gateway failed, not the credential: go on to
            // the readers (never the writer); withdraw it only when it
            // signalled the fault itself.
            if fault == crate::forge_gateway::outage::Fault::Withdraw {
                crate::forge_gateway::outage::withdraw_for_outage(site.caller);
            }
            tried.push(app_id);
            continue;
        }
        let Some(failure) = reader_failure(&first) else {
            // Not the credential's fault (a 5xx, a network error): another
            // reader would not help.
            return Ok(first);
        };
        let why = format!("{} {url}", site.caller);
        if failure == Failure::Coverage {
            match served::judge_coverage(
                &app_id,
                repo,
                family,
                site.item_scoped,
                first.1.as_ref().map(|h| h.status),
                &first.2,
                SystemTime::now(),
            ) {
                // W6: this reader covers the repo's `family`, so its 404 is
                // the item's own answer: no withdrawal, no other reader.
                served::CoverageRefusal::Answer => return Ok(first),
                // A plain 404 from a reader that sees the repo: no answer,
                // and the reader stays on the repo's other reads.
                served::CoverageRefusal::Shed => {
                    let until = SystemTime::now() + crate::forge_read_pool::DEFAULT_WITHDRAWAL;
                    return Err(shed(site, gh_bin, target, repo, until, Some(&app_id)));
                }
                served::CoverageRefusal::Withdraw => {}
            }
        }
        // A rate limit, a refused token, or a 403/404 from a reader with no
        // evidence it can see the repo (a "not accessible by integration",
        // an installation that does not include it): withdraw it — for this
        // repo only on coverage — and ask the router again. Returning the
        // refusal would read as "absent" to the caller, and placement is
        // deterministic, so every later read would land on the same blind
        // reader (#12394 review).
        crate::forge_identity::withdraw_after(&app_id, repo, failure, &why);
        tried.push(app_id);
    }
}

/// Book the shed (accounting row, span, rate-limited log) and build its error.
fn shed(
    site: ConditionalRead,
    gh_bin: &Path,
    target: &Target,
    repo: &str,
    until: SystemTime,
    app: Option<&str>,
) -> anyhow::Error {
    let owner = crate::credential_preflight::owner_of_nwo(repo).to_ascii_lowercase();
    GhInvocation::new(
        Operation::new(site.caller),
        AccessIntent::Read,
        GhTarget::None,
        super::FETCH_TIMEOUT,
    )
    .forge_op(site.op)
    .identity_scope(target.host.as_deref(), target.repo.as_deref())
    .program(gh_bin)
    .record_read_shed(site.class, &owner, Resource::Core, until, app);
    anyhow::Error::new(ReadShed { until })
}

#[cfg(test)]
pub(crate) use test_seam::{install_test_route, test_route};

/// Test seam: a fixed router for every [`super::fetch_conditional`] on THIS
/// thread, so a caller several layers up (a page walk) can be driven with the
/// readers out of budget and no real routing tables.
#[cfg(test)]
mod test_seam {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::forge_identity::{RouteDecision, RouteRequest};

    pub(crate) type Route = Rc<dyn Fn(&RouteRequest<'_>) -> RouteDecision>;

    thread_local! {
        static ROUTE: RefCell<Option<Route>> = const { RefCell::new(None) };
    }

    /// Uninstalls the route when dropped.
    pub(crate) struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            ROUTE.with(|r| *r.borrow_mut() = None);
        }
    }

    #[must_use]
    pub(crate) fn install_test_route(
        route: impl Fn(&RouteRequest<'_>) -> RouteDecision + 'static,
    ) -> Guard {
        ROUTE.with(|r| *r.borrow_mut() = Some(Rc::new(route)));
        Guard
    }

    pub(crate) fn test_route() -> Option<Route> {
        ROUTE.with(|r| r.borrow().clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "deferrable_tests.rs"]
mod tests;
