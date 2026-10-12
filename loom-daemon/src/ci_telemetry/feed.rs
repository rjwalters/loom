//! Push-driven CI capture: forge event feed first, repo sweep as the slow
//! correction floor (issue #9201, ADR-0021 + its 2026-09-27 amendment).
//!
//! # Why
//!
//! The repo sweep ([`super::poll::run_cycle`]) lists runs for **every repo of
//! every owner every cycle**. Its cost scales with *repos × cycles*, not with
//! *runs*, and almost every request answers "nothing new". The forge event
//! feed already reports each finished run as a `workflow_run` event, and with
//! the run's invalidation key ([`crate::forge_events::keys`]) this poller can
//! fetch exactly that run — about two requests per finished run.
//!
//! # Shape
//!
//! One loop, two inputs:
//!
//! - **Run keys** from `forge.event` pages, delivered by [`spawn_bridge`]. Each
//!   batch is recorded through [`super::poll::targeted::record_runs`], the
//!   same record path the sweep uses, within one feed poll of the page.
//! - **The sweep ticker**, at the configured `intervalSecs`. On each tick
//!   [`Driver::on_tick`] decides whether a sweep is due: at the configured
//!   interval normally, or at the slower correction floor
//!   (`feedFloorIntervalSecs`, default 60 min) **only while** the feed is
//!   driving capture.
//!
//! "The feed is driving capture" needs all three of: the consumer flag is on
//! (`forgeEvents.events.ciTelemetryRuns`, default off), the feed's status is
//! `healthy` **and fresh** ([`feed_is_live`]), and at least one run key has
//! actually arrived this process — proof that the operator's Worker forwards
//! run keys at all. Lose any one and the very next tick is back on the
//! configured interval, with no grace period: a dead feed costs nothing but
//! the return to today's polling.
//!
//! # Invariants
//!
//! 1. **Polling is never disabled.** The sweep always runs; the feed only
//!    stretches its interval, and never past [`MAX_FEED_FLOOR_INTERVAL_SECS`]
//!    — well inside the sweep's 24 h rescan window, so a run the feed dropped
//!    is always still inside the window the next floor sweep lists.
//! 2. **Exactly-once is the ledger's.** Both paths commit through
//!    `seen.jsonl`; a run recorded by one is `Seen` by the other.
//! 3. **The feed is a prompt.** A key only chooses what to fetch; see
//!    [`super::poll::targeted`] for what is re-read from the forge.
//! 4. **Off means off.** With the consumer flag off, [`super::spawn_task`]
//!    runs its pre-#9201 loop unchanged: no bus subscription, no bridge.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::poll::targeted::{self, TargetedReport};
use super::poll::{self, CycleError};
use super::{api, gate_tick, state, state_dir, ResolvedCiTelemetry, RESCAN_WINDOW_HOURS};
use crate::event_bus::EventBus;
use crate::forge_events::keys::{self, RunKey};
use crate::forge_events::wake::{self, WakeCounters, CI_TELEMETRY_RUNS};
use crate::types::{ForgeEventsState, ForgeEventsStatus};

/// `autonomous.ciTelemetry.feedFloorIntervalSecs` env override.
pub const FEED_FLOOR_INTERVAL_SECS_ENV: &str = "LOOM_CI_TELEMETRY_FEED_FLOOR_INTERVAL_SECS";

/// Default correction-floor sweep interval while the feed drives capture.
pub const DEFAULT_FEED_FLOOR_INTERVAL_SECS: u64 = 3600;

/// Upper clamp on the floor: half the rescan window, so a run the feed
/// dropped is re-listed by at least two floor sweeps before it ages out of
/// the window (invariant 1).
pub const MAX_FEED_FLOOR_INTERVAL_SECS: u64 = (RESCAN_WINDOW_HOURS as u64) * 3600 / 2;

/// A `healthy` feed status older than this (or than three of its own poll
/// intervals, whichever is longer) no longer counts as live — a feed task
/// that died would otherwise leave `healthy` frozen in place.
pub const FEED_FRESHNESS_FLOOR_SECS: i64 = 60;

/// Bridge → loop queue depth, in pages. A full queue drops the page's keys
/// (counted as `throttled`); the correction floor lists those runs later.
pub const QUEUE_PAGES: usize = 64;

/// **env > config > default**, then clamped to
/// `[interval_secs, MAX_FEED_FLOOR_INTERVAL_SECS]` — the floor can never be
/// faster than the configured sweep, nor slower than the rescan window allows.
#[must_use]
pub fn resolve_floor_interval_secs(root: &Path, interval_secs: u64) -> u64 {
    resolve_floor_interval_secs_with_env(root, interval_secs, &super::process_env)
}

