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
//! - the target is an explicit `owner/repo` ([`GhTarget::Repo`]);
//! - the caller did not pick a credential itself
//!   ([`GhInvocation::gh_config_dir`], [`GhInvocation::without_token_env`])
//!   and does not route its own reads ([`GhInvocation::identity_role`]);
//! - the caller did not pin the writer ([`GhInvocation::writer_identity`]),
//!   which a read whose answer depends on **who** asks (a permission or
//!   write-scope probe, `viewer`, `/user`) must do;
//! - a fresh reader exists for the repo
//!   ([`crate::forge_identity::route_read`]). With no readers configured
//!   this is always [`RouteDecision::NoPool`], so such a host behaves
//!   byte-for-byte as before.
//!
//! The request carries its [`super::affinity_key`] (W4-B), so a repo in
//! `forge.readPool.routing.splitRepos` spreads its reads across the pool
//! one URL per reader, and the spill latch can move a deterministic share
//! of them off a home reader that is running dry.
//!
//! The reader attempt runs with every token env var removed: `gh` prefers an
//! env `GH_TOKEN` / `GITHUB_TOKEN` over `GH_CONFIG_DIR`, so an ambient
//! personal token would otherwise serve the "reader" read. A credential
//! failure falls back to the writer through the shared
//! [`crate::forge_identity::reader_then_writer`] shape.

use super::{AccessIntent, GhCompletion, GhInvocation, OutputContract};
use crate::forge_bucket_book::Resource;
use crate::forge_identity::{self, Failure, IdentityRole, ReadClass, RouteDecision, RouteRequest};
use crate::proc_exec::{Completion, ExecError};

/// One read's [`RouteRequest`] → where it goes
/// ([`forge_identity::route_read`] in production).
pub(super) type ReaderLookup<'a> = &'a dyn Fn(&RouteRequest<'_>) -> RouteDecision;

/// `(app id, owner/repo, failure, why)` → withdraw that reader.
pub(super) type Withdraw<'a> = &'a dyn Fn(&str, &str, Failure, &str);

impl GhInvocation {
    /// The `owner/repo` this invocation may be served for by a reader, or
    /// `None` when it must stay on the writer (see the module docs).
    #[must_use]
    pub(super) fn reader_slug(&self) -> Option<String> {
        let eligible = self.intent == AccessIntent::Read
            && matches!(self.contract, OutputContract::Captured { .. })
            && !self.writer_only
            && self.role.is_none()
            && self.config_dir.is_none()
            && !self.strip_token_env;
        if eligible {
            self.target.slug()
        } else {
            None
        }
    }

    /// [`GhInvocation::execute`]'s routing step, with the reader lookup and
    /// the withdrawal injected (production passes
    /// [`forge_identity::route_read`] / [`forge_identity::withdraw_after`]).
    /// [`RouteDecision::NoPool`] and [`RouteDecision::Exhausted`] both run on
    /// the writer here: every read is [`ReadClass::Gate`] until W4-C.
    pub(super) fn execute_routed(
        self,
        lookup: ReaderLookup<'_>,
        withdraw: Withdraw<'_>,
    ) -> Result<GhCompletion, ExecError> {
        let Some(slug) = self.reader_slug() else {
            return self.execute_direct();
        };
        let host = super::accounting::resolved_identity(&self).origin;
        // W4-A: the pool this call spends, so a reader withdrawn from this
        // owner's `core` still serves its `graphql` reads (and vice versa).
        let resource = Resource::of_pool(super::accounting::static_pool(&self.args));
        let affinity = super::affinity_key(&self.args);
        let request = RouteRequest {
            owner_repo: &slug,
            host: host.as_deref(),
            resource,
            affinity_key: Some(&affinity),
            class: ReadClass::Gate,
        };
        let Some((dir, app_id)) = lookup(&request).into_credential() else {
            return self.execute_direct();
        };
        forge_identity::reader_then_writer(
            Some(dir.as_path()),
            |reader_dir, role| {
                let inv = self.clone().identity_role(role);
                match reader_dir {
                    Some(d) => inv.gh_config_dir(Some(d)).without_token_env(),
                    None => inv,
                }
                .execute_direct()
            },
            succeeded,
            |result| failure_of(result, resource),
            |failure, _| {
                let why = format!("{} via the gh choke point", self.operation.as_str());
                withdraw(&app_id, &slug, failure, &why);
            },
        )
    }
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

#[cfg(test)]
#[path = "reader_route_tests.rs"]
mod tests;
