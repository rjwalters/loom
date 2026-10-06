//! Batched session-container liveness read (Issue #10454, Epic #10452).
//!
//! A session-managed Codex account (one adopted by `accounts session start`,
//! see [`super::is_session_managed`]) can only be dispatched into while its
//! `loom-codex-session-<name>` container is running: `spawn-codex.sh`'s
//! session-exec posture check refuses a stopped one with exit 78. Before this
//! module the role runner's pre-spawn gate and the ordered preference
//! resolver counted such an account as spawnable anyway, so a
//! `rolePreference.judge = ["codex", "claude"]` tick with every session
//! container down selected Codex, picked an account and was killed — it never
//! fell through to Claude. That broke `runtime_preference::availability`'s
//! own rule: *a tap that would refuse at spawn must be passed over, not
//! selected and then killed.*
//!
//! # One `docker ps` per pass, never one `inspect` per account
//!
//! [`running_sessions`] lists every running `loom-codex-session-*` container
//! in a single `docker ps` and caches the answer for [`CACHE_TTL`], so the
//! preflight gate and the availability mapping — which both read the pool in
//! the same tick — share one Docker round trip. The call is bounded by
//! [`LIVENESS_TIMEOUT`] (the same kill-on-overrun helper the reaper uses for
//! its own `docker ps`), so a wedged dockerd cannot stall a role tick.
//!
//! # Fail open
//!
//! When Docker cannot be queried at all — no binary, a non-zero exit, a
//! timeout — the answer is `None`: *cannot observe*, which every caller treats
//! as "no account is down" (`availability`'s `Ungated` rule). Only a
//! successful listing that does not name an account's container marks that
//! account down. A bare-metal (non-session-managed) account is never
//! consulted at all, and no `docker` process is started unless some enabled
//! account is session-managed.

use std::collections::HashSet;
use std::time::Duration;

use super::super::account_registry::{AccountDescriptor, AccountProvider};
use super::{container_name, is_session_managed};

/// Wall-clock bound on the one `docker ps` a pass makes.
pub const LIVENESS_TIMEOUT: Duration = Duration::from_secs(5);

/// How long one listing answers for. Long enough to cover the preflight gate
/// and the availability mapping reading the pool in the same tick, short
/// enough that a container the reconciler just restarted is seen next tick.
pub const CACHE_TTL: Duration = Duration::from_secs(5);

/// The skip/count reason this module introduces. A count reason, never a
/// persisted `HealthReason` — no `account-health.json` schema change.
pub const SESSION_DOWN: &str = "SessionDown";

/// The set of session containers Docker reported as running.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionLiveness {
    running: HashSet<String>,
}

impl SessionLiveness {
    /// Build from container names (as `docker ps --format {{.Names}}` prints
    /// them). Names outside the `loom-codex-session-` namespace are ignored.
    pub fn from_container_names<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let prefix = container_name("");
        Self {
            running: names
                .into_iter()
                .map(|name| name.as_ref().trim().to_string())
                .filter(|name| name.len() > prefix.len() && name.starts_with(&prefix))
                .collect(),
        }
    }

    /// Parse `docker ps --format {{.Names}}` stdout (one name per line).
    #[must_use]
    pub fn parse_ps(stdout: &str) -> Self {
        Self::from_container_names(stdout.lines())
    }

    /// Whether `account_name`'s session container is running.
    #[must_use]
    pub fn is_running(&self, account_name: &str) -> bool {
        self.running.contains(&container_name(account_name))
    }
}

/// `true` iff `account` is session-managed AND `liveness` positively shows
/// its container is not running. `None` liveness (Docker unobservable) is
/// never "down" — the fail-open rule.
#[must_use]
pub fn is_session_down(account: &AccountDescriptor, liveness: Option<&SessionLiveness>) -> bool {
    liveness.is_some_and(|live| {
        is_session_managed(&account.credential_reference) && !live.is_running(&account.id.name)
    })
}

/// The liveness read for `inventory`: `None` without starting any process when
/// no enabled Codex account is session-managed, else [`running_sessions`].
#[must_use]
pub fn liveness_for(inventory: &[AccountDescriptor]) -> Option<SessionLiveness> {
    let any_session_managed = inventory.iter().any(|account| {
        account.id.provider == AccountProvider::Codex
            && account.enabled
            && is_session_managed(&account.credential_reference)
    });
    if any_session_managed {
        running_sessions()
    } else {
        None
    }
}

