//! The class-aware read chain (W4-C): [`GhInvocation::execute_routed_v2`].
//!
//! Before W4-C a reader that ran dry pushed every read it served back onto
//! the writer (`reader_then_writer`), so reader exhaustion cascaded into the
//! very writer bucket the readers exist to protect. The chain now is:
//!
//! 1. run on the reader [`crate::forge_identity::route_read`] chose;
//! 2. on a rate limit or a refused credential: withdraw it (W4-A's scope),
//!    then ask the router again — the next eligible reader serves the read;
//! 3. when no reader is left, a [`ReadClass::Gate`] read goes to the writer
//!    exactly as before, while a [`ReadClass::Hygiene`] /
//!    [`ReadClass::Observability`] read is **shed**: it returns
//!    [`GhCompletion::Shed`] without a request. The same holds when the
//!    router answers [`RouteDecision::Exhausted`] up front.
//!
//! A 403/404 keeps today's rule (re-run on the writer; withdraw the reader
//! for the repo only if the writer could read it). When both answer 404, the
//! miss is remembered for [`GONE_TTL`] per `(owner/repo, affinity key)`: a
//! non-`Gate` read inside that window returns the reader's 404 with no
//! writer retry. A `Gate` read always confirms on the writer.
//!
//! # Kill switch
//!
//! `LOOM_READ_SHED=0` ([`READ_SHED_ENV`]) treats every read as `Gate`: no
//! shed and no gone-memo shortcut, so Hygiene and Observability reads fall
//! back to the writer like they did before W4-C.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use super::{failure_of, succeeded, ReaderLookup, Withdraw};
use crate::forge_bucket_book::Resource;
use crate::forge_identity::{Failure, IdentityRole, ReadClass, RouteDecision, RouteRequest};
use crate::gh_invocation::{GhCompletion, GhInvocation};
use crate::proc_exec::{Completion, ExecError};

/// `0` disables shedding (every read falls back to the writer like `Gate`).
pub const READ_SHED_ENV: &str = "LOOM_READ_SHED";

/// The marker every shed's text carries, so a log line can be traced back.
pub const SHED_MARKER: &str = "(loom-shed)";

/// How long a "gone for everyone" 404 is remembered.
pub const GONE_TTL: Duration = Duration::from_secs(3600);

/// Most remembered misses; an insert past this first drops expired ones,
/// then the soonest-expiring.
const GONE_CAP: usize = 4096;

/// Readers a single read may try before it stops spilling (a guard: the
/// router never offers a withdrawn reader twice).
const MAX_READERS_TRIED: usize = 8;

/// Shedding on or off, read per call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShedPolicy {
    /// `LOOM_READ_SHED` is not `0`.
    pub shed: bool,
}

impl ShedPolicy {
    /// The live `LOOM_READ_SHED` (read on every call — no `OnceLock`).
    #[must_use]
    pub fn current() -> Self {
        Self {
            shed: !std::env::var(READ_SHED_ENV).is_ok_and(|v| v.trim() == "0"),
        }
    }

    /// The class the chain applies to a read of `class`.
    fn effective(self, class: ReadClass) -> ReadClass {
        if self.shed {
            class
        } else {
            ReadClass::Gate
        }
    }
}

/// `(owner/repo lowercased, affinity key)` → remembered until.
type GoneMap = HashMap<(String, String), SystemTime>;

fn gone_map() -> &'static Mutex<GoneMap> {
    static MAP: OnceLock<Mutex<GoneMap>> = OnceLock::new();
    MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Whether `(owner_repo, key)` is remembered as gone at `now`.
#[must_use]
pub fn is_gone_at(owner_repo: &str, key: &str, now: SystemTime) -> bool {
    gone_map().lock().is_ok_and(|m| {
        m.get(&(owner_repo.to_ascii_lowercase(), key.to_string()))
            .is_some_and(|&until| now < until)
    })
}

/// Remember `(owner_repo, key)` as gone for everyone from `now`.
pub fn remember_gone_at(owner_repo: &str, key: &str, now: SystemTime) {
    let Ok(mut m) = gone_map().lock() else {
        return;
    };
    if m.len() >= GONE_CAP {
        m.retain(|_, until| *until > now);
        if m.len() >= GONE_CAP {
            if let Some(oldest) = m.iter().min_by_key(|(_, u)| **u).map(|(k, _)| k.clone()) {
                m.remove(&oldest);
            }
        }
    }
    m.insert((owner_repo.to_ascii_lowercase(), key.to_string()), now + GONE_TTL);
}

