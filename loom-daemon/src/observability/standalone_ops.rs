//! The ops sink for a standalone process that is not the daemon (#12449).
//!
//! [`super::spawn_task`] registers the process-global ops sink inside the
//! daemon, alongside the collector and every other producer. A separate
//! long-running command (the fleet-dispatch captain server) wants only the
//! ops path: its spans and counters, over the same OTLP exporters the
//! workspace config names, with no collector and no event bus.
//!
//! [`start`] runs the same per-entry policy pass and ingest-key resolution as
//! the daemon (an entry the daemon would refuse is refused here too), opens a
//! durable queue per OTLP exporter under a file name tagged with the
//! process's `tag` (never the daemon's own queue file, which a daemon on the
//! same host owns), spawns a sender for each, and registers the global ops
//! sink. HTTPS (native ingest) entries are skipped: ops signals are
//! OTLP-only. Returns the sender handles, empty when nothing started.
//!
//! [`deliver`] is the one-shot form (#12806), for a process too short-lived
//! to run a sender: the watchdog's alerts. It writes one record durably into
//! the same tagged queue files and flushes them once, bounded by a timeout;
//! what it could not flush stays queued for the next call. It reports which
//! exporters drained, so a caller can tell a dead exporter from a live one
//! (#12831), and it is single-flight: while an earlier flush still runs
//! (it outlived its timeout) no second call touches the queue files.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::ingest_key::{resolve_for_planned, PlannedKey};
use super::queue::DurableQueue;
use super::{
    entry_endpoint, ops, otlp, resolve_batch_size, resolve_enabled, resolve_endpoint,
    resolve_exporters, resolve_flush_interval_secs, resolve_queue_capacity, sender, ExportStatus,
    ExporterKind, ObservabilityConfig,
};
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

/// One OTLP exporter a standalone process may run: its position in the
/// config (the queue file's index), endpoint and built exporter.
struct Planned {
    index: usize,
    endpoint: String,
    exporter: otlp::OtlpExporter,
}

/// The OTLP exporters `config` would run in a process tagged `tag`, with the
/// daemon's own per-entry policy and ingest-key resolution. Empty when
/// observability is off or no OTLP entry survives.
fn plan(config: &ObservabilityConfig, tag: &str) -> Vec<Planned> {
    if !resolve_enabled(config) {
        return Vec::new();
    }
    let shared = resolve_endpoint(config);
    let entries = resolve_exporters(config);
    let planned: Vec<_> = entries
        .iter()
        .filter(|e| e.kind == ExporterKind::Otlp)
        .filter_map(|e| match entry_endpoint(e, shared.as_ref()) {
            Ok(endpoint) => Some((e, endpoint)),
            Err((_, detail)) => {
                log::warn!("{tag}: OTLP exporter refused — {detail}");
                None
            }
        })
        .collect();
    if planned.is_empty() {
        return Vec::new();
    }
    let key = match resolve_for_planned(config, &planned) {
        PlannedKey::Key(k) => Some(k),
        PlannedKey::NotNeeded => None,
        PlannedKey::Missing(detail) => {
            log::warn!("{tag}: ingest key unavailable ({detail}); exporters that need it are off");
            None
        }
    };
    let mut out = Vec::new();
    for (index, (entry, endpoint)) in planned.into_iter().enumerate() {
        if key.is_none() && entry.headers_file.is_none() {
            continue;
        }
        match otlp::OtlpExporter::with_headers_file(
            endpoint.clone(),
            key.clone(),
            entry.headers_file.as_deref(),
        ) {
            Ok(exporter) => out.push(Planned {
                index,
                endpoint,
                exporter,
            }),
            Err(e) => log::warn!("{tag}: OTLP exporter for {endpoint} not started: {e}"),
        }
    }
    out
}

/// The queue file for exporter `index` of a process tagged `tag`.
fn queue_path(workspace_root: &Path, tag: &str, index: usize) -> PathBuf {
    workspace_root
        .join(".loom")
        .join("logs")
        .join(format!("{tag}-ops-queue.{index}.jsonl"))
}

