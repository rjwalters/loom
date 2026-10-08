//! The host side of the checkout fast-forward (#10869): the hold that keeps
//! it apart from the daemon's self-update, the record of what the workspace
//! half already asked the remote, and the glue that runs a pass from the
//! startup pass and after each workspace pass with the daemon's real inputs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use super::{CheckoutPass, Env, Memory, Transition, PASS_BUDGET, STARTUP_BUDGET, TOPIC};
use crate::event_bus::EventBus;
use crate::fleet_state::Enforcement;
use crate::fleet_sync::workspace_resync::{host_gate, HostGateInputs, WorkspacePass};
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

/// The head the workspace half heard for `root`'s `branch` at or after
/// `since`: in the pass this step follows. `None` for anything older.
pub(super) fn heard(root: &Path, branch: &str, since: DateTime<Utc>) -> Option<String> {
    let heads = remote_heads().lock().ok()?;
    let (b, head, at) = heads.get(root)?;
    (b == branch && *at >= since).then(|| head.clone())
}

/// The rate-limit breaker's name for this step.
const CALLER: &str = "checkout_ff";

// ============================================================================
// Running a pass
// ============================================================================

fn memory() -> &'static Mutex<Memory> {
    static MEMORY: OnceLock<Mutex<Memory>> = OnceLock::new();
    MEMORY.get_or_init(|| Mutex::new(Memory::default()))
}

/// What a live pass may do, and for how long.
struct Live<'a> {
    /// `fleet.autoApply`.
    write: bool,
    /// May it use the network at all?
    network: bool,
    /// Roots the workspace resync is backing off: not asked.
    backing_off: &'a [PathBuf],
    budget: Duration,
    gate: Option<&'a GateProbe>,
    /// When the workspace pass this follows began: a head it heard after
    /// that is trusted. `None` at startup, before any workspace pass.
    since: Option<DateTime<Utc>>,
}

/// One checkout pass over `roots` with the daemon's real git, hold, breaker
/// and clock. Blocking (git children), so it runs on a blocking thread.
fn run_live(live: &Live<'_>, roots: &[PathBuf], memory: &mut Memory) -> CheckoutPass {
    let started = Instant::now();
    let env = Env {
        write: live.write,
        gate_in_flight: &|root| live.gate.is_some_and(|probe| probe(root)),
        hold: &hold_for_move,
        network: live.network,
        backing_off: &|root| live.backing_off.iter().any(|r| r == root),
        confirmed: &|root, branch| live.since.and_then(|since| heard(root, branch, since)),
        breaker_open: &|| crate::rate_limit_breaker::global_skip_pass(CALLER),
        budget: Some(live.budget),
        elapsed: &|| started.elapsed(),
        clock: &Utc::now,
    };
    super::run(&env, roots, memory)
}

/// May the startup pass's checkout half use the network? Only on a host the
/// workspace resync would itself let use it (#10869): `fleet.autoApply` on
/// and [`host_gate`] passing on what is knowable at boot
/// ([`HostGateInputs::at_boot`]: a fresh daemon is exempt from
/// [`NotCurrent::Unverified`] alone, since this is the pass that verifies).
/// No outage hold exists yet in a process that has just started.
///
/// [`NotCurrent::Unverified`]: crate::fleet_sync::workspace_resync::NotCurrent::Unverified
#[must_use]
pub(in crate::fleet_sync) fn online_at_boot(auto_apply: bool, gate: &HostGateInputs) -> bool {
    auto_apply && host_gate(gate).is_ok()
}

