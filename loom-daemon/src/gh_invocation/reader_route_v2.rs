//! The reader-only read chain: [`GhInvocation::execute_routed_v2`].
//!
//! **The writer is never used for reads** (operator ruling, #12394). The
//! writer App's installation has a fixed hourly budget the fleet's claims,
//! merges and label flips spend; a read that spilled onto it when the readers
//! ran dry is what tripped `rate_limit_breaker` fleet-wide. The chain is:
//!
//! 1. run on the reader [`crate::forge_identity::route_read`] chose;
//! 2. on a rate limit or a refused credential: withdraw it (W4-A's scope),
//!    then ask the router again — the next eligible reader serves the read;
//! 3. when no reader is left — whatever the cause, whatever the
//!    [`ReadClass`] — the read is **shed**: it returns
//!    [`GhCompletion::Shed`] without a request, and its caller defers it,
//!    skips it this tick or serves it from cache.
//!
//! A 403/404 is an answer only with evidence the reader covers the repo
//! (W6, [`crate::forge_identity::served::judge_coverage`]): an item-scoped
//! `gh api repos/<o>/<r>/<family>/<item>` read answered `404` by a reader
//! that recently served the same repo's same family. Any other 403/404 (a
//! "not accessible by integration", "could not resolve to a repository", an
//! installation that does not include the repo), or any refusal from a
//! reader with no evidence it sees the repo, means the reader is blind to
//! it: it is withdrawn **for that repo** ([`Failure::Coverage`]'s scope),
//! the next reader is tried, as in 2, and the read is shed when none is
//! left. A plain 404 from a reader that recently served another of the
//! repo's families is shed without a withdrawal, so one missing item cannot
//! take a covering reader off the repo. Returning a refusal would read as
//! "absent" to the caller, and placement is deterministic, so every later
//! read would land on the same blind reader.
//!
//! The one read that still reaches the writer is a host with **no reader
//! pool at all** ([`RouteDecision::NoPool`] before any reader was tried): the
//! writer is the only credential there is, so shedding would brick the host.
//! It runs through `GhInvocation::writer_identity`, the same explicit,
//! allow-listed marker as an asker-dependent probe, so it is not booked as a
//! read-on-the-writer error.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use super::{failure_of, succeeded, ReaderLookup, Withdraw};
use crate::forge_bucket_book::Resource;
use crate::forge_identity::{
    served, ExhaustCause, Failure, IdentityRole, ReadClass, RouteDecision, RouteRequest,
};
use crate::gh_invocation::{GhCompletion, GhInvocation};
use crate::proc_exec::{Completion, ExecError};

/// The marker every shed's text carries, so a log line can be traced back.
pub const SHED_MARKER: &str = "(loom-shed)";

/// Readers a single read may try before it stops spilling (a guard: the
/// router never offers a withdrawn reader twice).
const MAX_READERS_TRIED: usize = 8;

/// How often a shed is logged at `info` per operation; the rest are `debug`
/// (the `forge.read.shed` span still records every one).
const SHED_LOG_EVERY: Duration = Duration::from_secs(300);

/// Whether a shed of `op` at `now` should be logged at `info`: the first,
/// then at most one per [`SHED_LOG_EVERY`].
fn shed_log_due(op: &str, now: SystemTime) -> bool {
    static LAST: OnceLock<Mutex<HashMap<String, SystemTime>>> = OnceLock::new();
    let Ok(mut m) = LAST.get_or_init(|| Mutex::new(HashMap::new())).lock() else {
        return false;
    };
    match m.get(op) {
        Some(&at) if now.duration_since(at).is_ok_and(|d| d < SHED_LOG_EVERY) => false,
        _ => {
            m.insert(op.to_string(), now);
            true
        }
    }
}

/// What a reader's coverage refusal `result` calls for
/// ([`served::judge_coverage`]): the status comes from the `--include`
/// headers when `gh` printed them, else the stderr's `HTTP nnn`.
fn judge_refusal(
    result: &GhCompletion,
    app_id: &str,
    slug: &str,
    family: &str,
    item_scoped: bool,
    now: SystemTime,
) -> served::CoverageRefusal {
    let GhCompletion::Captured(Completion::Exited(out)) = result else {
        return served::CoverageRefusal::Withdraw;
    };
    let stderr = String::from_utf8_lossy(&out.stderr);
    let status = crate::forge_listing::parse_http_response(&String::from_utf8_lossy(&out.stdout))
        .map(|r| r.status)
        .or_else(|| crate::forge_gateway::outage::http_status_in(&stderr));
    served::judge_coverage(app_id, slug, family, item_scoped, status, &stderr, now)
}

