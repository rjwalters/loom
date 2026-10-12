//! Best-effort session-container health refresh over an inventory (issue
//! #6927), split out of `session_lifecycle.rs` for the file-size ratchet
//! (#7711). Re-exported from there, so callers' paths are unchanged.

use std::path::Path;

use super::{
    is_session_managed, AccountDescriptor, AccountProvider, ProcessContainerRunner,
    SessionHealthOutcome, SessionLifecycle,
};

fn probe_enabled() -> bool {
    !matches!(
        std::env::var("LOOM_CODEX_SESSION_PROBE")
            .unwrap_or_default()
            .as_str(),
        "0" | "false" | "no"
    )
}

/// Best-effort proactive auth-state refresh over `inventory`, for callers on
/// the account-selection path (issue #6927).
///
/// Deliberately infallible: a probe is an *optimization* over discovering a
/// dead refresh chain by dispatching into it, so a probe that cannot run must
/// never be the reason a dispatch cannot run. It is also a complete no-op —
/// zero `docker` invocations — when no enabled account is session-managed,
/// which is every pool that has not opted into session containers, and when
/// `LOOM_CODEX_SESSION_PROBE` is set to `0`/`false`/`no`.
pub fn refresh_session_health(
    workspace: &Path,
    inventory: &[AccountDescriptor],
    now: u64,
) -> Vec<SessionHealthOutcome> {
    refresh_session_health_inner(workspace, inventory, now, None)
}

/// [`refresh_session_health`] bypassing the probe cache: every account in
/// `inventory` is probed now. For a container that was just (re)started, a
/// probe from minutes ago says nothing about its `auth.json` chain (issue
/// #10453).
pub fn refresh_session_health_uncached(
    workspace: &Path,
    inventory: &[AccountDescriptor],
    now: u64,
) -> Vec<SessionHealthOutcome> {
    refresh_session_health_inner(workspace, inventory, now, Some(0))
}

fn refresh_session_health_inner(
    workspace: &Path,
    inventory: &[AccountDescriptor],
    now: u64,
    ttl: Option<u64>,
) -> Vec<SessionHealthOutcome> {
    if !probe_enabled() {
        return Vec::new();
    }
    let any_session_managed = inventory.iter().any(|account| {
        account.id.provider == AccountProvider::Codex
            && account.enabled
            && is_session_managed(&account.credential_reference)
    });
    if !any_session_managed {
        return Vec::new();
    }
    let lifecycle = SessionLifecycle::new(workspace, ProcessContainerRunner, None);
    match ttl {
        Some(ttl) => lifecycle.refresh_health_with_ttl(inventory, now, ttl),
        None => lifecycle.refresh_health_at(inventory, now),
    }
    .unwrap_or_default()
}
