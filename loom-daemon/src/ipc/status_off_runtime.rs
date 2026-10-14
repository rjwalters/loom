//! Build the `DaemonStatus` report off the async runtime (Issue #10765), one
//! build per section set at a time (Issue #10861).
//!
//! [`super::build_daemon_status_for`] is synchronous and `O(registered
//! roots)`: a registry load, per-root config/filesystem reads, the
//! `role_shard::decide` walk and a `.loom/locks/` scan per root. On a busy
//! dispatcher it takes 10s to over 100s (#8163's `slow build` WARN names the
//! phases). Before #10765 the build ran inline in the per-connection task, so
//! each in-progress `status` / `health` call held one tokio **worker** for its
//! whole duration. With as many concurrent status callers as workers
//! (fleet-check, `fleet-versions.py`, `health`, an operator's `status`), every
//! worker was pinned and unrelated light requests on the same socket — the
//! watchdog's `quarantine list` probe among them — missed their budget while
//! the daemon was alive.
//!
//! The build therefore runs on the blocking pool, so a slow build only ever
//! costs a blocking-pool thread and the workers stay free for the rest of the
//! IPC surface.
//!
//! # Single-flight (#10861)
//!
//! A blocking build cannot be cancelled, and the blocking pool (512 threads)
//! is no bound. A client that timed out and retried used to leave its first
//! build running and start a second, each repeating the same registry walk
//! and slowing the next. [`StatusFlights`] coalesces them: while a build for
//! a section set is in flight, every request for the same set waits for that
//! build's report instead of starting another.
//!
//! * **Key.** The normalized [`SectionSet`]: order and duplicates do not
//!   matter, and an all-sections `DaemonStatusSections` shares the key of a
//!   plain `DaemonStatus`. Different sets never wait on each other — a cheap
//!   `daemon_build,auto_update` request does not join a full build that is
//!   20s in, even though it is a subset — so concurrent builds are bounded by
//!   the number of distinct sets in flight (a handful: full, plus each tool's
//!   fixed set).
//! * **Detached.** The build runs in its own task, not in the leading
//!   request's future. A leader whose client disconnects does not strand the
//!   requests waiting on its build.
//! * **In flight only.** A finished report is never reused: its slot is
//!   cleared before the report is published, so a request arriving after a
//!   build completed starts a new one. Status is read straight after operator
//!   actions (drain, halt, release) and must not predate them by a cache
//!   window. A request that *joins* a build can still receive a snapshot whose
//!   build began up to one build-duration before the request did.
//! * **Drain is per request.** The shared build is drain-agnostic; each
//!   request overlays the live drain state on its own copy of the report
//!   ([`super::status_scope::overlay_drain`]), so `drain` followed by `status`
//!   reports the drain even when it joins a build begun earlier.
//! * **Every request gets one frame** (#4279). A panic inside the build, a
//!   join error, and a build task that ends without publishing (its sender is
//!   dropped, e.g. the runtime aborting it at shutdown) each become an error
//!   frame for every waiting request — never a dropped socket, never a hang.
//!
//! `loom.daemon.ipc.status_builds{outcome}` counts the builds; against
//! `loom.daemon.ipc.requests{kind=DaemonStatus}` (still one per request) it
//! gives the coalescing ratio.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::watch;

use super::DrainState;
use crate::main_health_gate::WorkspaceHealthStates;
use crate::observability::ops::ipc_latency::{record_status_build_outcome, StatusBuildOutcome};
use crate::status_section::SectionSet;
use crate::types::{CredentialPreflightReport, DaemonStatusReport, Response};
use crate::workspace_pool::WorkspacePool;

/// Test-only: milliseconds every status build sleeps first, so a test can
/// hold builds in flight and probe the rest of the IPC surface meanwhile.
#[cfg(test)]
pub(super) static TEST_BUILD_DELAY_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// What one build produced: the report, shared by every request that waited
/// on it, or the cause it failed with.
type Outcome = Result<Arc<DaemonStatusReport>, String>;

/// The receiving end of one in-flight build. `None` until the build lands.
type Slot = watch::Receiver<Option<Outcome>>;

