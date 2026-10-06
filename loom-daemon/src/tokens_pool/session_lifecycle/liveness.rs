//! Session-container liveness for account selection (Issue #10454, converged
//! on the shared snapshot by #10660; Epic #10452).
//!
//! A session-managed Codex account (one adopted by `accounts session start`,
//! see [`super::is_session_managed`]) can only be dispatched into while its
//! `loom-codex-session-<name>` container can take a `docker exec`:
//! `spawn-codex.sh`'s session-exec posture check refuses a down one with exit
//! 78. The role runner's pre-spawn gate and the ordered preference resolver
//! therefore must not count such an account as spawnable — *a tap that would
//! refuse at spawn must be passed over, not selected and then killed*
//! (`runtime_preference::availability`).
//!
//! # No Docker read of its own
//!
//! This module is a thin adapter over [`session_state`]: it never builds a
//! `docker` command line. "Down" is [`SessionState::is_down`] — stopped,
//! restarting (a crash loop backing off) or missing — the same rule the
//! visibility gauge, the posture check and the reconciler classify with, so
//! the readers cannot drift apart. There are two ways to get a snapshot:
//!
//! * **In the daemon** ([`published`]): the newest snapshot the always-on
//!   watch (`observability::ops::codex_session::spawn_watch`, every 60 s)
//!   published, read with [`session_state::latest`] and
//!   [`session_state::LATEST_MAX_AGE`]. A mutex read: the pre-spawn gate and
//!   the preference resolver run on every role tick and start **zero**
//!   `docker` processes, so a wedged dockerd costs the role loop nothing.
//! * **Outside the daemon** ([`for_selector`]): `loom-daemon tokens select`
//!   (run by `spawn-codex.sh`) and the worker launcher are separate,
//!   short-lived processes that cannot see the daemon's in-memory snapshot.
//!   The choice made in #10660 is that the selector takes **one** bounded
//!   [`session_state::snapshot`] itself — the same code path, the same
//!   [`session_state::SNAPSHOT_DEADLINE`], the same classification — rather
//!   than having the daemon pass a verdict down through the adapter's
//!   environment (which would have needed new portable shell, and a second
//!   encoding of the verdict to keep in step). It is taken only when some
//!   enabled Codex account is session-managed; every other pool starts no
//!   process at all. A process that runs the watch ([`mark_watch_runs_here`])
//!   never takes it: there the selector reads [`published`] like every other
//!   in-daemon reader, including while no fresh snapshot exists.
//!
//! The selector's own snapshot is taken with no registered workspace roots,
//! so it never classifies `stale_mounts`; that changes nothing here, because
//! of the next rule.
//!
//! # `stale_mounts` is not down
//!
//! A running container that lacks the mount of some registered root can still
//! serve every repository it does mount. Whether it can serve *this* dispatch
//! is the per-dispatch mount check's question (#10364), not selection's, so a
//! `stale_mounts` account stays selectable.
//!
//! # Fail open
//!
//! `None` (no snapshot younger than the max age — including the first moments
//! after daemon start, before the watch's first pass) and
//! [`Snapshot::Unavailable`] (Docker could not be queried: no binary, an
//! unreachable or wedged daemon) both mean *cannot observe*, which every
//! caller treats as "no account is down" (`availability`'s `Ungated` rule).
//! Only an [`Snapshot::Available`] map marks anything down. A bare-metal
//! (non-session-managed) account is never gated on a container at all.
//!
//! # Operator holds
//!
//! [`is_held`] reads the `.session-hold.json` sidecar `accounts session stop`
//! leaves (`tokens_pool::session_hold`). A hold never changes whether an
//! account is down — that is the snapshot's answer alone — only how a down
//! account is described: the skip text must not tell the operator to
//! `accounts session start` a container they stopped on purpose. It reads the
//! account's own profile directory; a hold recorded only under another
//! registered root's differently-resolved profile is not seen from here.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::super::account_registry::{AccountDescriptor, AccountProvider};
use super::super::session_hold;
use super::super::session_state::{self, SessionState, Snapshot};
use super::{container_name, is_session_managed};

/// The skip/count reason selection reports. A count reason, never a
/// persisted `HealthReason` — no `account-health.json` schema change.
pub const SESSION_DOWN: &str = "SessionDown";