/// [`resolve_floor_interval_secs`] against `env` instead of the process env
/// (#11066: tests pass a fixed map rather than writing process-global env).
#[must_use]
pub fn resolve_floor_interval_secs_with_env(
    root: &Path,
    interval_secs: u64,
    env: super::EnvLookup<'_>,
) -> u64 {
    let config = crate::config_resolver::resolve_effective_config(root);
    let configured =
        crate::config_resolver::get_path(&config, "autonomous.ciTelemetry.feedFloorIntervalSecs")
            .and_then(serde_json::Value::as_u64);
    let raw = env(FEED_FLOOR_INTERVAL_SECS_ENV)
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or(configured)
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_FEED_FLOOR_INTERVAL_SECS);
    raw.clamp(interval_secs, MAX_FEED_FLOOR_INTERVAL_SECS.max(interval_secs))
}

/// Is the feed `healthy` and has it polled recently?
#[must_use]
pub fn feed_is_live(status: &ForgeEventsStatus, now: DateTime<Utc>) -> bool {
    if status.state != ForgeEventsState::Healthy {
        return false;
    }
    let window = i64::try_from(status.poll_interval_secs.saturating_mul(3))
        .unwrap_or(i64::MAX)
        .max(FEED_FRESHNESS_FLOOR_SECS);
    status
        .last_poll_at
        .is_some_and(|at| (now - at).num_seconds() <= window)
}

/// What the loop should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Run a full repo sweep.
    Sweep,
    /// Record exactly these runs.
    Batch(Vec<RunKey>),
    /// Nothing is due.
    Idle,
}

/// The loop's decisions, free of I/O so the "feed dead ⇒ today's polling"
/// and "feed live ⇒ correction floor" properties are asserted directly.
#[derive(Debug, Clone)]
pub struct Driver {
    /// The configured sweep interval (and the ticker's period).
    pub base: Duration,
    /// The correction-floor interval used while the feed drives capture.
    pub floor: Duration,
    last_sweep: Option<Instant>,
    pending: BTreeSet<RunKey>,
}

impl Driver {
    #[must_use]
    pub fn new(base: Duration, floor: Duration) -> Self {
        Driver {
            base,
            floor: floor.max(base),
            last_sweep: None,
            pending: BTreeSet::new(),
        }
    }

    /// The sweep cadence in effect.
    #[must_use]
    pub fn cadence(&self, feed_driving: bool) -> Duration {
        if feed_driving {
            self.floor
        } else {
            self.base
        }
    }

    /// Keys waiting for a batch (queued while the cycle lock was busy).
    #[must_use]
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// A ticker tick at `at`. A due sweep wins; otherwise any keys left
    /// pending by a busy lock are retried.
    pub fn on_tick(&mut self, at: Instant, feed_driving: bool) -> Action {
        let cadence = self.cadence(feed_driving);
        // One second of slack so a tick landing a hair early on the
        // interval's own schedule does not push the sweep a whole tick late.
        let due = self.last_sweep.is_none_or(|last| {
            at.saturating_duration_since(last) + Duration::from_secs(1) >= cadence
        });
        if due {
            self.last_sweep = Some(at);
            return Action::Sweep;
        }
        self.take_batch()
    }

    /// Keys arrived from the feed.
    pub fn on_keys(&mut self, keys: impl IntoIterator<Item = RunKey>) -> Action {
        self.pending.extend(keys);
        self.take_batch()
    }

    fn take_batch(&mut self) -> Action {
        if self.pending.is_empty() {
            Action::Idle
        } else {
            Action::Batch(std::mem::take(&mut self.pending).into_iter().collect())
        }
    }

    /// A batch could not take the cycle lock — keep its keys for the next
    /// tick or page. Any other outcome is final for those keys (the floor
    /// sweep is the retry).
    pub fn requeue(&mut self, keys: Vec<RunKey>) {
        self.pending.extend(keys);
    }

    /// A sweep completed successfully: it listed every completed run in its
    /// window, so pending keys are already covered.
    pub fn sweep_succeeded(&mut self) {
        self.pending.clear();
    }
}

/// Subscribe to `forge.event` and forward each qualifying page's run keys.
///
/// A qualifying page with **no** keys (the Worker does not forward run ids)
/// counts as a prompt but proves nothing, so the sweep keeps its configured
/// interval. A page whose keys do not fit the queue is counted as throttled.
pub fn spawn_bridge(
    bus: &EventBus,
    tx: mpsc::Sender<Vec<RunKey>>,
    counters: Arc<WakeCounters>,
    proven: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    let mut subscription = bus.subscribe([crate::forge_events::BUS_TOPIC]);
    tokio::spawn(async move {
        while let Ok(event) = subscription.recv().await {
            if !wake::event_qualifies(&event, CI_TELEMETRY_RUNS.types) {
                continue;
            }
            counters.prompts.fetch_add(1, Ordering::Relaxed);
            let crate::types::Event::Generic { payload, .. } = &event else {
                continue;
            };
            let keys = keys::from_payload(payload);
            if keys.is_empty() {
                continue;
            }
            proven.store(true, Ordering::Relaxed);
            let n = keys.len() as u64;
            if tx.try_send(keys).is_err() {
                counters.throttled.fetch_add(n, Ordering::Relaxed);
            }
        }
    })
}