/// When a withdrawn reader is expected back, for a shed's `until`.
fn until_after(failure: Failure, now: SystemTime) -> SystemTime {
    match failure {
        Failure::RateLimited { reset: Some(t), .. } if t > now => t,
        _ => now + crate::forge_read_pool::DEFAULT_WITHDRAWAL,
    }
}

impl GhInvocation {
    /// [`GhInvocation::execute`]'s W4-C routing step (see the module docs),
    /// with the router and the withdrawal injected.
    pub(in crate::gh_invocation) fn execute_routed_v2(
        self,
        lookup: ReaderLookup<'_>,
        withdraw: Withdraw<'_>,
    ) -> Result<GhCompletion, ExecError> {
        self.execute_routed_v2_at(lookup, withdraw, SystemTime::now())
    }

    /// [`GhInvocation::execute_routed_v2`] at an explicit instant.
    pub(in crate::gh_invocation) fn execute_routed_v2_at(
        self,
        lookup: ReaderLookup<'_>,
        withdraw: Withdraw<'_>,
        now: SystemTime,
    ) -> Result<GhCompletion, ExecError> {
        let Some(slug) = self.reader_slug() else {
            return self.execute_direct();
        };
        let host = super::super::accounting::resolved_identity(&self).origin;
        let resource = Resource::of_pool(super::super::accounting::static_pool(&self.args));
        let affinity = super::super::affinity_key(&self.args);
        let class = self.read_class;
        let request = RouteRequest {
            owner_repo: &slug,
            host: host.as_deref(),
            resource,
            affinity_key: Some(&affinity),
            class,
        };
        let owner = crate::credential_preflight::owner_of_nwo(&slug).to_ascii_lowercase();
        let why = format!("{} via the gh choke point", self.operation.as_str());
        // W6 coverage evidence: the REST path's endpoint family, and whether
        // it names one item. Not a `gh api` call: no family, no evidence.
        let endpoint = super::super::accounting::api_endpoint(&self.args).unwrap_or_default();
        let family = served::endpoint_family(&endpoint);
        let item_scoped = served::is_item_path(&endpoint);
        let mut tried: HashSet<String> = HashSet::new();
        let mut last_app: Option<String> = None;
        let mut last_failure: Option<Failure> = None;
        let mut decision = lookup(&request);
        loop {
            let (dir, app_id) = match decision {
                RouteDecision::NoPool if tried.is_empty() => {
                    // No reader pool at all: the writer is the only
                    // credential there is (the one allow-listed read on it).
                    return self.writer_only_read();
                }
                RouteDecision::NoPool => {
                    let until = now + crate::forge_read_pool::DEFAULT_WITHDRAWAL;
                    return Ok(self.shed_read(class, &owner, resource, until, last_app.as_deref()));
                }
                RouteDecision::Exhausted { until, .. } => {
                    return Ok(self.shed_read(class, &owner, resource, until, last_app.as_deref()));
                }
                RouteDecision::Reader { dir, app_id, .. } => (dir, app_id),
            };
            if tried.contains(&app_id) || tried.len() >= MAX_READERS_TRIED {
                // The router offered a reader this read already failed on:
                // treat it as no reader left.
                let until = last_failure
                    .map_or(now + crate::forge_read_pool::DEFAULT_WITHDRAWAL, |f| {
                        until_after(f, now)
                    });
                return Ok(self.shed_read(class, &owner, resource, until, Some(app_id.as_str())));
            }
            let first = self
                .clone()
                .identity_role(IdentityRole::Reader)
                .gh_config_dir(Some(&dir))
                .without_token_env()
                .execute_direct()?;
            if succeeded(&first) {
                served::note_reader_served(&app_id, &slug, family, now);
                return Ok(first);
            }
            let fault = if app_id == crate::forge_gateway::GATEWAY_APP_ID {
                super::gateway_fault(&first)
            } else {
                crate::forge_gateway::outage::Fault::None
            };
            if fault.reruns() {
                // 2am#4367 P4: the gateway failed, not the credential: run
                // the read once more on the readers (never the writer), and
                // take the gateway out only when it signalled the fault.
                if fault == crate::forge_gateway::outage::Fault::Withdraw {
                    super::withdraw_gateway_outage(self.operation.as_str());
                }
                tried.insert(app_id.clone());
                last_app = Some(app_id);
                last_failure = None;
                decision = match crate::forge_gateway::past_gateway(lookup(&request), &request, now)
                {
                    RouteDecision::NoPool => RouteDecision::Exhausted {
                        until: now + crate::forge_read_pool::DEFAULT_WITHDRAWAL,
                        cause: ExhaustCause::Unavailable,
                    },
                    other => other,
                };
                continue;
            }
            let Some(mut failure) = failure_of(&first, resource) else {
                // Not the credential's fault (a 5xx, a network error):
                // another reader would not help.
                return Ok(first);
            };
            if failure == Failure::Coverage {
                match judge_refusal(&first, &app_id, &slug, family, item_scoped, now) {
                    // W6: this reader covers the repo's `family`, so its 404
                    // is the item's own answer: no withdrawal, no other reader.
                    served::CoverageRefusal::Answer => return Ok(first),
                    // A plain 404 from a reader that sees the repo: no
                    // answer, and the reader stays on the repo's other reads.
                    served::CoverageRefusal::Shed => {
                        let until = until_after(failure, now);
                        return Ok(self.shed_read(class, &owner, resource, until, Some(&app_id)));
                    }
                    served::CoverageRefusal::Withdraw => {}
                    // No evidence at all: a short withdrawal (#12511).
                    served::CoverageRefusal::WithdrawUnproven => {
                        failure = Failure::CoverageUnproven;
                    }
                }
            }
            // A rate limit, a refused token, or a 403/404 from a reader with
            // no evidence it can see the repo: withdraw it (for this repo
            // only on coverage) and ask the router for the next one.
            withdraw(&app_id, &slug, failure, &why);
            tried.insert(app_id.clone());
            last_app = Some(app_id);
            last_failure = Some(failure);
            decision = self.next_after(lookup, &request, &tried, failure, now);
        }
    }