/// One shared snapshot, read by account name.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionLiveness(Arc<Snapshot>);

impl Default for SessionLiveness {
    /// Docker answered and listed no session container: every account's
    /// container is missing.
    fn default() -> Self {
        Self(Arc::new(Snapshot::Available(std::collections::BTreeMap::new())))
    }
}

impl SessionLiveness {
    #[must_use]
    pub fn from_snapshot(snapshot: Arc<Snapshot>) -> Self {
        Self(snapshot)
    }

    /// `account_name`'s container state; `None` when the snapshot is
    /// unavailable (cannot observe).
    #[must_use]
    pub fn state_of(&self, account_name: &str) -> Option<SessionState> {
        self.0.state_of(&container_name(account_name))
    }

    /// Whether the snapshot positively shows `account_name`'s container
    /// cannot take a dispatch ([`SessionState::is_down`]).
    #[must_use]
    pub fn is_down(&self, account_name: &str) -> bool {
        self.state_of(account_name)
            .is_some_and(SessionState::is_down)
    }
}

/// `true` iff `account` is session-managed AND `liveness` positively shows
/// its container is down. `None` liveness and an unavailable snapshot are
/// never "down" — the fail-open rule.
#[must_use]
pub fn is_session_down(account: &AccountDescriptor, liveness: Option<&SessionLiveness>) -> bool {
    liveness.is_some_and(|live| {
        live.is_down(&account.id.name) && is_session_managed(&account.credential_reference)
    })
}

/// Whether the operator holds `account`'s session down (`accounts session
/// stop`). File reads only. See the module doc for what this does and does
/// not decide.
#[must_use]
pub fn is_held(account: &AccountDescriptor) -> bool {
    session_hold::held_across(std::slice::from_ref(&account.credential_reference))
}

/// The in-daemon read: the watch's newest published snapshot, if fresh.
/// Never starts a process.
#[must_use]
pub fn published() -> Option<SessionLiveness> {
    #[cfg(test)]
    {
        test_support::published()
    }
    #[cfg(not(test))]
    {
        session_state::latest(session_state::LATEST_MAX_AGE).map(SessionLiveness)
    }
}

static WATCH_RUNS_HERE: AtomicBool = AtomicBool::new(false);

/// Declare that this process runs the session watch, so [`for_selector`]
/// reads what it publishes and never forks `docker` here. Called once where
/// the daemon starts the watch.
pub fn mark_watch_runs_here() {
    WATCH_RUNS_HERE.store(true, Ordering::Release);
}

/// The account selector's read for `inventory`: `None` without starting any
/// process when no enabled Codex account is session-managed; else the
/// published snapshot (in the daemon), else one bounded snapshot.
#[must_use]
pub fn for_selector(inventory: &[AccountDescriptor]) -> Option<SessionLiveness> {
    let any_session_managed = inventory.iter().any(|account| {
        account.id.provider == AccountProvider::Codex
            && account.enabled
            && is_session_managed(&account.credential_reference)
    });
    if !any_session_managed {
        return None;
    }
    #[cfg(test)]
    let (watched, docker) = test_support::selector();
    #[cfg(not(test))]
    let (watched, docker) = (
        WATCH_RUNS_HERE.load(Ordering::Acquire),
        Some(
            std::env::var("LOOM_CODEX_SESSION_DOCKER")
                .ok()
                .filter(|docker| !docker.is_empty())
                .unwrap_or_else(|| "docker".to_string()),
        ),
    );
    selector_read(published(), watched, docker.as_deref())
}

/// [`for_selector`]'s decision. `published` wins; a process that runs the
/// watch stops there; only a process with neither takes one snapshot from
/// `docker` (`None`: no binary to ask — cannot observe).
fn selector_read(
    published: Option<SessionLiveness>,
    watched: bool,
    docker: Option<&str>,
) -> Option<SessionLiveness> {
    if published.is_some() || watched {
        return published;
    }
    let snapshot = session_state::snapshot(docker?, &[], session_state::SNAPSHOT_DEADLINE);
    if let Snapshot::Unavailable(reason) = &snapshot {
        log::warn!(
            "session liveness: Codex session containers cannot be observed ({reason}) — not \
             marking any session down (fail open, #10454)"
        );
    }
    Some(SessionLiveness(Arc::new(snapshot)))
}

