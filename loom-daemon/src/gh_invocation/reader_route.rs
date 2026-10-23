//! Read routing at the choke point (#9872).
//!
//! Every personal access token, OAuth token and `gh` login of one GitHub user
//! spends that user's single rate-limit pool. Only a GitHub App
//! **installation** token (or another account) has a pool of its own, and the
//! fleet's reader Apps (#9248 / #9537, [`crate::forge_identity`]) are exactly
//! that. Before this module only three hand-routed callers used them; every
//! other [`AccessIntent::Read`] went to the writer — on a host without the
//! writer App key, the operator's personal pool.
//!
//! A read is routed to the target repository's reader when **all** hold:
//!
//! - the intent is [`AccessIntent::Read`] and the contract is
//!   [`OutputContract::Captured`] (a passthrough stream cannot be retried);
//! - the target is an explicit `owner/repo` ([`GhTarget::Repo`]), or (W4-C)
//!   the facade derived one for an untargeted read ([`super::cwd_route`]);
//! - the caller does not route its own reads (a reader credential it chose,
//!   [`GhInvocation::gh_config_dir`] with [`GhInvocation::identity_role`]
//!   `Reader`). Any other credential a caller hands a read is set aside
//!   first (#12584, [`GhInvocation::route_site_credential`]), so it does not
//!   keep the read off the readers;
//! - the caller did not pin the writer ([`GhInvocation::writer_identity`]),
//!   which a read whose answer depends on **who** asks (a permission or
//!   write-scope probe, `viewer`, `/user`) must do;
//! - a fresh reader exists for the repo
//!   ([`crate::forge_identity::route_read`]). With no readers configured
//!   this is always [`RouteDecision::NoPool`]: such a host has no reader to
//!   be out of budget, so its reads run on the only credential it has.
//!
//! The request carries its [`super::affinity_key`] (W4-B), so a repo in
//! `forge.readPool.routing.splitRepos` spreads its reads across the pool
//! one URL per reader, and the spill latch can move a deterministic share
//! of them off a home reader that is running dry.
//!
//! The reader attempt runs with every token env var removed: `gh` prefers an
//! env `GH_TOKEN` / `GITHUB_TOKEN` over `GH_CONFIG_DIR`, so an ambient
//! personal token would otherwise serve the "reader" read. A read **never**
//! falls back to the writer (#12394): [`GhInvocation::execute`] runs the
//! reader-only chain in [`v2`], which sheds a read no reader can serve.

use super::{AccessIntent, GhCompletion, GhInvocation, OutputContract};
use crate::forge_bucket_book::Resource;
use crate::forge_gateway::outage::Fault;
use crate::forge_identity::{self, Failure, IdentityRole, RouteDecision, RouteRequest};
use crate::proc_exec::Completion;

/// One read's [`RouteRequest`] → where it goes
/// ([`forge_identity::route_read`] in production).
pub(super) type ReaderLookup<'a> = &'a dyn Fn(&RouteRequest<'_>) -> RouteDecision;

/// `(app id, owner/repo, failure, why)` → withdraw that reader.
pub(super) type Withdraw<'a> = &'a dyn Fn(&str, &str, Failure, &str);

impl GhInvocation {
    /// The `owner/repo` this invocation may be served for by a reader, or
    /// `None` when it must stay on the writer (see the module docs). An
    /// untargeted read offers its derived route (W4-C, [`super::cwd_route`]).
    #[must_use]
    pub(super) fn reader_slug(&self) -> Option<String> {
        let eligible = self.intent == AccessIntent::Read
            && matches!(self.contract, OutputContract::Captured { .. })
            && !self.writer_only
            && self.role.is_none()
            && self.config_dir.is_none()
            && !self.strip_token_env;
        if eligible {
            self.target.slug().or_else(|| self.route_slug.clone())
        } else {
            None
        }
    }
}

/// What a failed gateway attempt calls for
/// ([`crate::forge_gateway::outage::fault`]); a timeout reruns.
pub(super) fn gateway_fault(result: &GhCompletion) -> Fault {
    crate::forge_gateway::outage::escalate(classify_gateway_fault(result))
}

/// [`gateway_fault`] for one attempt alone, before the consecutive-rerun
/// escalation (#12074).
fn classify_gateway_fault(result: &GhCompletion) -> Fault {
    let out = match result {
        GhCompletion::Captured(Completion::TimedOut { .. }) => return Fault::Rerun,
        GhCompletion::Captured(Completion::Exited(out)) if !out.status.success() => out,
        _ => return Fault::None,
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let response = crate::forge_listing::parse_http_response(&stdout);
    crate::forge_gateway::outage::fault(
        response.as_ref(),
        &stdout,
        &String::from_utf8_lossy(&out.stderr),
    )
}

/// Withdraw the gateway after an outage
/// ([`crate::forge_gateway::outage::withdraw_for_outage`]).
pub(super) fn withdraw_gateway_outage(why: &str) {
    crate::forge_gateway::outage::withdraw_for_outage(why);
    // A CLI or front process: let the next one skip the gateway too.
    crate::forge_gateway::note_outage_from_front();
}

/// A captured run that exited 0.
fn succeeded(result: &GhCompletion) -> bool {
    matches!(result, GhCompletion::Captured(Completion::Exited(out)) if out.status.success())
}

/// What a non-zero captured exit says about the credential. A timeout is not
/// the credential's fault and is never retried. The response's own
/// `x-ratelimit-*` / `Retry-After` headers (a `--include` call) name the
/// refused pool and its reset (W4-A); `resource` is the pool the call
/// statically spends, used when they do not.
fn failure_of(result: &GhCompletion, resource: Resource) -> Option<Failure> {
    let GhCompletion::Captured(Completion::Exited(out)) = result else {
        return None;
    };
    if out.status.success() {
        return None;
    }
    let response = crate::forge_listing::parse_http_response(&String::from_utf8_lossy(&out.stdout));
    forge_identity::classify_failure(
        &String::from_utf8_lossy(&out.stderr),
        response.as_ref().map(|r| r.status),
        response.as_ref().map(|r| &r.ratelimit),
        resource,
    )
}

/// The production withdrawal: the reset travels inside `failure` (W4-A).
pub(super) fn withdraw_reader(app_id: &str, slug: &str, failure: Failure, why: &str) {
    forge_identity::withdraw_after(app_id, slug, failure, why);
}

/// Which role an execution is accounted under: the one the routing step (or
/// a hand-routed caller) set, else the writer.
#[must_use]
pub(super) fn role_of(inv: &GhInvocation) -> IdentityRole {
    inv.role.unwrap_or(IdentityRole::Writer)
}

#[path = "reader_route_v2.rs"]
mod v2;
pub(crate) use v2::ShedWhy;
pub use v2::{host_has_read_pool, SHED_MARKER};

#[cfg(test)]
#[path = "reader_route_v2_tests.rs"]
mod v2_tests;

#[cfg(test)]
#[path = "reader_route_gateway_tests.rs"]
mod gateway_tests;

#[cfg(test)]
#[path = "reader_only_tests.rs"]
mod reader_only_tests;
