//! Daemon lifecycle records: `daemon.start`, `daemon.shutdown`,
//! `daemon.heartbeat` (Issue #10023).
//!
//! Before this, nothing in SigNoz could say whether a host's daemon was
//! running: no span or log named start/stop/heartbeat, and `host.health`
//! reached the OTLP path only as gauges. A host whose spans simply stopped was
//! indistinguishable across "daemon stopped", "laptop asleep" and "export
//! failing". These three records answer that, as `daemon.event` logs (the
//! existing [`DaemonEventRecord`] wrapper, so they ride every configured
//! exporter with no new record kind) whose `loom.topic` is the event name:
//!
//! - **`daemon.start`** — enqueued once when the exporter comes up: version,
//!   full build commit + tree state, supervisor (`launchd`/`systemd`/`none`),
//!   pid.
//! - **`daemon.shutdown`** — enqueued by [`super::shutdown::exit`] on every
//!   clean exit path (signal, IPC shutdown, restart, drain, fleet stop)
//!   *before* the final bounded drain, so it is the last record exported.
//!   SIGKILL, a crash or a power loss cannot run it — that is the point:
//!   its absence is what "silent" means.
//! - **`daemon.heartbeat`** — every [`resolve_heartbeat_interval`] (default
//!   [`DEFAULT_HEARTBEAT_SECS`]), carrying export health per exporter —
//!   `last_success_at`, `queued`, `dropped` — plus their aggregates. A
//!   heartbeat is queued like any record, so after an export outage the
//!   backlog lands with its original timestamps and a `last_success_at` far
//!   behind its own: "export failing", not "host down".
//!
//! Last state for a host is then: latest of the three is `daemon.shutdown` ⇒
//! **stopped**; latest heartbeat newer than ~3 intervals ⇒ **running**;
//! otherwise **silent since** that heartbeat. The SigNoz recipe is in
//! `defaults/docs/observability.md` §"Daemon lifecycle".

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::telemetry::{DaemonEventRecord, TelemetryEnvelope, TelemetryRecord};

use super::queue::{DurableQueue, QueueSink};
use super::ExportStatus;

pub const TOPIC_START: &str = "daemon.start";
pub const TOPIC_SHUTDOWN: &str = "daemon.shutdown";
pub const TOPIC_HEARTBEAT: &str = "daemon.heartbeat";

/// `observability.heartbeatSecs` env override.
pub const HEARTBEAT_SECS_ENV: &str = "LOOM_OBSERVABILITY_HEARTBEAT_SECS";
/// Default heartbeat cadence: low-rate (one log record per host every two
/// minutes), still fine-grained enough that "silent" is visible within
/// minutes.
pub const DEFAULT_HEARTBEAT_SECS: u64 = 120;

/// **env > config (`observability.heartbeatSecs`) > default**. Zero or
/// unparseable values fall through, never a busy loop.
#[must_use]
pub fn resolve_heartbeat_interval(root: &Path) -> Duration {
    let from_env = std::env::var(HEARTBEAT_SECS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0);
    let from_config = || {
        let config = crate::config_resolver::resolve_effective_config(root);
        crate::config_resolver::get_path(&config, "observability.heartbeatSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|v| *v > 0)
    };
    Duration::from_secs(
        from_env
            .or_else(from_config)
            .unwrap_or(DEFAULT_HEARTBEAT_SECS),
    )
}

/// One exporter's health inputs, read (never mutated) by the heartbeat.
#[derive(Clone)]
pub struct ExportHealthSource {
    pub name: String,
    pub queue: Arc<DurableQueue>,
    pub status: Arc<ExportStatus>,
}

fn record(topic: &str, payload: serde_json::Value) -> TelemetryRecord {
    TelemetryRecord::DaemonEvent(DaemonEventRecord {
        topic: topic.to_string(),
        payload,
    })
}

/// `"launchd"`/`"systemd"`, or `"none"` for an unsupervised daemon.
#[must_use]
pub fn supervisor() -> String {
    crate::ipc::detect_supervisor().unwrap_or_else(|| "none".to_string())
}

/// The `daemon.start` record.
#[must_use]
pub fn start_record(supervisor: &str) -> TelemetryRecord {
    record(
        TOPIC_START,
        serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "build_commit": crate::self_update::BUILT_COMMIT_FULL,
            "build_tree_state": crate::self_update::BUILT_TREE_STATE,
            "supervisor": supervisor,
            "pid": std::process::id(),
        }),
    )
}