fn open_queue(workspace_root: &Path, tag: &str, index: usize, capacity: usize) -> DurableQueue {
    let path = queue_path(workspace_root, tag, index);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    DurableQueue::open(path, capacity)
}

/// Start OTLP export of ops signals for a standalone process at
/// `workspace_root`, its queue files tagged `tag`.
#[must_use]
pub fn start(
    config: &ObservabilityConfig,
    workspace_root: &Path,
    tag: &str,
) -> Vec<tokio::task::JoinHandle<()>> {
    let planned = plan(config, tag);
    if planned.is_empty() {
        return Vec::new();
    }
    let host_id = crate::sweep_registry::host_identity();
    let batch = resolve_batch_size(config);
    let flush = Duration::from_secs(resolve_flush_interval_secs(config));
    let capacity = resolve_queue_capacity(config);
    let mut handles = Vec::new();
    let mut queues = Vec::new();
    for Planned {
        index,
        endpoint,
        exporter,
    } in planned
    {
        let queue = Arc::new(open_queue(workspace_root, tag, index, capacity));
        let status = Arc::new(ExportStatus::started(
            &host_id,
            &endpoint,
            ExporterKind::Otlp.name(),
            flush.as_secs(),
        ));
        status.attach_queue(queue.clone());
        handles.push(sender::spawn_task(queue.clone(), exporter, batch, flush, status));
        queues.push(queue);
    }
    if let Some(sink) = ops::sink_for_otlp_queues(queues, &host_id) {
        ops::register_global_ops_sink(sink);
        log::info!("{tag}: exporting ops spans and counters over OTLP");
    }
    handles
}

/// What one [`deliver`] or [`flush_pending`] call came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// Every exporter's queue is empty: the record (and anything an earlier
    /// call left) was exported.
    Exported,
    /// The record is durably queued but not all of it was exported within the
    /// timeout; the next [`deliver`] (or [`flush_pending`]) retries it.
    Queued,
}

/// Per exporter (by its index in the config's OTLP entries), what a
/// [`deliver`] came to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// The targeted exporters whose queue drained: they have the record.
    pub drained: Vec<usize>,
    /// The targeted exporters still holding it (failed, or out of time).
    pub pending: Vec<usize>,
}

impl Report {
    /// Everything targeted drained.
    #[must_use]
    pub fn delivery(&self) -> Delivery {
        if self.pending.is_empty() {
            Delivery::Exported
        } else {
            Delivery::Queued
        }
    }
}

/// Set while a flush thread runs: it can outlive its caller's timeout, and a
/// second writer on the same queue files would rewrite them from a stale
/// in-memory copy and drop a record (#12831).
static FLUSH_BUSY: AtomicBool = AtomicBool::new(false);

/// Held by whoever may write the queue files; released on drop.
struct Busy;

impl Busy {
    fn acquire() -> Option<Busy> {
        // Lazy: an eagerly built `Busy` would clear the flag on its drop.
        (!FLUSH_BUSY.swap(true, Ordering::AcqRel)).then(|| Busy)
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        FLUSH_BUSY.store(false, Ordering::Release);
    }
}

/// Queue `record` durably for a process tagged `tag`, then try once, for at
/// most `timeout`, to export what each targeted exporter has queued. For a
/// process too short-lived to run a sender (the watchdog, #12806).
///
/// `only` names the exporters that get the record (all when `None`).
/// Everywhere, `supersedes` first removes the queued records it matches, so a
/// record about an incident replaces the older ones instead of piling up
/// behind a dead exporter, and an exporter left out never sends a stale one.
///
/// # Errors
/// No OTLP exporter is configured, no queue could be written durably, or an
/// earlier flush still runs (nothing was touched; try again later).
pub fn deliver(
    config: &ObservabilityConfig,
    workspace_root: &Path,
    tag: &str,
    record: &TelemetryRecord,
    only: Option<&[usize]>,
    supersedes: impl Fn(&TelemetryRecord) -> bool,
    timeout: Duration,
) -> Result<Report, String> {
    let Some(busy) = Busy::acquire() else {
        return Err("an earlier flush is still running".to_string());
    };
    let planned = plan(config, tag);
    if planned.is_empty() {
        return Err("no OTLP exporter is configured".to_string());
    }
    deliver_planned(config, workspace_root, tag, planned, record, only, supersedes, timeout, busy)
}

