//! `eta.snapshot` emission (Issue #9329): this host's live ETA estimate set,
//! sent to the native HTTPS backend on the collector's snapshot cadence.
//!
//! The ETA pass ([`super::eta::record`]) already estimates every live item on
//! that same cadence and keeps each emitted estimate in the tracker until its
//! outcome. This module is a **reader** of that state, run immediately after
//! the pass from the same timer: it has no tick loop, no forge read of its
//! own, and never touches the tracker's estimation path. From the tracker's
//! pending set it:
//!
//! - keeps only the **`current`** heuristic's newest estimate per
//!   `(repo, issue, kind)` — a shadow candidate's estimate (#9328) is never
//!   the subject's answer, and a superseded one is not current;
//! - tags each row with its repo's [`RepoVisibility`] from the TTL-cached
//!   `derive_visibility` (see [`crate::telemetry::kinds::eta_snapshot`] for
//!   the anti-leak rules);
//! - emits only when that set has **changed** since the last emission, so a
//!   stalled tracker shows up downstream as an ageing `as_of` and not as a
//!   re-stamped copy of an old estimate set;
//! - offers the record to the **non-OTLP** exporter queues only. SigNoz gets
//!   every estimate as `eta.estimate` with its whole explanation, so an OTLP
//!   queue would carry this record only to drop it.
//!
//! Three ways there is no record at all, none of which is an empty one:
//! ETA disabled (`autonomous.eta.enabled = false` — no tracker, so
//! [`super::eta::snapshot_input`] answers `None`), no HTTPS exporter
//! configured (no sink registered), and a tracker with no current estimates
//! (a fresh daemon, or every estimate resolved). A snapshot of zero rows
//! would read as "this host estimates nothing" — which is true only of the
//! first case, and unknowable from the record — so none is sent.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};

use super::queue::{DurableQueue, FanoutQueue, QueueSink};
use crate::eta::score::EstimateSummary;
use crate::eta::Kind;
use crate::telemetry::kinds::eta_snapshot::{EtaSnapshotRecord, EtaSnapshotRow, MAX_ROWS};
use crate::telemetry::{RepoVisibility, TelemetryEnvelope, TelemetryRecord};

/// Offers `eta.snapshot` envelopes, stamped with this daemon's host id, to
/// the native exporters' queues.
#[derive(Clone)]
pub struct EtaSnapshotSink {
    queue: Arc<dyn QueueSink>,
    host_id: String,
}

impl EtaSnapshotSink {
    #[must_use]
    pub fn new(queue: Arc<dyn QueueSink>, host_id: impl Into<String>) -> Self {
        EtaSnapshotSink {
            queue,
            host_id: host_id.into(),
        }
    }

    /// Enqueue one record.
    pub fn push(&self, record: EtaSnapshotRecord) {
        self.queue.offer(TelemetryEnvelope::new(
            self.host_id.clone(),
            TelemetryRecord::EtaSnapshot(record),
        ));
    }
}

/// The sink for a daemon whose running non-OTLP exporters' queues are
/// `native_queues`: `None` when there are none (the mirror of
/// [`super::queue_snapshot::sink_for_native_queues`]).
#[must_use]
pub fn sink_for_native_queues(
    native_queues: Vec<Arc<DurableQueue>>,
    host_id: &str,
) -> Option<EtaSnapshotSink> {
    if native_queues.is_empty() {
        return None;
    }
    Some(EtaSnapshotSink::new(Arc::new(FanoutQueue::new(native_queues)), host_id))
}

static GLOBAL_SINK: OnceLock<EtaSnapshotSink> = OnceLock::new();

/// Register the process-global sink. Called once from [`super::spawn_task`];
/// later calls are no-ops.
pub fn register_global_sink(sink: EtaSnapshotSink) {
    let _ = GLOBAL_SINK.set(sink);
}

/// The fingerprint of the last emitted estimate set.
static LAST_EMITTED: Mutex<Option<u64>> = Mutex::new(None);

/// The current estimate for each `(repo, issue, kind)` this host tracks:
/// the newest estimate of the kind's `current` heuristic, in
/// `(repo, issue, kind)` order.
///
/// `pending` is every estimate still awaiting an outcome, so it holds a
/// series per item — earlier refreshes, and one per shadow candidate. Pure.
#[must_use]
pub fn select_current(
    pending: &[EstimateSummary],
    current: &BTreeMap<Kind, String>,
) -> Vec<EstimateSummary> {
    let mut newest: BTreeMap<(String, u32, Kind), &EstimateSummary> = BTreeMap::new();
    for estimate in pending {
        if current.get(&estimate.kind) != Some(&estimate.heuristic) {
            continue;
        }
        let key = (estimate.repo.to_ascii_lowercase(), estimate.issue, estimate.kind);
        newest
            .entry(key)
            .and_modify(|held| {
                if estimate.as_of > held.as_of {
                    *held = estimate;
                }
            })
            .or_insert(estimate);
    }
    newest.into_values().cloned().collect()
}