/// The cause reported when a build task ends without publishing an outcome.
const NO_OUTCOME: &str = "status build ended before producing a report";

/// The status builds in flight, at most one per section set (see the module
/// docs). One per `IpcServer`, shared by its connection handlers.
#[derive(Debug, Default)]
pub(super) struct StatusFlights {
    inflight: Mutex<HashMap<SectionSet, Slot>>,
    /// Test-only: builds this registry has started.
    #[cfg(test)]
    builds_started: std::sync::atomic::AtomicUsize,
}

impl StatusFlights {
    /// The in-flight map. Never held across an `.await`; a poisoned lock is
    /// recovered (the map is only ever inserted into and removed from).
    fn inflight(&self) -> MutexGuard<'_, HashMap<SectionSet, Slot>> {
        self.inflight.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Join the build in flight for `key`, or open a new slot and return the
    /// [`Flight`] the caller must run. Synchronous, so no caller can be
    /// cancelled between claiming a slot and owning its `Flight`.
    fn enter(self: &Arc<Self>, key: SectionSet) -> (Slot, Option<Flight>) {
        let mut inflight = self.inflight();
        if let Some(slot) = inflight.get(&key) {
            return (slot.clone(), None);
        }
        let (tx, slot) = watch::channel(None);
        inflight.insert(key.clone(), slot.clone());
        #[cfg(test)]
        self.builds_started
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let guard = SlotGuard {
            flights: Arc::clone(self),
            key,
            slot: slot.clone(),
        };
        (slot, Some(Flight { guard, tx }))
    }
}

/// Clears one flight's slot when dropped — on the normal path just before
/// the outcome is published, and also if the build task is aborted or
/// panics, so a dead flight can never be joined.
#[derive(Debug)]
struct SlotGuard {
    flights: Arc<StatusFlights>,
    key: SectionSet,
    slot: Slot,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let mut inflight = self.flights.inflight();
        // Only this flight's own entry: never a newer flight under the same key.
        if inflight
            .get(&self.key)
            .is_some_and(|current| current.same_channel(&self.slot))
        {
            inflight.remove(&self.key);
        }
    }
}

/// The leading side of one in-flight build. Dropping it without
/// [`Flight::land`] (the build task aborted) clears the slot and then drops
/// the sender, which wakes every waiter with [`NO_OUTCOME`].
#[derive(Debug)]
struct Flight {
    // Field order is drop order: the slot is cleared before the sender goes,
    // so no request can join a flight whose sender is already gone.
    guard: SlotGuard,
    tx: watch::Sender<Option<Outcome>>,
}

impl Flight {
    /// Clear the slot, then publish `outcome` to every waiter. In that order,
    /// so a finished report is never joined (no reuse window).
    fn land(self, outcome: Outcome) {
        let Flight { guard, tx } = self;
        drop(guard);
        // An error here means every waiter has gone; nobody is left to tell.
        let _ = tx.send(Some(outcome));
    }
}

/// Run `build` on the blocking pool and land its outcome on `flight`.
///
/// * The report → `Ok`, shared by every waiter.
/// * A panic inside `build` → `Err` naming the panic cause (#4279). The
///   panic is caught inside the blocking task, so no unwinding crosses the
///   `.await`.
/// * A join error (the runtime shutting down underneath the blocking task) →
///   `Err` as well.
///
/// A failure is logged at ERROR here — once per build, not once per waiter —
/// and each outcome is counted in `loom.daemon.ipc.status_builds`.
async fn run_flight<F>(flight: Flight, build: F)
where
    F: FnOnce() -> DaemonStatusReport + Send + 'static,
{
    let joined = tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(build))
    })
    .await;
    let (outcome, counted) = match joined {
        Ok(Ok(report)) => (Ok(Arc::new(report)), StatusBuildOutcome::Ok),
        Ok(Err(panic)) => (Err(super::describe_panic(panic.as_ref())), StatusBuildOutcome::Panic),
        Err(join_err) => (
            Err(format!("status build task did not complete: {join_err}")),
            StatusBuildOutcome::JoinError,
        ),
    };
    if let Err(cause) = &outcome {
        log::error!(
            "DaemonStatus handler failed while building the report: {cause}; \
             replying with an error frame instead of dropping the connection"
        );
    }
    record_status_build_outcome(counted);
    flight.land(outcome);
}

