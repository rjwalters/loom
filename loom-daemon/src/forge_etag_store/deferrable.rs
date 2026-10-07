//! Deferrable conditional reads (W4-C's shed, for the ETag store).
//!
//! [`super::fetch_conditional`] is a [`ReadClass::Gate`] read by default: a
//! reader that cannot serve it is withdrawn and the same request is retried
//! on the writer. That is right for a read that gates a decision and wrong
//! for housekeeping: a paginated listing that falls back page by page spends
//! the writer bucket the readers exist to protect.
//!
//! A site that opts in with [`super::ConditionalRead::deferrable`] gets the
//! same contract as a deferrable read through the `gh` facade
//! (`gh_invocation`'s class-aware chain):
//!
//! 1. it runs on the reader [`crate::forge_identity::route_read`] chose, with
//!    the site's class, so the router's headroom reserve applies;
//! 2. a rate-limited or refused reader is withdrawn and the router is asked
//!    again, so the next eligible reader serves the read;
//! 3. when the readers are out of **budget** ([`ExhaustCause::Budget`], or
//!    the last reader tried answered with a rate limit) the read is **shed**:
//!    no request, an `o=shed` accounting row, and the error [`ReadShed`];
//! 4. only when there is no reader pool at all, or the readers are out for a
//!    reason that is not budget (a coverage miss, a stale or refused token),
//!    does the writer serve it. Shedding there would stop the caller's
//!    housekeeping for as long as the condition lasts.
//!
//! `LOOM_READ_SHED=0` ([`crate::gh_invocation::READ_SHED_ENV`]) turns this
//! off: every conditional read is routed as `Gate` again.

use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;

use super::{reader_failure, run_fetch_with, ConditionalRead, FetchResult, Target};
use crate::forge_bucket_book::Resource;
use crate::forge_identity::{
    served, ExhaustCause, Failure, IdentityRole, RouteDecision, RouteRequest,
};
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

/// A deferrable conditional read that was not sent: every reader that can
/// serve the repo is out of budget until about `until`. The caller skips its
/// pass; nothing went to the writer.
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
            "read shed: every reader is out of budget until {until}; deferred, not sent to the \
             writer {}",
            crate::gh_invocation::SHED_MARKER
        )
    }
}

impl std::error::Error for ReadShed {}

/// Readers one read may try before it stops asking the router.
const MAX_READERS_TRIED: usize = 8;

/// `LOOM_READ_SHED` is not `0` (read per call, like the facade's policy).
pub(super) fn shedding_enabled() -> bool {
    !std::env::var(crate::gh_invocation::READ_SHED_ENV).is_ok_and(|v| v.trim() == "0")
}

/// One deferrable conditional read (see the module docs).
///
/// # Errors
/// [`ReadShed`] when the readers are out of budget; otherwise as
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
    let on_writer = |role: IdentityRole| {
        run_fetch_with(site, gh_bin, cwd, target, url, etag, None, false, role, None)
    };
    let Some(repo) = target.repo.as_deref() else {
        // An unresolved repo has no reader pool to be out of budget.
        return on_writer(IdentityRole::Writer);
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
    let mut last_failure: Option<Failure> = None;
    loop {
        let (dir, app_id) = match route(&request) {
            RouteDecision::Reader { dir, app_id, .. }
                if !tried.contains(&app_id) && tried.len() < MAX_READERS_TRIED =>
            {
                (dir, app_id)
            }
            // The router offered a reader this read already failed on: no
            // reader is left. Budget only if what it failed on was a rate
            // limit.
            RouteDecision::Reader { app_id, .. } => {
                if matches!(last_failure, Some(Failure::RateLimited { .. })) {
                    let until = SystemTime::now() + crate::forge_read_pool::DEFAULT_WITHDRAWAL;
                    return Err(shed(site, gh_bin, target, repo, until, Some(&app_id)));
                }
                return on_writer(IdentityRole::WriterFallback);
            }
            RouteDecision::Exhausted {
                until,
                cause: ExhaustCause::Budget,
            } => {
                let app = tried.last().map(String::as_str);
                return Err(shed(site, gh_bin, target, repo, until, app));
            }
            RouteDecision::NoPool | RouteDecision::Exhausted { .. } => {
                return on_writer(if tried.is_empty() {
                    IdentityRole::Writer
                } else {
                    IdentityRole::WriterFallback
                });
            }
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
        let Some(failure) = reader_failure(&first) else {
            // Not the credential's fault (a 5xx, a network error): neither
            // another reader nor the writer would help.
            return Ok(first);
        };
        let why = format!("{} {url}", site.caller);
        if failure == Failure::Coverage {
            // The Gate rule: confirm on the writer, and withdraw the reader
            // for this repo only if the writer could read it.
            let second = on_writer(IdentityRole::WriterFallback)?;
            if answered(&second) {
                crate::forge_identity::withdraw_after(&app_id, repo, failure, &why);
            }
            return Ok(second);
        }
        crate::forge_identity::withdraw_after(&app_id, repo, failure, &why);
        tried.push(app_id);
        last_failure = Some(failure);
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
