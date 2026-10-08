//! The host side of the checkout fast-forward (#10869): the hold that keeps
//! it apart from the daemon's self-update, the record of what the workspace
//! half already fetched, and the glue that runs a pass from the startup pass
//! and from the fleet-sync timer with the daemon's real inputs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use chrono::Utc;

use super::{CheckoutPass, Env, Memory, Transition, STARTUP_BUDGET, TOPIC};
use crate::event_bus::EventBus;
use crate::fleet_state::Enforcement;
use crate::fleet_sync::{FleetSyncStatus, PassInputs};

/// "Is a main-health gate run building in this root right now?" In production,
/// `WorkspaceHealthStates::is_gate_in_flight`.
pub type GateProbe = Arc<dyn Fn(&Path) -> bool + Send + Sync>;

// ============================================================================
// Keeping apart from the self-update
// ============================================================================

/// Which checkouts each side is acting in right now.
struct Acting {
    /// Checkouts a self-update is running in, or is waiting to start in.
    updating: Vec<PathBuf>,
    /// Checkouts a fast-forward attempt is in progress in.
    moving: Vec<PathBuf>,
}

static ACTING: Mutex<Acting> = Mutex::new(Acting {
    updating: Vec::new(),
    moving: Vec::new(),
});
static ACTING_CHANGED: Condvar = Condvar::new();

fn acting() -> MutexGuard<'static, Acting> {
    ACTING.lock().unwrap_or_else(PoisonError::into_inner)
}

fn forget(list: &mut Vec<PathBuf>, checkout: &Path) {
    if let Some(at) = list.iter().position(|p| p == checkout) {
        list.swap_remove(at);
    }
}

/// Held while the daemon's self-update script runs in a checkout. See
/// [`hold_for_self_update`].
#[must_use = "the checkout is only protected while this is alive"]
pub(crate) struct SelfUpdateHold(PathBuf);

impl Drop for SelfUpdateHold {
    fn drop(&mut self) {
        forget(&mut acting().updating, &self.0);
        ACTING_CHANGED.notify_all();
    }
}

/// Keep the fast-forward out of `checkout` for as long as the returned hold
/// lives. Called around every run of `loom-daemon-update.sh`: the script
/// verifies that the binary it built is a build of the checkout's HEAD, so
/// HEAD must not move while it runs.
///
/// Blocks while a fast-forward attempt is in progress in that checkout, for
/// at most that one attempt, so an update never starts in a checkout that is
/// halfway through a merge. No new attempt starts there once this is called.
pub(crate) fn hold_for_self_update(checkout: &Path) -> SelfUpdateHold {
    let checkout = crate::workspace_registry::normalize_path(checkout);
    let mut acting = acting();
    acting.updating.push(checkout.clone());
    while acting.moving.contains(&checkout) {
        acting = ACTING_CHANGED
            .wait(acting)
            .unwrap_or_else(PoisonError::into_inner);
    }
    SelfUpdateHold(checkout)
}

/// Held for one checkout's fast-forward attempt. See [`hold_for_move`].
#[must_use = "a self-update may start in the checkout once this is dropped"]
pub struct MoveHold(Option<PathBuf>);

impl MoveHold {
    /// A hold that excludes nothing, for a pass with no self-update to keep
    /// apart from (tests).
    pub fn free() -> Self {
        Self(None)
    }
}

impl Drop for MoveHold {
    fn drop(&mut self) {
        if let Some(checkout) = &self.0 {
            forget(&mut acting().moving, checkout);
            ACTING_CHANGED.notify_all();
        }
    }
}

/// Take the hold for a fast-forward attempt in `root`. `None` when a
/// self-update is running (or waiting to run) in that very checkout: it is
/// skipped this pass. An update running in some *other* checkout does not
/// stop this one.
pub fn hold_for_move(root: &Path) -> Option<MoveHold> {
    let checkout = crate::workspace_registry::normalize_path(root);
    let mut acting = acting();
    if acting.updating.contains(&checkout) {
        return None;
    }
    acting.moving.push(checkout.clone());
    Some(MoveHold(Some(checkout)))
}

// ============================================================================
// What the workspace half already fetched
// ============================================================================

fn fetch_log() -> &'static Mutex<HashMap<PathBuf, (String, Instant)>> {
    static LOG: OnceLock<Mutex<HashMap<PathBuf, (String, Instant)>>> = OnceLock::new();
    LOG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record that `origin/<branch>` of `root` was just fetched. Called by the