/// A stable digest of the selected estimate set — what "changed since the
/// last emission" compares. Over each row's identity and its `estimate_id`,
/// which is derived from `(repo, issue, kind, heuristic, as_of)`: a new
/// estimate always changes it, and a pass that re-reads the same estimates
/// never does. Visibility is not part of it (a tag, not an estimate). Pure.
#[must_use]
pub fn fingerprint(selected: &[EstimateSummary]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for estimate in selected {
        estimate.repo.to_ascii_lowercase().hash(&mut hasher);
        estimate.issue.hash(&mut hasher);
        estimate.kind.hash(&mut hasher);
        estimate.estimate_id.hash(&mut hasher);
    }
    hasher.finish()
}

/// Whether an estimate set fingerprinted `now` still needs a snapshot, given
/// the last emitted one's fingerprint. A restart (`last` = `None`) always
/// does: the set was never emitted by *this* process, and the record it
/// produces describes the restored estimates as they are, not a stale copy.
#[must_use]
pub fn is_changed(now: u64, last: Option<u64>) -> bool {
    last != Some(now)
}

/// Build the record for `selected` (already in row order). `visibility` tags
/// each repo; a repo absent from it is [`RepoVisibility::Private`], the
/// safe default. Pure.
#[must_use]
pub fn build_record(
    selected: &[EstimateSummary],
    visibility: &HashMap<String, RepoVisibility>,
) -> EtaSnapshotRecord {
    let as_of = selected
        .iter()
        .map(|estimate| estimate.as_of)
        .max()
        .unwrap_or_else(chrono::Utc::now);
    let rows: Vec<EtaSnapshotRow> = selected
        .iter()
        .take(MAX_ROWS)
        .map(|estimate| EtaSnapshotRow {
            repo: estimate.repo.clone(),
            visibility: visibility
                .get(&estimate.repo)
                .copied()
                .unwrap_or(RepoVisibility::Private),
            issue: estimate.issue,
            pr: estimate.pr_number,
            kind: estimate.kind,
            p25: estimate.p25_sec,
            p50: estimate.p50_sec,
            p75: estimate.p75_sec,
            heuristic: estimate.heuristic.clone(),
            estimate_id: estimate.estimate_id.clone(),
            as_of: estimate.as_of,
            stage: estimate.stage,
            no_estimate_reason: estimate.no_estimate_reason,
        })
        .collect();
    EtaSnapshotRecord {
        as_of,
        rows_truncated: selected.len().saturating_sub(rows.len()),
        rows,
    }
}

/// Whether this pass has a snapshot to send, and what it is: the selected
/// rows and their fingerprint. `input` is [`super::eta::snapshot_input`]'s
/// answer and `last` the previously emitted fingerprint.
///
/// The whole decision, with no I/O, so every "no record at all" case is a
/// unit test rather than a wiring inspection. `None` when:
///
/// - `input` is `None` — ETA is disabled (`autonomous.eta.enabled = false`),
///   so there is no tracker and this kind is never emitted;
/// - nothing is currently estimated — an empty snapshot would claim this
///   host estimates nothing, which is a different fact;
/// - the set is byte-for-byte the one already emitted.
#[must_use]
pub fn decide(
    input: Option<(Vec<EstimateSummary>, BTreeMap<Kind, String>)>,
    last: Option<u64>,
) -> Option<(Vec<EstimateSummary>, u64)> {
    let (pending, current) = input?;
    let selected = select_current(&pending, &current);
    if selected.is_empty() {
        return None;
    }
    let digest = fingerprint(&selected);
    is_changed(digest, last).then_some((selected, digest))
}

/// Emit this host's current estimate set when a native sink is registered,
/// ETA is enabled, the tracker holds at least one current estimate, and that
/// set has changed since the previous snapshot.
pub(super) async fn record() {
    let Some(sink) = GLOBAL_SINK.get() else {
        return;
    };
    let last = *LAST_EMITTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some((selected, digest)) = decide(super::eta::snapshot_input(), last) else {
        return;
    };
    // Only after the decision: a pass with nothing new costs no visibility
    // lookups at all.
    let mut visibility: HashMap<String, RepoVisibility> = HashMap::new();
    for estimate in &selected {
        if !visibility.contains_key(&estimate.repo) {
            let tag = super::collector::resolve_visibility(&estimate.repo).await;
            visibility.insert(estimate.repo.clone(), tag);
        }
    }
    let record = build_record(&selected, &visibility);
    log::debug!(
        "eta.snapshot: {} row(s) ({} truncated), as_of={}",
        record.rows.len(),
        record.rows_truncated,
        record.as_of
    );
    sink.push(record);
    *LAST_EMITTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(digest);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "eta_snapshot_tests.rs"]
mod tests;
