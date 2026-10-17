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

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::ingest_key::{resolve_for_planned, PlannedKey};
use super::queue::DurableQueue;
use super::{
    entry_endpoint, ops, otlp, resolve_batch_size, resolve_enabled, resolve_endpoint,
    resolve_exporters, resolve_flush_interval_secs, resolve_queue_capacity, sender, ExportStatus,
    ExporterKind, ObservabilityConfig,
};

/// Start OTLP export of ops signals for a standalone process at
/// `workspace_root`, its queue files tagged `tag`.
#[must_use]
pub fn start(
    config: &ObservabilityConfig,
    workspace_root: &Path,
    tag: &str,
) -> Vec<tokio::task::JoinHandle<()>> {
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
    let host_id = crate::sweep_registry::host_identity();
    let batch = resolve_batch_size(config);
    let flush = Duration::from_secs(resolve_flush_interval_secs(config));
    let capacity = resolve_queue_capacity(config);
    let mut handles = Vec::new();
    let mut queues = Vec::new();
    for (index, (entry, endpoint)) in planned.into_iter().enumerate() {
        if key.is_none() && entry.headers_file.is_none() {
            continue;
        }
        let exporter = match otlp::OtlpExporter::with_headers_file(
            endpoint.clone(),
            key.clone(),
            entry.headers_file.as_deref(),
        ) {
            Ok(e) => e,
            Err(e) => {
                log::warn!("{tag}: OTLP exporter for {endpoint} not started: {e}");
                continue;
            }
        };
        let path = workspace_root
            .join(".loom")
            .join("logs")
            .join(format!("{tag}-ops-queue.{index}.jsonl"));
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let queue = Arc::new(DurableQueue::open(path, capacity));
        let status = Arc::new(ExportStatus::started(
            &host_id,
            &endpoint,
            entry.kind.name(),
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