/// workspace half ([`crate::fleet_sync::workspace_resync`]), so the checkout
/// half of the same pass does not fetch the same branch again.
pub(in crate::fleet_sync) fn note_fetched(root: &Path, branch: &str) {
    if let Ok(mut log) = fetch_log().lock() {
        log.insert(root.to_path_buf(), (branch.to_string(), Instant::now()));
    }
}

/// Was `origin/<branch>` of `root` fetched at or after `since`?
fn fetched_since(root: &Path, branch: &str, since: Instant) -> bool {
    fetch_log().lock().is_ok_and(|log| {
        log.get(root)
            .is_some_and(|(b, at)| b == branch && *at >= since)
    })
}

// ============================================================================
// Running a pass
// ============================================================================

fn memory() -> &'static Mutex<Memory> {
    static MEMORY: OnceLock<Mutex<Memory>> = OnceLock::new();
    MEMORY.get_or_init(|| Mutex::new(Memory::default()))
}

/// One checkout pass over the registered workspaces. Blocking (git children),
/// so it runs on a blocking thread. `since` is when this fleet-sync pass
/// began: a fetch the workspace half made after it is reused.
fn run_live(
    write: bool,
    budget: Option<Duration>,
    since: Instant,
    gate: Option<&GateProbe>,
) -> CheckoutPass {
    let started = Instant::now();
    let env = Env {
        write,
        gate_in_flight: &|root| gate.is_some_and(|probe| probe(root)),
        hold: &hold_for_move,
        fetched: &|root, branch| fetched_since(root, branch, since),
        budget,
        elapsed: &|| started.elapsed(),
        clock: &Utc::now,
    };
    // One pass at a time: a startup pass that outlived its cap may still be
    // running when the timer's first pass begins.
    let mut guard = memory().lock().unwrap_or_else(PoisonError::into_inner);
    super::run(&env, &crate::fleet_sync::workspace_resync::registered_roots(), &mut guard)
}

/// The checkout half of the **startup pass**: runs on the startup pass's own
/// blocking thread, so it is done (or out of its [`STARTUP_BUDGET`]) before
/// `fleet_sync::start` returns and any dispatch producer exists. No gate can
/// be in flight yet: the gate task is spawned later in boot.
///
/// A host whose desired state is `stopped` is about to exit, and is left
/// alone.
pub(in crate::fleet_sync) fn startup(
    inputs: &PassInputs,
    status: &mut FleetSyncStatus,
) -> Vec<Transition> {
    if status.enforced == Enforcement::Stop {
        return Vec::new();
    }
    let pass = run_live(inputs.auto_apply, Some(STARTUP_BUDGET), Instant::now(), None);
    status.checkouts = pass.checkouts;
    pass.transitions
}

/// Publish each transition on [`TOPIC`]. The log line was written by the pass.
pub(in crate::fleet_sync) fn announce(transitions: &[Transition], bus: Option<&EventBus>) {
    let Some(bus) = bus else {
        return;
    };
    for transition in transitions {
        let payload = serde_json::to_value(transition).unwrap_or(serde_json::Value::Null);
        let _ = bus.publish_generic(TOPIC, payload);
    }
}

/// Run the checkout half from the timer task and record it in the published
/// snapshot. Called last: once at startup after the workspace half (for the
/// workspaces the startup budget did not reach, and for a resync that half
/// just pushed) and after the workspace half of every timer pass, so the host
/// that pushed a resync fast-forwards to it in the same pass.
pub(in crate::fleet_sync) async fn pass(
    inputs: &PassInputs,
    since: Instant,
    gate: &Option<GateProbe>,
    bus: &Option<Arc<EventBus>>,
) {
    let (write, gate) = (inputs.auto_apply, gate.clone());
    let found =
        tokio::task::spawn_blocking(move || run_live(write, None, since, gate.as_ref())).await;
    match found {
        Ok(found) => {
            announce(&found.transitions, bus.as_deref());
            // Read the snapshot only now: the pass above took a while, and
            // the fast-forward must not depend on a snapshot existing.
            if let Some(mut status) = crate::fleet_sync::cached_status() {
                status.checkouts = found.checkouts;
                crate::fleet_sync::publish(&status);
            }
        }
        Err(e) => log::warn!("fleet_sync: a checkout pass panicked: {e}"),
    }
}