    /// The router's next answer after a reader failed with `failure`: past
    /// the gateway once it was tried, and a pool that went away under us is
    /// an exhaustion (shed), never "no pool" (the writer).
    fn next_after(
        &self,
        lookup: ReaderLookup<'_>,
        request: &RouteRequest<'_>,
        tried: &HashSet<String>,
        failure: Failure,
        now: SystemTime,
    ) -> RouteDecision {
        let next = lookup(request);
        let next = if tried.contains(crate::forge_gateway::GATEWAY_APP_ID) {
            crate::forge_gateway::past_gateway(next, request, now)
        } else {
            next
        };
        match next {
            RouteDecision::NoPool => RouteDecision::Exhausted {
                until: until_after(failure, now),
                cause: ExhaustCause::Unavailable,
            },
            other => other,
        }
    }

    /// A read on a host with no reader pool: the writer, through the
    /// explicit `writer_identity` marker.
    fn writer_only_read(self) -> Result<GhCompletion, ExecError> {
        self.writer_identity().execute_direct()
    }

    /// No reader can serve this read: shed it, whatever its class and cause.
    /// Nothing is sent; the caller defers, skips or serves from cache.
    fn shed_read(
        &self,
        class: ReadClass,
        owner: &str,
        resource: Resource,
        until: SystemTime,
        app: Option<&str>,
    ) -> GhCompletion {
        self.record_read_shed(class, owner, resource, until, app);
        GhCompletion::Shed {
            owner: owner.to_string(),
            resource,
            until,
        }
    }

    /// Book one shed of this read: the `o=shed` accounting row, the
    /// `forge.read.shed` span and the (rate-limited) log line. No request is
    /// sent. Shared with the conditional-read path
    /// ([`crate::forge_etag_store`]), which sheds outside this chain.
    pub(crate) fn record_read_shed(
        &self,
        class: ReadClass,
        owner: &str,
        resource: Resource,
        until: SystemTime,
        app: Option<&str>,
    ) {
        super::super::accounting::record_shed(self, app, owner, resource, until);
        #[cfg(test)]
        super::super::test_routing::note_shed();
        crate::observability::ops::read_shed::record_shed(
            &crate::observability::ops::read_shed::Shed {
                op: self.operation.as_str(),
                class: class_str(class),
                app: app.unwrap_or("-"),
                owner,
                resource: resource.as_str(),
                until: until.into(),
            },
        );
        let op = self.operation.as_str();
        let until_s = chrono::DateTime::<chrono::Utc>::from(until)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        if shed_log_due(op, SystemTime::now()) {
            log::info!(
                "gh_invocation: {op} shed — no reader can serve {owner} {} \
                 until {until_s}; deferred, not sent to the writer {SHED_MARKER} \
                 (logged at most every {}s per operation)",
                resource.as_str(),
                SHED_LOG_EVERY.as_secs()
            );
        } else {
            log::debug!(
                "gh_invocation: {op} shed — no reader can serve {owner} {} \
                 until {until_s} {SHED_MARKER}",
                resource.as_str()
            );
        }
    }
}

/// The `forge.read.class` value.
#[must_use]
pub fn class_str(class: ReadClass) -> &'static str {
    match class {
        ReadClass::Gate => "gate",
        ReadClass::Hygiene => "hygiene",
        ReadClass::Observability => "observability",
    }
}