#[allow(clippy::too_many_arguments)]
fn deliver_planned(
    config: &ObservabilityConfig,
    workspace_root: &Path,
    tag: &str,
    planned: Vec<Planned>,
    record: &TelemetryRecord,
    only: Option<&[usize]>,
    supersedes: impl Fn(&TelemetryRecord) -> bool,
    timeout: Duration,
    busy: Busy,
) -> Result<Report, String> {
    let capacity = resolve_queue_capacity(config);
    let host_id = crate::sweep_registry::host_identity();
    let mut targets = Vec::new();
    let mut unqueued = Vec::new();
    let mut last_error = String::new();
    for p in planned {
        let queue = open_queue(workspace_root, tag, p.index, capacity);
        queue.retain(|e| !supersedes(&e.record));
        if only.is_some_and(|only| !only.contains(&p.index)) {
            continue;
        }
        match queue.push_durable(TelemetryEnvelope::new(host_id.clone(), record.clone())) {
            Ok(()) => targets.push(p),
            Err(e) => {
                last_error = format!("{}: {e}", queue_path(workspace_root, tag, p.index).display());
                unqueued.push(p.index);
            }
        }
    }
    if targets.is_empty() && !unqueued.is_empty() {
        return Err(format!("could not queue the record durably ({last_error})"));
    }
    let mut report = flush(config, workspace_root, tag, targets, timeout, busy);
    // A queue that would not take the record is still owed it.
    report.pending.extend(unqueued);
    Ok(report)
}

/// Try once, for at most `timeout`, to export whatever earlier [`deliver`]
/// calls of a process tagged `tag` left queued. `None` when no OTLP exporter
/// is configured; [`Delivery::Queued`] while an earlier flush still runs.
#[must_use]
pub fn flush_pending(
    config: &ObservabilityConfig,
    workspace_root: &Path,
    tag: &str,
    timeout: Duration,
) -> Option<Delivery> {
    let planned = plan(config, tag);
    if planned.is_empty() {
        return None;
    }
    let Some(busy) = Busy::acquire() else {
        return Some(Delivery::Queued);
    };
    Some(flush(config, workspace_root, tag, planned, timeout, busy).delivery())
}

