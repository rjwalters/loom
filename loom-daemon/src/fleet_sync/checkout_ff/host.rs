//! The host side of the checkout fast-forward (#10869): the hold that keeps
//! it apart from the daemon's self-update, the record of what the workspace
//! half already asked the remote, and the glue that runs a pass from the
//! startup pass and after each workspace pass with the daemon's real inputs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use super::{CheckoutPass, Env, Memory, Transition, PASS_BUDGET, STARTUP_BUDGET, TOPIC};
use crate::event_bus::EventBus;
use crate::fleet_state::{Enforcement, Enforcer};
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
// What the workspace half already asked the remote
// ============================================================================

type Heard = (String, String, DateTime<Utc>);

fn remote_heads() -> &'static Mutex<HashMap<PathBuf, Heard>> {
    static HEADS: OnceLock<Mutex<HashMap<PathBuf, Heard>>> = OnceLock::new();
    HEADS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record that the remote just named `head` as `branch`'s head in `root`.
/// Called by the workspace half ([`crate::fleet_sync::workspace_resync`])
/// after each `git ls-remote`, so the checkout half does not ask the same
/// remote the same question.
pub(in crate::fleet_sync) fn note_remote_head(root: &Path, branch: &str, head: &str) {
    if let Ok(mut heads) = remote_heads().lock() {
        heads.insert(root.to_path_buf(), (branch.to_string(), head.to_string(), Utc::now()));
    }
}

/// The head the workspace half last heard for `root`'s `branch`, and when.
fn heard(root: &Path, branch: &str) -> Option<super::Confirmed> {
    let heads = remote_heads().lock().ok()?;
    let (b, head, at) = heads.get(root)?;
    (b == branch).then(|| (head.clone(), *at))
}

// ============================================================================
// Running a pass
// ============================================================================

fn memory() -> &'static Mutex<Memory> {
    static MEMORY: OnceLock<Mutex<Memory>> = OnceLock::new();
    MEMORY.get_or_init(|| Mutex::new(Memory::default()))
}

/// One checkout pass over the registered workspaces. Blocking (git children),
/// so it runs on a blocking thread.
fn run_live(
    inputs: &PassInputs,
    network: bool,
    budget: Duration,
    gate: Option<&GateProbe>,
) -> CheckoutPass {
    let started = Instant::now();
    let env = Env {
        write: inputs.auto_apply,
        gate_in_flight: &|root| gate.is_some_and(|probe| probe(root)),
        hold: &hold_for_move,
        network,
        recheck: crate::fleet_sync::workspace_resync::recheck_after(inputs.interval),
        confirmed: &heard,
        budget: Some(budget),
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
/// alone. A `paused` host, like one with `fleet.autoApply` off, asks no
/// remote: it compares with `origin/<default>` as the clone has it.
pub(in crate::fleet_sync) fn startup(
    inputs: &PassInputs,
    status: &mut FleetSyncStatus,
) -> Vec<Transition> {
    if status.enforced == Enforcement::Stop {
        return Vec::new();
    }
    let network = inputs.auto_apply && status.enforced == Enforcement::Proceed;
    let pass = run_live(inputs, network, STARTUP_BUDGET, None);
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

/// The checkout half on the timer, as the step that follows a workspace pass
/// (`workspace_resync::spawn_pass`'s `then`). It runs on that pass's own task
/// and inside its single flight, so:
///
/// - the fleet-sync timer never waits for it;
/// - it follows any resync the pass pushed, and that push already moved
///   `origin/<default>` in this clone, so the host that pushed fast-forwards
///   to it in the same pass with nothing to fetch;
/// - it never overlaps the next workspace pass.
///
/// It asks no remote when `fleet.autoApply` is off or dispatch is paused (a
/// drain, a roll's pause or a fleet hold), and none without an enforcer to
/// read that from.
pub(in crate::fleet_sync) fn after_resync(
    inputs: &PassInputs,
    enforcer: &Option<Arc<dyn Enforcer>>,
    gate: &Option<GateProbe>,
    bus: &Option<Arc<EventBus>>,
) -> impl FnOnce() + Send + 'static {
    let (inputs, enforcer, gate, bus) =
        (inputs.clone(), enforcer.clone(), gate.clone(), bus.clone());
    move || {
        let paused = enforcer.as_deref().is_none_or(|e| e.drain_facts().0);
        let network = inputs.auto_apply && !paused;
        let found = run_live(&inputs, network, PASS_BUDGET, gate.as_ref());
        announce(&found.transitions, bus.as_deref());
        crate::fleet_sync::publish_checkouts(&found.checkouts);
    }
}