/// Whether `root` arms the feed consumer (**env > config > default off**).
#[must_use]
pub fn consumer_armed(root: &Path) -> bool {
    wake::resolve_enabled(&CI_TELEMETRY_RUNS, &wake::read_config(root))
}

/// Spawn the feed-driven poller loop. Called by [`super::spawn_task`] only
/// when [`consumer_armed`] — otherwise the pre-#9201 loop runs unchanged.
#[must_use]
pub fn spawn(
    root: PathBuf,
    resolved: ResolvedCiTelemetry,
    bus: &EventBus,
) -> tokio::task::JoinHandle<()> {
    let base = Duration::from_secs(resolved.interval_secs);
    let floor = Duration::from_secs(resolve_floor_interval_secs(&root, resolved.interval_secs));
    let counters = Arc::new(WakeCounters::default());
    wake::register_armed_counters(&CI_TELEMETRY_RUNS, floor, counters.clone());
    let proven = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel(QUEUE_PAGES);
    let bridge = spawn_bridge(bus, tx, counters.clone(), proven.clone());
    log::info!(
        "ci_telemetry: feed capture armed (forgeEvents.events.{}); sweep every {}s, \
         stretching to a {}s correction floor while the feed is healthy and carrying run keys",
        CI_TELEMETRY_RUNS.config_key,
        base.as_secs(),
        floor.as_secs()
    );
    tokio::spawn(async move {
        let _bridge = AbortOnDrop(bridge);
        run_loop(root, resolved, Driver::new(base, floor), rx, counters, proven).await;
    })
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn run_loop(
    root: PathBuf,
    resolved: ResolvedCiTelemetry,
    mut driver: Driver,
    mut rx: mpsc::Receiver<Vec<RunKey>>,
    counters: Arc<WakeCounters>,
    proven: Arc<AtomicBool>,
) {
    let mut ticker = tokio::time::interval(driver.base);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        // Both branches are cancel-safe (`Interval::tick`, `mpsc::recv`).
        let action = tokio::select! {
            biased;
            at = ticker.tick() => {
                let driving = proven.load(Ordering::Relaxed)
                    && feed_is_live(&crate::forge_events::global_status(), Utc::now());
                let action = driver.on_tick(at, driving);
                if action == Action::Sweep {
                    let dir = state_dir(&root);
                    let cadence = driver.cadence(driving).as_secs();
                    if let Err(error) = state::note_sweep_cadence(&dir, cadence) {
                        log::debug!("ci_telemetry: could not record the sweep cadence: {error}");
                    }
                }
                action
            }
            Some(keys) = rx.recv() => {
                let mut all = keys;
                while let Ok(more) = rx.try_recv() {
                    all.extend(more);
                }
                driver.on_keys(all)
            }
        };
        match action {
            Action::Idle => {}
            Action::Sweep => {
                if !gate_tick(&root, &crate::sweep_registry::host_identity(), Utc::now()) {
                    continue;
                }
                let (root, resolved) = (root.clone(), resolved.clone());
                let outcome = tokio::task::spawn_blocking(move || {
                    let api = api::GhCliApi::from_env();
                    poll::run_cycle(&poll::CycleContext::new(&root, &resolved), &api)
                })
                .await;
                match outcome {
                    Ok(Ok(report)) => {
                        driver.sweep_succeeded();
                        log::info!("ci_telemetry: {}", report.summary());
                    }
                    Ok(Err(error)) => log::warn!("ci_telemetry: cycle failed: {error}"),
                    Err(error) => log::warn!("ci_telemetry: cycle task panicked: {error}"),
                }
            }
            Action::Batch(keys) => {
                if !gate_tick(&root, &crate::sweep_registry::host_identity(), Utc::now()) {
                    continue;
                }
                let (root, resolved, batch) = (root.clone(), resolved.clone(), keys.clone());
                let outcome = tokio::task::spawn_blocking(move || {
                    let api = api::GhCliApi::from_env();
                    targeted::record_runs(&poll::CycleContext::new(&root, &resolved), &api, &batch)
                })
                .await;
                note_batch(&mut driver, &counters, keys, outcome);
            }
        }
    }
}

fn note_batch(
    driver: &mut Driver,
    counters: &WakeCounters,
    keys: Vec<RunKey>,
    outcome: Result<Result<TargetedReport, CycleError>, tokio::task::JoinError>,
) {
    match outcome {
        Ok(Ok(report)) => {
            counters.early_ticks.fetch_add(1, Ordering::Relaxed);
            counters
                .throttled
                .fetch_add(report.dropped.len() as u64, Ordering::Relaxed);
            for reason in &report.dropped {
                log::debug!("ci_telemetry: feed key dropped — {reason}");
            }
            log::info!("ci_telemetry: {}", report.summary_line());
        }
        Ok(Err(CycleError::Busy)) => driver.requeue(keys),
        Ok(Err(error)) => {
            log::warn!(
                "ci_telemetry: feed batch failed (the correction floor will cover it): {error}"
            );
        }
        Err(error) => log::warn!("ci_telemetry: feed batch task panicked: {error}"),
    }
}