/// The accounts in `inventory` the selector should try first: everything
/// except session-managed accounts whose container is down. `None` when there
/// is nothing to prefer — Docker unobservable, no account down, or no account
/// live — so the caller selects from the full inventory exactly as before.
#[must_use]
pub fn live_preferred(inventory: &[AccountDescriptor]) -> Option<Vec<AccountDescriptor>> {
    let liveness = liveness_for(inventory)?;
    let live: Vec<AccountDescriptor> = inventory
        .iter()
        .filter(|account| !is_session_down(account, Some(&liveness)))
        .cloned()
        .collect();
    let any_live_enabled = live.iter().any(|account| account.enabled);
    (live.len() < inventory.len() && any_live_enabled).then_some(live)
}

/// Every running `loom-codex-session-*` container, from one bounded
/// `docker ps`, cached for [`CACHE_TTL`]. `None` when Docker could not be
/// queried (fail open — see the module doc).
#[must_use]
pub fn running_sessions() -> Option<SessionLiveness> {
    #[cfg(test)]
    {
        test_support::current()
    }
    #[cfg(not(test))]
    {
        use std::sync::Mutex;
        use std::time::Instant;
        static CACHE: Mutex<Option<(Instant, Option<SessionLiveness>)>> = Mutex::new(None);
        let mut cache = CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((at, value)) = cache.as_ref() {
            if at.elapsed() < CACHE_TTL {
                return value.clone();
            }
        }
        let value = query_docker();
        *cache = Some((Instant::now(), value.clone()));
        value
    }
}

#[cfg_attr(test, allow(dead_code))]
fn query_docker() -> Option<SessionLiveness> {
    let mut cmd = std::process::Command::new("docker");
    cmd.args([
        "ps",
        "--filter",
        "status=running",
        "--filter",
        &format!("name={}", container_name("")),
        "--format",
        "{{.Names}}",
    ])
    .stdin(std::process::Stdio::null());
    match crate::sweep_registry::reaper::output_with_timeout(cmd, LIVENESS_TIMEOUT) {
        Ok(Some(out)) if out.status.success() => {
            Some(SessionLiveness::parse_ps(&String::from_utf8_lossy(&out.stdout)))
        }
        Ok(Some(out)) => {
            log::debug!(
                "session liveness: `docker ps` exited {:?} — not marking any session down \
                 (fail open, #10454)",
                out.status.code()
            );
            None
        }
        Ok(None) => {
            log::warn!(
                "session liveness: `docker ps` timed out after {LIVENESS_TIMEOUT:?} — not \
                 marking any session down (fail open, #10454)"
            );
            None
        }
        Err(e) => {
            log::debug!(
                "session liveness: `docker ps` could not run ({e}) — not marking any session \
                 down (fail open, #10454)"
            );
            None
        }
    }
}

/// Test seam: under `cfg(test)` [`running_sessions`] never shells out to
/// Docker. It answers what the current thread installed with [`set`], and by
/// default `None` (unobservable) — so a test that marks an account
/// session-managed for an unrelated reason is unaffected by whatever
/// containers happen to be running on the test host.
#[cfg(test)]
pub(crate) mod test_support {
    use super::SessionLiveness;
    use std::cell::RefCell;

    thread_local! {
        static OVERRIDE: RefCell<Option<SessionLiveness>> = const { RefCell::new(None) };
    }

    pub(crate) fn current() -> Option<SessionLiveness> {
        OVERRIDE.with(|cell| cell.borrow().clone())
    }

    /// Restores "unobservable" on drop, including across a panic.
    pub(crate) struct LivenessGuard;

    impl Drop for LivenessGuard {
        fn drop(&mut self) {
            OVERRIDE.with(|cell| *cell.borrow_mut() = None);
        }
    }

    /// Answer every [`super::running_sessions`] on this thread with
    /// `liveness` (`None` = Docker could not be queried) until the guard drops.
    #[must_use]
    pub(crate) fn set(liveness: Option<SessionLiveness>) -> LivenessGuard {
        OVERRIDE.with(|cell| *cell.borrow_mut() = liveness);
        LivenessGuard
    }

    /// Liveness listing exactly `accounts`' session containers as running.
    pub(crate) fn running(accounts: &[&str]) -> Option<SessionLiveness> {
        Some(SessionLiveness::from_container_names(
            accounts.iter().map(|name| super::container_name(name)),
        ))
    }
}

#[cfg(test)]
#[path = "liveness_tests.rs"]
mod tests;
