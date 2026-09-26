//! `queue.snapshot` emission (Issue #8852, phase 2): the work finder's last
//! ranked ready queue, sent to the native HTTPS backend on the collector's
//! `host.health` cadence.
//!
//! The work-finder tick publishes its rows host-locally (phase 1,
//! `work_finder::last_tick_summary`). This module reads that slot from the
//! collector's snapshot timer instead of from the tick loop, which keeps
//! blocking forge probes out of the tick. From that slot it:
//! - maps each workspace root to its forge `owner/repo` through the
//!   collector's slug cache, plus a [`RepoVisibility`] from the TTL-cached
//!   `derive_visibility` (see [`crate::telemetry::queue_snapshot`] for the
//!   anti-leak rules);
//! - emits only when the work finder has ticked since the last emission, so a
//!   stalled work finder shows up downstream as an ageing `tick_at` and not
//!   as a re-stamped copy of an old queue;
//! - offers the record to the **non-OTLP** exporter queues only. SigNoz gets
//!   the queue as gauges (`super::ops::queue`), so an OTLP queue would carry
//!   the record only to drop it.
//!
//! With observability off, or with no HTTPS exporter configured, no sink is
//! registered and [`record`] returns before doing any work.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, Utc};

use super::queue::{DurableQueue, FanoutQueue, QueueSink};
use crate::telemetry::queue_snapshot::{
    exportable_detail, QueueRepoRef, QueueSnapshotRow, QueueStateCounts, MAX_ROWS,
};
use crate::telemetry::{QueueSnapshotRecord, TelemetryEnvelope, TelemetryRecord};
use crate::types::WorkFinderTickSummary;

/// Offers `queue.snapshot` envelopes, stamped with this daemon's host id, to
/// the native exporters' queues.
#[derive(Clone)]
pub struct QueueSnapshotSink {
    queue: Arc<dyn QueueSink>,
    host_id: String,
}

impl QueueSnapshotSink {
    #[must_use]
    pub fn new(queue: Arc<dyn QueueSink>, host_id: impl Into<String>) -> Self {
        QueueSnapshotSink {
            queue,
            host_id: host_id.into(),
        }
    }

    /// Enqueue one record.
    pub fn push(&self, record: QueueSnapshotRecord) {
        self.queue.offer(TelemetryEnvelope::new(
            self.host_id.clone(),
            TelemetryRecord::QueueSnapshot(record),
        ));
    }
}

/// The sink for a daemon whose running non-OTLP exporters' queues are
/// `native_queues`: `None` when there are none (the mirror of
/// `ops::sink_for_otlp_queues`).
#[must_use]
pub fn sink_for_native_queues(
    native_queues: Vec<Arc<DurableQueue>>,
    host_id: &str,
) -> Option<QueueSnapshotSink> {
    if native_queues.is_empty() {
        return None;
    }
    Some(QueueSnapshotSink::new(Arc::new(FanoutQueue::new(native_queues)), host_id))
}

static GLOBAL_SINK: OnceLock<QueueSnapshotSink> = OnceLock::new();

/// Register the process-global sink. Called once from [`super::spawn_task`];
/// later calls are no-ops.
pub fn register_global_sink(sink: QueueSnapshotSink) {
    let _ = GLOBAL_SINK.set(sink);
}

/// The tick timestamp of the last emitted snapshot.
static LAST_EMITTED: Mutex<Option<DateTime<Utc>>> = Mutex::new(None);

/// Whether a tick completed at `tick_at` still needs a snapshot, given the
/// last emitted tick's timestamp.
#[must_use]
pub fn is_new_tick(tick_at: DateTime<Utc>, last_emitted: Option<DateTime<Utc>>) -> bool {
    last_emitted.is_none_or(|last| tick_at > last)
}