/// Drain each planned exporter's queue on a private runtime and thread,
/// waiting at most `timeout` overall; `busy` is released when the thread ends,
/// however late. A queue whose export fails keeps its records.
fn flush(
    config: &ObservabilityConfig,
    workspace_root: &Path,
    tag: &str,
    planned: Vec<Planned>,
    timeout: Duration,
    busy: Busy,
) -> Report {
    let indices: Vec<usize> = planned.iter().map(|p| p.index).collect();
    let capacity = resolve_queue_capacity(config);
    let batch = resolve_batch_size(config);
    let flush_secs = resolve_flush_interval_secs(config);
    let host_id = crate::sweep_registry::host_identity();
    let root = workspace_root.to_path_buf();
    let tag = tag.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name(format!("{tag}-otlp-flush"))
        .spawn(move || {
            let _busy = busy;
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            for p in planned {
                let queue = open_queue(&root, &tag, p.index, capacity);
                let status = ExportStatus::started(
                    &host_id,
                    &p.endpoint,
                    ExporterKind::Otlp.name(),
                    flush_secs,
                );
                let drained = loop {
                    match rt.block_on(sender::try_flush(&queue, &p.exporter, batch, &status)) {
                        sender::FlushOutcome::Sent(_) => {}
                        sender::FlushOutcome::Empty => break true,
                        sender::FlushOutcome::Failed => break false,
                    }
                };
                // Reported as each finishes, so a live exporter is not held
                // up behind a dead one that eats the timeout.
                let _ = tx.send((p.index, drained));
            }
        });
    let mut report = Report::default();
    if spawned.is_err() {
        report.pending = indices;
        return report;
    }
    let deadline = std::time::Instant::now() + timeout;
    while report.drained.len() + report.pending.len() < indices.len() {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok((index, true)) => report.drained.push(index),
            Ok((index, false)) => report.pending.push(index),
            Err(_) => break,
        }
    }
    let missing: Vec<usize> = indices
        .into_iter()
        .filter(|i| !report.drained.contains(i) && !report.pending.contains(i))
        .collect();
    report.pending.extend(missing);
    report
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::observability::RawExporterEntry;
    use serial_test::serial;
    use std::io::{Read, Write};

    const TAG: &str = "ops-test";
    const T: Duration = Duration::from_secs(10);

    /// A loopback OTLP receiver answering 200 to everything.
    fn live_endpoint() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        let Ok(n) = s.read(&mut chunk) else { return };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                            continue;
                        };
                        let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            let _ = s.write_all(
                                b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                                  content-length: 2\r\nconnection: close\r\n\r\n{}",
                            );
                            return;
                        }
                    }
                });
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    /// A loopback port nothing listens on.
    fn dead_endpoint() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port())
    }

    /// Exporter 0 is `first`, exporter 1 is `second`.
    fn config(dir: &Path, first: &str, second: &str) -> ObservabilityConfig {
        let key = dir.join("ingest.key");
        std::fs::write(&key, "k\n").unwrap();
        let raw = |endpoint: &str| RawExporterEntry {
            kind: "otlp".to_string(),
            endpoint: Some(endpoint.to_string()),
            headers_file: None,
        };
        ObservabilityConfig {
            enabled: Some(true),
            ingest_key_file: Some(key.to_string_lossy().into_owned()),
            exporters: Some(vec![raw(first), raw(second)]),
            ..Default::default()
        }
    }

    fn record(rule: &str, state: &str) -> TelemetryRecord {
        let alert = crate::alerts::Alert::new(
            rule,
            "loom-daemon",
            "t",
            crate::alerts::Severity::Critical,
            if state == "open" {
                crate::alerts::State::Open
            } else {
                crate::alerts::State::Resolved
            },
        );
        TelemetryRecord::AlertTransition(crate::alerts::record(&alert, "h", chrono::Utc::now()))
    }

    fn same_rule(rule: &'static str) -> impl Fn(&TelemetryRecord) -> bool {
        move |r| matches!(r, TelemetryRecord::AlertTransition(q) if q.rule == rule)
    }

    fn queued(dir: &Path, index: usize) -> Vec<String> {
        open_queue(dir, TAG, index, 100)
            .peek_batch(100)
            .into_iter()
            .map(|e| match e.record {
                TelemetryRecord::AlertTransition(q) => format!("{}:{}", q.rule, q.state),
                _ => "other".to_string(),
            })
            .collect()
    }

    /// Two OTLP exporters (`resolve_exporters` keeps one per kind today, so
    /// the config path cannot plan two): index 0 at `first`, 1 at `second`.
    fn put(
        dir: &Path,
        endpoints: [&str; 2],
        record: &TelemetryRecord,
        only: Option<&[usize]>,
        rule: &'static str,
    ) -> Result<Report, String> {
        let planned = endpoints
            .iter()
            .enumerate()
            .map(|(index, endpoint)| Planned {
                index,
                endpoint: (*endpoint).to_string(),
                exporter: otlp::OtlpExporter::with_headers_file(
                    (*endpoint).to_string(),
                    Some("k".to_string()),
                    None,
                )
                .unwrap(),
            })
            .collect();
        let busy = Busy::acquire().expect("nothing else flushing");
        deliver_planned(
            &config(dir, endpoints[0], endpoints[1]),
            dir,
            TAG,
            planned,
            record,
            only,
            same_rule(rule),
            T,
            busy,
        )
    }

    #[test]
    #[serial]
    fn a_dead_exporter_does_not_stop_the_live_one_and_the_report_says_which() {
        let d = tempfile::tempdir().unwrap();
        let (dead, live) = (dead_endpoint(), live_endpoint());
        let report = put(d.path(), [&dead, &live], &record("r", "open"), None, "r").unwrap();
        assert_eq!(report.drained, vec![1]);
        assert_eq!(report.pending, vec![0]);
        assert_eq!(report.delivery(), Delivery::Queued);
        assert_eq!(queued(d.path(), 0), vec!["r:open"]);
        assert!(queued(d.path(), 1).is_empty());
        // The resolve goes to the exporter that has the open: delivered, even
        // though the other one stays dead.
        let report =
            put(d.path(), [&dead, &live], &record("r", "resolved"), Some(&[1]), "r").unwrap();
        assert_eq!(report.drained, vec![1]);
        assert!(report.pending.is_empty());
        assert_eq!(report.delivery(), Delivery::Exported);
        assert!(queued(d.path(), 0).is_empty(), "the open the dead one holds is dropped");
    }

    #[test]
    #[serial]
    fn a_record_goes_only_to_the_named_exporters_and_replaces_the_queued_one() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (dead_endpoint(), dead_endpoint());
        put(d.path(), [&a, &b], &record("r", "open"), None, "r").unwrap();
        assert_eq!(queued(d.path(), 0), vec!["r:open"]);
        assert_eq!(queued(d.path(), 1), vec!["r:open"]);
        // Re-sending replaces, not piles up.
        put(d.path(), [&a, &b], &record("r", "open"), None, "r").unwrap();
        assert_eq!(queued(d.path(), 0), vec!["r:open"]);
        // The resolve goes to exporter 1 only; exporter 0's queued open is
        // dropped, not left to go out after the incident ended.
        let report = put(d.path(), [&a, &b], &record("r", "resolved"), Some(&[1]), "r").unwrap();
        assert_eq!(report.pending, vec![1]);
        assert!(queued(d.path(), 0).is_empty());
        assert_eq!(queued(d.path(), 1), vec!["r:resolved"]);
        // An unrelated rule's queued record is left alone.
        put(d.path(), [&a, &b], &record("other", "open"), None, "other").unwrap();
        put(d.path(), [&a, &b], &record("r", "resolved"), Some(&[]), "r").unwrap();
        assert_eq!(queued(d.path(), 0), vec!["other:open"]);
        assert_eq!(queued(d.path(), 1), vec!["other:open"]);
    }

    #[test]
    #[serial]
    fn a_second_writer_is_refused_while_a_flush_runs() {
        let d = tempfile::tempdir().unwrap();
        let cfg = config(d.path(), &dead_endpoint(), &live_endpoint());
        let busy = Busy::acquire().expect("nothing else flushing");
        let err = deliver(&cfg, d.path(), TAG, &record("r", "open"), None, same_rule("r"), T)
            .unwrap_err();
        assert!(err.contains("still running"), "{err}");
        assert_eq!(flush_pending(&cfg, d.path(), TAG, T), Some(Delivery::Queued));
        assert!(!queue_path(d.path(), TAG, 0).exists(), "the queue file was not touched");
        drop(busy);
        assert!(deliver(&cfg, d.path(), TAG, &record("r", "open"), None, same_rule("r"), T).is_ok());
    }

    #[test]
    #[serial]
    fn a_flush_that_outlives_its_timeout_keeps_the_queue_files_to_itself() {
        let d = tempfile::tempdir().unwrap();
        // Accepts and never answers: the flush thread outlives a short timeout.
        let hang = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://127.0.0.1:{}", hang.local_addr().unwrap().port());
        let held = std::thread::spawn(move || {
            let conns: Vec<_> = hang.incoming().take(1).collect();
            std::thread::sleep(Duration::from_secs(3));
            drop(conns);
        });
        let cfg = config(d.path(), &endpoint, &dead_endpoint());
        let report = deliver(
            &cfg,
            d.path(),
            TAG,
            &record("r", "open"),
            None,
            same_rule("r"),
            Duration::from_millis(300),
        )
        .unwrap();
        assert_eq!(report.delivery(), Delivery::Queued);
        // Same tick, another alert: refused while the first flush still runs.
        let second = deliver(&cfg, d.path(), TAG, &record("q", "open"), None, same_rule("q"), T);
        assert!(second.is_err());
        assert_eq!(queued(d.path(), 0), vec!["r:open"], "no record dropped or added");
        held.join().unwrap();
    }
}
