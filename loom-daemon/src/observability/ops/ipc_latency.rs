//! IPC request latency, by request kind (Issue #10765).
//!
//! The daemon's IPC socket answers every CLI call, the watchdog's liveness
//! probe (`quarantine list`) and fleet tooling. Before #10765 a live but slow
//! daemon was visible only as a watchdog "IPC timed out" line on the host. Each
//! connection handler now times every request from the moment its line is read
//! to the moment its response is written ([`RequestTimer`]), and this module
//! exports, per request `kind`, on its own 60 s ticker ([`spawn_task`]):
//!
//! - `loom.daemon.ipc.latency_max` (gauge, seconds): the slowest request of
//!   that kind answered in the interval.
//! - `loom.daemon.ipc.latency` (delta counter, seconds): summed latency.
//! - `loom.daemon.ipc.requests` (delta counter): requests answered. With the
//!   sum above this gives the mean.
//!
//! A kind with no requests in an interval emits no point. The `kind` label is
//! the request's wire `type` tag, read only after the frame parsed as a
//! [`crate::types::Request`], so it is one of that enum's variant names (a
//! closed, code-defined set — never a path, repo or issue). A frame that did
//! not parse is labelled `invalid`.
//!
//! Independent of the OTLP export, a request other than `DaemonStatus` that
//! takes longer than [`SLOW_REQUEST_WARN`] is logged at WARN, so a host
//! without an exporter still records the stall in `daemon.log`. `DaemonStatus`
//! is excluded there because its build already logs its own phase breakdown
//! when slow (`status_budget::record_status_build`).
//!
//! Naming: these sit beside `loom.daemon.task_alive` / `task_faults`
//! ([`super::liveness`]) under `loom.daemon.*`. The self-update loop's
//! `auto_update.tick` record (`auto_update/tick_telemetry.rs`) is a log
//! record, not a metric, and uses no `loom.daemon.ipc.*` name.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::telemetry::ops::{MetricName, MetricPoint, MetricValue};

/// The `kind` label of a frame that did not parse as a request.
pub const INVALID_KIND: &str = "invalid";

/// A non-status request slower than this is logged at WARN. Matches the 5 s
/// IPC budget the watchdog's probe reports against.
pub const SLOW_REQUEST_WARN: Duration = Duration::from_secs(5);

/// How often the series are exported.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(60);

/// One kind's requests since the previous drain.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Agg {
    count: u64,
    sum: Duration,
    max: Duration,
}

type Series = BTreeMap<String, Agg>;

#[cfg(not(test))]
fn with_store<R>(f: impl FnOnce(&mut Series) -> R) -> Option<R> {
    static STORE: std::sync::Mutex<Series> = std::sync::Mutex::new(BTreeMap::new());
    STORE.lock().ok().map(|mut s| f(&mut s))
}

/// Test builds keep the series per thread, so parallel tests never see each
/// other's requests.
#[cfg(test)]
fn with_store<R>(f: impl FnOnce(&mut Series) -> R) -> Option<R> {
    thread_local! {
        static STORE: std::cell::RefCell<Series> = const { std::cell::RefCell::new(BTreeMap::new()) };
    }
    Some(STORE.with(|s| f(&mut s.borrow_mut())))
}

/// Record one answered request of `kind` that took `elapsed`. Logs a slow
/// non-status request at WARN; accumulates for export only when ops signals
/// are exported (OTLP), so a host without an exporter pays one atomic check.
pub fn record(kind: &str, elapsed: Duration) {
    if elapsed >= SLOW_REQUEST_WARN && kind != "DaemonStatus" {
        log::warn!(
            "ipc: {kind} request took {elapsed:?} from read to response written \
             (over {SLOW_REQUEST_WARN:?}) — the daemon is alive but slow to answer (#10765)"
        );
    }
    if !super::spans_exported() {
        return;
    }
    with_store(|s| {
        let agg = s.entry(kind.to_string()).or_default();
        agg.count = agg.count.saturating_add(1);
        agg.sum = agg.sum.saturating_add(elapsed);
        agg.max = agg.max.max(elapsed);
    });
}

fn seconds(name: MetricName, d: Duration, kind: &str) -> MetricPoint {
    MetricPoint {
        name,
        value: MetricValue::Double(d.as_secs_f64()),
        labels: BTreeMap::new(),
    }
    .label("kind", kind)
}

/// Drain every kind into its three points (delta semantics: a second drain
/// with no requests in between is empty).
#[must_use]
pub fn drain_points() -> Vec<MetricPoint> {
    let drained = with_store(std::mem::take).unwrap_or_default();
    let mut points = Vec::with_capacity(drained.len() * 3);
    for (kind, agg) in drained {
        points.push(seconds(MetricName::DaemonIpcLatencyMax, agg.max, &kind));
        points.push(seconds(MetricName::DaemonIpcLatency, agg.sum, &kind));
        points.push(
            MetricPoint::int(
                MetricName::DaemonIpcRequests,
                i64::try_from(agg.count).unwrap_or(i64::MAX),
            )
            .label("kind", kind),
        );
    }
    points
}

/// Export the series accumulated since `since` (the delta counters'
/// interval start); a no-op without the ops sink.
pub fn emit(since: chrono::DateTime<chrono::Utc>) {
    let points = drain_points();
    if points.is_empty() {
        return;
    }
    if let Some(sink) = super::global_ops_sink() {
        sink.emit_metrics_since(points, Some(since));
    }
}

/// Export every [`SAMPLE_INTERVAL`]. Pure async, no blocking call.
pub fn spawn_task() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async {
        let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut since = chrono::Utc::now();
        loop {
            ticker.tick().await;
            let now = chrono::Utc::now();
            emit(since);
            since = now;
        }
    })
}

/// Times one IPC request from its line being read until the timer drops
/// (after the response is written), then [`record`]s it.
#[derive(Debug)]
pub struct RequestTimer {
    started: Instant,
    kind: Option<String>,
}

/// The wire `type` tag of a request frame.
#[derive(serde::Deserialize)]
struct WireKind {
    #[serde(rename = "type")]
    kind: String,
}

impl RequestTimer {
    /// Start timing a request whose line was just read. Until [`Self::set_kind`]
    /// is called the request is labelled [`INVALID_KIND`].
    #[must_use]
    pub fn start() -> Self {
        RequestTimer {
            started: Instant::now(),
            kind: Some(INVALID_KIND.to_string()),
        }
    }

    /// Label the request with `line`'s wire `type` tag. Call only after `line`
    /// parsed as a [`crate::types::Request`], so the tag is a variant name.
    pub fn set_kind(&mut self, line: &str) {
        if let Ok(wire) = serde_json::from_str::<WireKind>(line) {
            self.kind = Some(wire.kind);
        }
    }

    /// Do not record this request (a long-lived event subscription is not a
    /// request/response latency).
    pub fn disarm(&mut self) {
        self.kind = None;
    }
}

impl Drop for RequestTimer {
    fn drop(&mut self) {
        if let Some(kind) = self.kind.take() {
            record(&kind, self.started.elapsed());
        }
    }
}

#[cfg(test)]
mod tests;