/// The checkout half of the **startup pass**: runs on the startup pass's own
/// blocking thread, so it is done (or out of its [`STARTUP_BUDGET`]) before
/// `fleet_sync::start` returns and any dispatch producer exists. No gate can
/// be in flight yet: the gate task is spawned later in boot.
///
/// A host whose desired state is `stopped` is about to exit, and is left
/// alone. One that may not use the network ([`online_at_boot`]) compares with
/// `origin/<default>` as the clone has it.
///
/// It waits for the pass memory: nothing can hold it this early but an
/// earlier startup pass, and that is bounded by its own budget.
pub(in crate::fleet_sync) fn startup(
    inputs: &PassInputs,
    status: &mut FleetSyncStatus,
) -> Vec<Transition> {
    if status.enforced == Enforcement::Stop {
        return Vec::new();
    }
    let paused = status.enforced != Enforcement::Proceed;
    let gate = HostGateInputs::at_boot(paused, status.floor.floor.as_deref());
    let live = Live {
        write: inputs.auto_apply,
        network: online_at_boot(inputs.auto_apply, &gate),
        backing_off: &[],
        budget: STARTUP_BUDGET,
        gate: None,
        since: None,
    };
    let roots = crate::fleet_sync::workspace_resync::registered_roots();
    let mut guard = memory().lock().unwrap_or_else(PoisonError::into_inner);
    let pass = run_live(&live, &roots, &mut guard);
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

/// What the timer's checkout step found, for [`publish`] once the workspace
/// pass it followed has been stored.
#[derive(Debug)]
pub(in crate::fleet_sync) enum Stepped {
    /// The step ran.
    Ran(CheckoutPass),
    /// An earlier step that never ended still holds the pass memory: nothing
    /// was checked this tick.
    Busy,
}

/// The status line for a tick whose step found the memory taken.
pub const BUSY_NOTE: &str = "an earlier checkout step has not ended; nothing was checked this tick";

/// The checkout step on the timer, after the workspace pass `resync` that
/// began at `began`, over the registered `roots`.
///
/// - **Its network decision is that pass's own** ([`WorkspacePass::online`]),
///   never re-derived here, so the two halves cannot disagree: a host the
///   resync keeps off the network (not in H0, an outage hold, autoApply off,
///   paused) costs no `ls-remote` and no fetch here either. Roots the pass is
///   backing off ([`WorkspacePass::backing_off`]) are not asked.
/// - **It never waits for the pass memory.** A step abandoned inside a git
///   child still holds it; waiting would park this thread too, so the
///   workspace pass it follows would never end and its result would be lost.
///   The step is skipped for the tick instead ([`Stepped::Busy`]), as the
///   resync half does.
/// - It keeps to what is left of the pass's
///   [`crate::fleet_sync::workspace_resync::PASS_DEADLINE`] and to
///   [`PASS_BUDGET`], and trusts the heads the pass learned.
pub(in crate::fleet_sync) fn timer_step(
    memory: &Mutex<Memory>,
    roots: &dyn Fn() -> Vec<PathBuf>,
    write: bool,
    resync: &WorkspacePass,
    began: Instant,
    gate: Option<&GateProbe>,
) -> Stepped {
    let mut guard = match memory.try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => {
            log::warn!("checkout_ff: {BUSY_NOTE}");
            return Stepped::Busy;
        }
    };
    let spent = began.elapsed();
    let left = crate::fleet_sync::workspace_resync::PASS_DEADLINE.saturating_sub(spent);
    let live = Live {
        write,
        network: resync.online,
        backing_off: &resync.backing_off,
        budget: PASS_BUDGET.min(left),
        gate,
        since: Some(Utc::now() - chrono::Duration::from_std(spent).unwrap_or_default()),
    };
    Stepped::Ran(run_live(&live, &roots(), &mut guard))
}

/// The two halves of the checkout step on the timer, for
/// `workspace_resync::spawn_pass`: the step itself ([`timer_step`]), run on
/// the workspace pass's own blocking thread inside the same supervised
/// closure, and what publishes its result once the workspace pass's own has
/// been stored. So:
///
/// - the fleet-sync timer never waits for it;
/// - the single flight, the stuck-pass watchdog and abandonment cover it;
/// - it follows any resync the pass pushed;
/// - it never overlaps the next workspace pass.
pub(in crate::fleet_sync) fn after_resync(
    inputs: &PassInputs,
    gate: &Option<GateProbe>,
    bus: &Option<Arc<EventBus>>,
) -> (
    impl FnOnce(Instant, &WorkspacePass) -> Stepped + Send + 'static,
    impl FnOnce(Stepped) + Send + 'static,
) {
    let (write, gate, bus) = (inputs.auto_apply, gate.clone(), bus.clone());
    let step = move |began: Instant, resync: &WorkspacePass| {
        let roots = crate::fleet_sync::workspace_resync::registered_roots;
        timer_step(memory(), &roots, write, resync, began, gate.as_ref())
    };
    (step, move |stepped| publish(stepped, bus.as_deref()))
}

/// Put a step's result on the snapshot and the event bus. A busy tick keeps
/// the last pass's reports and says why they are not new.
fn publish(stepped: Stepped, bus: Option<&EventBus>) {
    match stepped {
        Stepped::Ran(found) => {
            crate::fleet_sync::publish_checkouts(Some(&found.checkouts), None);
            announce(&found.transitions, bus);
        }
        Stepped::Busy => crate::fleet_sync::publish_checkouts(None, Some(BUSY_NOTE)),
    }
}