/// A coarse, stable name for a daemon exit code (the codes themselves are
/// shared: 143 is both SIGTERM and an IPC shutdown/drain-then-exit).
#[must_use]
pub fn exit_reason(code: i32) -> &'static str {
    match code {
        crate::ipc::EXIT_RESTART => "restart",
        crate::ipc::EXIT_SIGINT => "sigint",
        crate::ipc::EXIT_SHUTDOWN => "stop",
        crate::fleet_state::EXIT_FLEET_STOPPED => "fleet_stopped",
        _ => "exit",
    }
}

/// The `daemon.shutdown` record for a clean exit with `code`.
#[must_use]
pub fn shutdown_record(code: i32, uptime: Duration) -> TelemetryRecord {
    record(
        TOPIC_SHUTDOWN,
        serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "build_commit": crate::self_update::BUILT_COMMIT_FULL,
            "exit_code": code,
            "reason": exit_reason(code),
            "clean": true,
            "uptime_sec": uptime.as_secs(),
            "pid": std::process::id(),
        }),
    )
}

/// The `daemon.heartbeat` record: per-exporter export health plus the
/// aggregates a query can read without unpacking the list —
/// `last_success_at` / `last_failure_at` (most recent across exporters,
/// `null` if none ever happened), `queued` and `dropped` (sums).
#[must_use]
pub fn heartbeat_record(sources: &[ExportHealthSource], uptime: Duration) -> TelemetryRecord {
    let mut last_success_at: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut last_failure_at: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut queued_total: u64 = 0;
    let mut dropped_total: u64 = 0;
    let exports: Vec<serde_json::Value> = sources
        .iter()
        .map(|source| {
            let snapshot = source.status.snapshot();
            let queued = source.queue.len() as u64;
            let dropped = source.queue.dropped_total();
            queued_total = queued_total.saturating_add(queued);
            dropped_total = dropped_total.saturating_add(dropped);
            if let Some(at) = snapshot.last_success_at {
                last_success_at = Some(last_success_at.map_or(at, |prev| prev.max(at)));
            }
            if let Some(at) = snapshot.last_failure_at {
                last_failure_at = Some(last_failure_at.map_or(at, |prev| prev.max(at)));
            }
            serde_json::json!({
                "exporter": source.name,
                "state": snapshot.state,
                "last_success_at": snapshot.last_success_at,
                "last_failure_at": snapshot.last_failure_at,
                "consecutive_failures": snapshot.consecutive_failures,
                "queued": queued,
                "dropped": dropped,
            })
        })
        .collect();
    record(
        TOPIC_HEARTBEAT,
        serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_sec": uptime.as_secs(),
            "last_success_at": last_success_at,
            "last_failure_at": last_failure_at,
            "queued": queued_total,
            "dropped": dropped_total,
            "exports": exports,
        }),
    )
}

/// Where lifecycle records go: the exporter fan-out queue and this host's id.
struct Sink {
    queue: Arc<dyn QueueSink>,
    host_id: String,
    started: Instant,
}

/// Process-global so [`super::shutdown::exit`] — reached from signal, IPC and
/// drain paths that hold no observability handle — can enqueue the shutdown
/// record. Last-wins, like `GLOBAL_EXPORT_STATUSES`, so in-process tests
/// observe their own registration.
static SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// Register the lifecycle sink and enqueue `daemon.start`.
pub fn start(queue: Arc<dyn QueueSink>, host_id: String, started: Instant) {
    queue.offer(TelemetryEnvelope::new(host_id.clone(), start_record(&supervisor())));
    *SINK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Sink {
        queue,
        host_id,
        started,
    });
}

/// Enqueue `daemon.shutdown` for exit `code`, durably, if a sink is
/// registered. Called before the final drain. Returns whether a record was
/// enqueued. Takes the sink, so a second call (two racing exit paths) cannot
/// enqueue a second record.
pub fn record_shutdown(code: i32) -> bool {
    let sink = SINK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let Some(sink) = sink else {
        return false;
    };
    let envelope =
        TelemetryEnvelope::new(sink.host_id.clone(), shutdown_record(code, sink.started.elapsed()));
    if let Err(error) = sink.queue.offer_durable(envelope) {
        log::warn!("observability: could not persist the daemon.shutdown record: {error}");
    }
    true
}

/// Spawn the heartbeat loop. The first heartbeat fires one `interval` after
/// start (`daemon.start` already marks the beginning).
pub fn spawn_heartbeat(
    queue: Arc<dyn QueueSink>,
    host_id: String,
    sources: Vec<ExportHealthSource>,
    interval: Duration,
    started: Instant,
) -> tokio::task::JoinHandle<()> {
    log::info!("observability: daemon.heartbeat every {}s", interval.as_secs());
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            queue.offer(TelemetryEnvelope::new(
                host_id.clone(),
                heartbeat_record(&sources, started.elapsed()),
            ));
        }
    })
}

#[cfg(test)]
#[path = "daemon_lifecycle_tests.rs"]
mod tests;
