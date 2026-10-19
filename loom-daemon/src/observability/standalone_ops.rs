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
//! what it could not flush stays queued for the next call.

use std::path::{Path, PathBuf};
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

/// What one [`deliver`] call came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// Every exporter's queue is empty: the record (and anything an earlier
    /// call left) was exported.
    Exported,
    /// The record is durably queued but not all of it was exported within the
    /// timeout; the next [`deliver`] (or [`flush_pending`]) retries it.
    Queued,
}

/// Write `record` durably into each OTLP exporter's queue of a process tagged
/// `tag`, then try once, for at most `timeout`, to export everything queued.
/// For a process too short-lived to run a sender (the watchdog, #12806).
///
/// # Errors
/// No OTLP exporter is configured, or no queue could be written durably.
pub fn deliver(
    config: &ObservabilityConfig,
    workspace_root: &Path,
    tag: &str,
    record: TelemetryRecord,
    timeout: Duration,
) -> Result<Delivery, String> {
    let planned = plan(config, tag);
    if planned.is_empty() {
        return Err("no OTLP exporter is configured".to_string());
    }
    let capacity = resolve_queue_capacity(config);
    let host_id = crate::sweep_registry::host_identity();
    let mut written = 0usize;
    let mut last_error = String::new();
    for p in &planned {
        let queue = open_queue(workspace_root, tag, p.index, capacity);
        match queue.push_durable(TelemetryEnvelope::new(host_id.clone(), record.clone())) {
            Ok(()) => written += 1,
            Err(e) => {
                last_error = format!("{}: {e}", queue_path(workspace_root, tag, p.index).display())
            }
        }
    }
    if written == 0 {
        return Err(format!("could not queue the record durably ({last_error})"));
    }
    Ok(flush(config, workspace_root, tag, planned, timeout))
}

/// Try once, for at most `timeout`, to export whatever earlier [`deliver`]
/// calls of a process tagged `tag` left queued. `None` when no OTLP exporter
/// is configured.
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
    Some(flush(config, workspace_root, tag, planned, timeout))
}

/// Drain each planned exporter's queue on a private runtime and thread,
/// waiting at most `timeout`. A queue whose export fails keeps its records.
fn flush(
    config: &ObservabilityConfig,
    workspace_root: &Path,
    tag: &str,
    planned: Vec<Planned>,
    timeout: Duration,
) -> Delivery {
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
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                let _ = tx.send(false);
                return;
            };
            let mut all_empty = true;
            for p in planned {
                let queue = open_queue(&root, &tag, p.index, capacity);
                let status = ExportStatus::started(
                    &host_id,
                    &p.endpoint,
                    ExporterKind::Otlp.name(),
                    flush_secs,
                );
                loop {
                    match rt.block_on(sender::try_flush(&queue, &p.exporter, batch, &status)) {
                        sender::FlushOutcome::Sent(_) => {}
                        sender::FlushOutcome::Empty => break,
                        sender::FlushOutcome::Failed => {
                            all_empty = false;
                            break;
                        }
                    }
                }
            }
            let _ = tx.send(all_empty);
        });
    if spawned.is_err() {
        return Delivery::Queued;
    }
    match rx.recv_timeout(timeout) {
        Ok(true) => Delivery::Exported,
        _ => Delivery::Queued,
    }
}