/// A captured run that exited non-zero with an HTTP 404.
fn is_not_found(result: &GhCompletion) -> bool {
    let GhCompletion::Captured(Completion::Exited(out)) = result else {
        return false;
    };
    if out.status.success() {
        return false;
    }
    let status = crate::forge_listing::parse_http_response(&String::from_utf8_lossy(&out.stdout))
        .map(|r| r.status);
    if status == Some(404) {
        return true;
    }
    let stderr = String::from_utf8_lossy(&out.stderr).to_ascii_lowercase();
    status.is_none() && stderr.contains("http 404")
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
        policy: ShedPolicy,
    ) -> Result<GhCompletion, ExecError> {
        self.execute_routed_v2_at(lookup, withdraw, policy, SystemTime::now())
    }

    /// [`GhInvocation::execute_routed_v2`] at an explicit instant (the gone
    /// memo's clock).
    pub(in crate::gh_invocation) fn execute_routed_v2_at(
        self,
        lookup: ReaderLookup<'_>,
        withdraw: Withdraw<'_>,
        policy: ShedPolicy,
        now: SystemTime,
    ) -> Result<GhCompletion, ExecError> {
        let Some(slug) = self.reader_slug() else {
            return self.execute_direct();
        };
        let host = super::super::accounting::resolved_identity(&self).origin;
        let resource = Resource::of_pool(super::super::accounting::static_pool(&self.args));
        let affinity = super::super::affinity_key(&self.args);
        let class = policy.effective(self.read_class);
        let request = RouteRequest {
            owner_repo: &slug,
            host: host.as_deref(),
            resource,
            affinity_key: Some(&affinity),
            // The effective class: under `LOOM_READ_SHED=0` every read is
            // routed (and falls back) like a Gate read.
            class,
        };
        let owner = crate::credential_preflight::owner_of_nwo(&slug).to_ascii_lowercase();
        let why = format!("{} via the gh choke point", self.operation.as_str());
        let mut tried: HashSet<String> = HashSet::new();
        let mut last_app: Option<String> = None;
        let mut decision = lookup(&request);
        loop {
            let (dir, app_id) = match decision {
                RouteDecision::NoPool => {
                    // No reader pool at all: the writer, as before W4.
                    return if tried.is_empty() {
                        self.execute_direct()
                    } else {
                        self.writer_fallback()
                    };
                }
                RouteDecision::Exhausted { until } => {
                    return self.no_reader_left(
                        class,
                        &owner,
                        resource,
                        until,
                        last_app.as_deref(),
                        !tried.is_empty(),
                    );
                }
                RouteDecision::Reader { dir, app_id, .. } => (dir, app_id),
            };
            if tried.contains(&app_id) || tried.len() >= MAX_READERS_TRIED {
                // The router offered a reader this read already failed on:
                // treat it as no reader left.
                let until = now + crate::forge_read_pool::DEFAULT_WITHDRAWAL;
                return self.no_reader_left(
                    class,
                    &owner,
                    resource,
                    until,
                    Some(app_id.as_str()),
                    true,
                );
            }
            let first = self
                .clone()
                .identity_role(IdentityRole::Reader)
                .gh_config_dir(Some(&dir))
                .without_token_env()
                .execute_direct()?;
            if succeeded(&first) {
                return Ok(first);
            }
            let Some(failure) = failure_of(&first, resource) else {
                // Not the credential's fault (a 5xx, a network error):
                // neither another reader nor the writer would help.
                return Ok(first);
            };
            if failure == Failure::Coverage {
                if class != ReadClass::Gate
                    && is_not_found(&first)
                    && is_gone_at(&slug, &affinity, now)
                {
                    // Gone for everyone, recently confirmed: the reader's 404
                    // is the answer; no writer retry.
                    return Ok(first);
                }
                let second = self.writer_fallback()?;
                if succeeded(&second) {
                    withdraw(&app_id, &slug, failure, &why);
                } else if is_not_found(&first) && is_not_found(&second) {
                    remember_gone_at(&slug, &affinity, now);
                }
                return Ok(second);
            }
            withdraw(&app_id, &slug, failure, &why);
            tried.insert(app_id.clone());
            last_app = Some(app_id);
            decision = match lookup(&request) {
                RouteDecision::NoPool => RouteDecision::Exhausted {
                    until: until_after(failure, now),
                },
                other => other,
            };
        }
    }

    /// Re-run this read on the writer after a reader could not serve it.
    fn writer_fallback(&self) -> Result<GhCompletion, ExecError> {
        self.clone()
            .identity_role(IdentityRole::WriterFallback)
            .execute_direct()
    }

    /// No reader can serve this read: `Gate` goes to the writer (as a
    /// fallback when a reader was tried, else as the plain writer read it
    /// was before W4), anything else is shed.
    fn no_reader_left(
        &self,
        class: ReadClass,
        owner: &str,
        resource: Resource,
        until: SystemTime,
        app: Option<&str>,
        tried: bool,
    ) -> Result<GhCompletion, ExecError> {
        if class == ReadClass::Gate {
            return if tried {
                self.writer_fallback()
            } else {
                self.clone().execute_direct()
            };
        }
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
        log::debug!(
            "gh_invocation: {} shed — every reader for {owner} {} is withdrawn {SHED_MARKER}",
            self.operation.as_str(),
            resource.as_str()
        );
        Ok(GhCompletion::Shed {
            owner: owner.to_string(),
            resource,
            until,
        })
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