/// The accounts in `inventory` the selector should try first: everything
/// except session-managed accounts whose container is down. `None` when there
/// is nothing to prefer — cannot observe, no account down, or no account
/// live — so the caller selects from the full inventory exactly as before.
#[must_use]
pub fn live_preferred(inventory: &[AccountDescriptor]) -> Option<Vec<AccountDescriptor>> {
    let liveness = for_selector(inventory)?;
    let live: Vec<AccountDescriptor> = inventory
        .iter()
        .filter(|account| !is_session_down(account, Some(&liveness)))
        .cloned()
        .collect();
    let any_live_enabled = live.iter().any(|account| account.enabled);
    (live.len() < inventory.len() && any_live_enabled).then_some(live)
}

/// Test seam: under `cfg(test)` nothing here reads the process-wide published
/// snapshot (other tests publish to it) or the host's Docker. [`published`]
/// answers what the current thread installed with [`set`], by default `None`
/// (cannot observe) — so a test that marks an account session-managed for an
/// unrelated reason is unaffected by whatever containers happen to be running
/// on the test host. The selector's one-snapshot path runs only against a
/// fake `docker` a test names with [`set_selector_docker`].
#[cfg(test)]
pub(crate) mod test_support {
    use super::{container_name, Arc, SessionLiveness, SessionState, Snapshot};
    use crate::tokens_pool::session_state::Observed;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Seam {
        published: Option<SessionLiveness>,
        watched: bool,
        docker: Option<String>,
    }

    thread_local! {
        static SEAM: RefCell<Seam> = RefCell::new(Seam::default());
    }

    pub(crate) fn published() -> Option<SessionLiveness> {
        SEAM.with(|seam| seam.borrow().published.clone())
    }

    pub(crate) fn selector() -> (bool, Option<String>) {
        SEAM.with(|seam| (seam.borrow().watched, seam.borrow().docker.clone()))
    }

    /// Restores the default seam on drop, including across a panic.
    pub(crate) struct LivenessGuard;

    impl Drop for LivenessGuard {
        fn drop(&mut self) {
            SEAM.with(|seam| *seam.borrow_mut() = Seam::default());
        }
    }

    /// Answer every [`super::published`] on this thread with `liveness`
    /// (`None` = no fresh snapshot) until the guard drops.
    #[must_use]
    pub(crate) fn set(liveness: Option<SessionLiveness>) -> LivenessGuard {
        SEAM.with(|seam| {
            *seam.borrow_mut() = Seam {
                published: liveness,
                ..Seam::default()
            };
        });
        LivenessGuard
    }

    /// Nothing published; the selector may take its one snapshot from the
    /// fake `docker`, unless `watched` says this "process" runs the watch.
    #[must_use]
    pub(crate) fn set_selector_docker(docker: &str, watched: bool) -> LivenessGuard {
        SEAM.with(|seam| {
            *seam.borrow_mut() = Seam {
                published: None,
                watched,
                docker: Some(docker.to_string()),
            };
        });
        LivenessGuard
    }

    /// An available snapshot holding exactly `accounts`' containers in the
    /// given states; any other account's container is missing.
    pub(crate) fn states(accounts: &[(&str, SessionState)]) -> Option<SessionLiveness> {
        let map = accounts
            .iter()
            .map(|(name, state)| {
                let observed = Observed {
                    state: *state,
                    inspect: serde_json::Value::Null,
                };
                (container_name(name), observed)
            })
            .collect();
        Some(SessionLiveness(Arc::new(Snapshot::Available(map))))
    }

    /// A snapshot listing exactly `accounts`' session containers as running.
    pub(crate) fn running(accounts: &[&str]) -> Option<SessionLiveness> {
        let up: Vec<_> = accounts
            .iter()
            .map(|name| (*name, SessionState::Running))
            .collect();
        states(&up)
    }

    /// A fresh snapshot in which Docker could not be queried.
    pub(crate) fn unavailable() -> Option<SessionLiveness> {
        Some(SessionLiveness(Arc::new(Snapshot::Unavailable("docker ps timed out".into()))))
    }
}

#[cfg(test)]
#[path = "liveness_tests.rs"]
mod tests;