/// Build the record for `summary`. `repos` maps each workspace root (as the
/// summary names it) to its forge slug and visibility. A root absent from
/// `repos` is unresolved, and its rows are dropped and counted. Pure.
#[must_use]
pub fn build_record(
    summary: &WorkFinderTickSummary,
    repos: &HashMap<String, QueueRepoRef>,
) -> QueueSnapshotRecord {
    let mut counts = QueueStateCounts::default();
    let mut rows = Vec::new();
    let mut unresolved_rows = 0;
    let mut rows_truncated = 0;
    for row in &summary.queue {
        match row.disposition.state() {
            "running" => counts.running += 1,
            "ready" => counts.ready += 1,
            _ => counts.blocked += 1,
        }
        let Some(repo) = repos.get(&row.repo) else {
            unresolved_rows += 1;
            continue;
        };
        if rows.len() >= MAX_ROWS {
            rows_truncated += 1;
            continue;
        }
        rows.push(QueueSnapshotRow {
            rank: row.rank,
            repo: repo.repo.clone(),
            visibility: repo.visibility,
            issue: row.issue,
            workspace_priority: row.workspace_priority,
            urgent: row.urgent,
            created_at: row.created_at.clone(),
            tier: row.tier.clone(),
            disposition: row.disposition,
            state: row.disposition.state().to_string(),
            reason: row.disposition.reason().to_string(),
            detail: exportable_detail(row.disposition, row.detail.as_deref()),
        });
    }
    let listing_failed: Vec<QueueRepoRef> = summary
        .listing_failed
        .iter()
        .filter_map(|root| repos.get(root).cloned())
        .collect();
    QueueSnapshotRecord {
        tick_at: summary.at,
        max_concurrent: summary.max_concurrent,
        seen: summary.seen,
        counts,
        listing_failed_unresolved: summary.listing_failed.len() - listing_failed.len(),
        listing_failed,
        rows,
        unresolved_rows,
        rows_truncated,
    }
}

/// Resolve every workspace root `summary` names to its slug and visibility.
/// Only absolute paths are probed. The single-workspace loop's
/// `workspace #N` placeholders stay unresolved.
async fn resolve_repos(
    summary: &WorkFinderTickSummary,
    slug_cache: &mut HashMap<String, String>,
) -> HashMap<String, QueueRepoRef> {
    let roots: BTreeSet<&str> = summary
        .queue
        .iter()
        .map(|r| r.repo.as_str())
        .chain(summary.listing_failed.iter().map(String::as_str))
        .collect();
    let mut repos = HashMap::new();
    for root in roots {
        if !Path::new(root).is_absolute() {
            continue;
        }
        let Some(slug) = super::collector::resolve_repo_slug_cached(slug_cache, root).await else {
            continue;
        };
        let visibility = super::collector::resolve_visibility(&slug).await;
        repos.insert(
            root.to_string(),
            QueueRepoRef {
                repo: slug,
                visibility,
            },
        );
    }
    repos
}

/// Emit a snapshot of the last work-finder tick when a native sink is
/// registered and the work finder has ticked since the previous snapshot,
/// with the managed repos' forge-side `loom:blocked` issues appended
/// (Issue #8957, [`super::queue_blocked`]).
pub(super) async fn record(
    workspace_pool: &crate::workspace_pool::WorkspacePool,
    slug_cache: &mut HashMap<String, String>,
) {
    let Some(sink) = GLOBAL_SINK.get() else {
        return;
    };
    let Some(summary) = crate::work_finder::last_tick_summary() else {
        return;
    };
    let last = *LAST_EMITTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !is_new_tick(summary.at, last) {
        return;
    }
    let repos = resolve_repos(&summary, slug_cache).await;
    let mut record = build_record(&summary, &repos);
    let blocked = super::queue_blocked::collect(workspace_pool, slug_cache).await;
    super::queue_blocked::append(&mut record, blocked);
    sink.push(record);
    *LAST_EMITTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(summary.at);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "queue_snapshot_tests.rs"]
mod tests;