/// Wait for the build behind `slot` to land. Never hangs: the sender either
/// publishes an outcome or is dropped, and a drop wakes this with
/// [`NO_OUTCOME`].
async fn await_outcome(mut slot: Slot) -> Outcome {
    match slot.wait_for(Option::is_some).await {
        Ok(landed) => landed
            .clone()
            .unwrap_or_else(|| Err(NO_OUTCOME.to_string())),
        Err(_sender_dropped) => Err(NO_OUTCOME.to_string()),
    }
}

/// The outcome of the status build for `key`: the build already in flight
/// for that key if there is one, otherwise a new one running `build`,
/// detached from this request (see the module docs). `build` is dropped
/// unrun when the request joins an existing flight.
pub(super) async fn single_flight<F>(
    flights: &Arc<StatusFlights>,
    key: SectionSet,
    build: F,
) -> Outcome
where
    F: FnOnce() -> DaemonStatusReport + Send + 'static,
{
    let (slot, flight) = flights.enter(key);
    if let Some(flight) = flight {
        tokio::spawn(run_flight(flight, build));
    }
    await_outcome(slot).await
}

/// One request's reply frame for a build `outcome`.
///
/// * A report → `Response::DaemonStatus` (boxed, #4292): this request's own
///   copy with the live drain state overlaid.
/// * A failed build → `Response::Error` naming the cause (#4279).
fn reply(outcome: Outcome, drain: &DrainState) -> Response {
    match outcome {
        Ok(report) => {
            let mut report = Arc::unwrap_or_clone(report);
            super::status_scope::overlay_drain(&mut report, drain);
            Response::DaemonStatus(Box::new(report))
        }
        Err(cause) => Response::Error {
            message: format!("daemon failed to build status report: {cause}"),
        },
    }
}

/// The `DaemonStatus` / `DaemonStatusSections` reply for the IPC handler: the
/// CPU-sample pre-warm (#4031) and the report build, both on the blocking
/// pool and shared with every concurrent request for the same `sections`
/// (#10861), then this request's drain overlay. `sections` scopes the build
/// (#10787); the pre-warm runs only when `dynamic_cap` (the one section
/// reporting the sample) is requested.
pub(super) async fn serve(
    flights: &Arc<StatusFlights>,
    workspace_pool: &Arc<WorkspacePool>,
    health_states: &Arc<WorkspaceHealthStates>,
    fallback_root: &Path,
    credential_preflight: &Arc<CredentialPreflightReport>,
    drain_state: &Arc<DrainState>,
    sections: SectionSet,
) -> Response {
    if sections.is_empty() {
        // Not a build that reports nothing: the caller asked for no section.
        return Response::Error {
            message: "DaemonStatusSections: `sections` must name at least one section".to_string(),
        };
    }
    let (pool, health, credentials) =
        (workspace_pool.clone(), health_states.clone(), credential_preflight.clone());
    let root = fallback_root.to_path_buf();
    let scope = sections.clone();
    let outcome = single_flight(flights, sections, move || {
        // The macOS `iostat` read sleeps ~1s (#4031). A panic in it is not a
        // status failure: the build falls back to the last cached sample.
        if scope.needs_cpu_sample() {
            let _ = std::panic::catch_unwind(crate::cpu_headroom::refresh_cpu_util_cache);
        }
        #[cfg(test)]
        std::thread::sleep(std::time::Duration::from_millis(
            TEST_BUILD_DELAY_MS.load(std::sync::atomic::Ordering::SeqCst),
        ));
        super::build_daemon_status_for(&pool, &health, &root, &credentials, &scope)
    })
    .await;
    reply(outcome, drain_state)
}

#[cfg(test)]
mod flight_tests;
#[cfg(test)]
mod tests;
