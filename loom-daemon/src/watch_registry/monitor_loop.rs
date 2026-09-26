//! The watch-monitor loop's runtime wiring (Issue #3971), split out of
//! [`super`] so the module that grew a second input in #8766 is not the same
//! one already at the file-size ratchet's threshold.
//!
//! Nothing here decides anything: the tick body lives in
//! [`super::run_one_tick`], and this file is the timer, the blocking-thread
//! hand-off, and the panic log around it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::event_bus::EventBus;
use crate::forge_events::wake::EarlyTicker;

use super::{run_one_tick, WatchProbe};

/// Spawn the watch-monitor loop on the shared daemon runtime. Each tick loads the
/// persisted registry, runs one [`super::tick`] with the given `probe`, appends
/// every resolved [`super::WatchResult`] to the durable results log, and re-saves
/// the registry (only when something changed). The probe runs on a blocking
/// thread (it shells out to `gh`) so it never parks a runtime worker.
///
/// A completely empty registry short-circuits to a no-op (no forge calls), so the
/// default-on loop costs a single file read per tick until an operator registers
/// a watch.
///
/// **In-flight PR watch (#8766).** This is the daemon's only standing "has this
/// in-flight issue/PR reached terminal state yet" poller, so it is the loop
/// Phase 2 of ADR-0021 attaches its third consumer to. `event_bus` lets a
/// `forge.event` prompt of a PR-lifecycle shape (`pull_request`,
/// `check_run`/`check_suite`) run the *next ordinary tick now* rather than at
/// the cadence above — and nothing else. The tick body is untouched: it still
/// resolves terminal state by asking the forge through [`WatchProbe`], so an
/// early tick cannot report a state the forge did not just confirm, and the
/// empty-registry short-circuit means an early tick on a host with no watches
/// costs one file read and zero forge calls. Disarmed — holding no bus
/// subscription at all — unless `forgeEvents.events.inFlightPrWatch` is on for
/// `root`, in which case [`EarlyTicker`] is exactly the
/// `tokio::time::interval(interval)` this loop used before.
pub fn spawn_watch_monitor_task<P>(
    probe: P,
    interval: Duration,
    expiry: Duration,
    event_bus: Arc<EventBus>,
    root: PathBuf,
) -> tokio::task::JoinHandle<()>
where
    P: WatchProbe + Send + Sync + 'static,
{
    log::info!(
        "watch_monitor: starting loop (interval={}s, expiry={}s)",
        interval.as_secs(),
        expiry.as_secs()
    );
    tokio::spawn(async move {
        let probe = Arc::new(probe);
        let mut ticker = EarlyTicker::for_in_flight_pr(interval, &event_bus, &root);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let probe = probe.clone();
            let joined = tokio::task::spawn_blocking(move || run_one_tick(&*probe, expiry)).await;
            if let Err(e) = joined {
                log::error!("watch_monitor: tick task panicked ({e}); stopping loop");
                return;
            }
        }
    })
}
