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
use crate::telemetry::kinds::eta_snapshot::{
    EtaSnapshotAlternate, EtaSnapshotRecord, EtaSnapshotRow, EtaSnapshotStage, MAX_ALTERNATES,
    MAX_RECORD_BYTES, MAX_ROWS,
};
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

/// A row's identity: `(lower-cased repo, issue, kind)`.
pub type RowKey = (String, u32, Kind);

/// The registered heuristic ids of each kind (`Registry::for_kind`).
pub type RegisteredIds = BTreeMap<Kind, Vec<String>>;

/// Shadow estimates per row, newest per heuristic, sorted by heuristic id.
pub type Alternates = BTreeMap<RowKey, Vec<EstimateSummary>>;

/// The shadow candidates' estimates for each `(repo, issue, kind)` (#10390):
/// from `pending`, every estimate whose heuristic is registered for its kind
/// and is not the kind's `current`, the newest `as_of` per heuristic, sorted
/// by heuristic id and cut at [`MAX_ALTERNATES`]. Matched by item, never by
/// equal `as_of` (each series emits on its own schedule). Only attached to a
/// row by [`build_record_with`]; this never creates one. Pure.
#[must_use]
pub fn select_alternates(
    pending: &[EstimateSummary],
    current: &BTreeMap<Kind, String>,
    registered: &RegisteredIds,
) -> Alternates {
    let mut newest: BTreeMap<(RowKey, String), &EstimateSummary> = BTreeMap::new();
    for estimate in pending {
        if current.get(&estimate.kind) == Some(&estimate.heuristic) {
            continue;
        }
        let is_registered = registered
            .get(&estimate.kind)
            .is_some_and(|ids| ids.contains(&estimate.heuristic));
        if !is_registered {
            continue;
        }
        let key = (estimate.repo.to_ascii_lowercase(), estimate.issue, estimate.kind);
        newest
            .entry((key, estimate.heuristic.clone()))
            .and_modify(|held| {
                if estimate.as_of > held.as_of {
                    *held = estimate;
                }
            })
            .or_insert(estimate);
    }
    let mut out: Alternates = BTreeMap::new();
    // BTreeMap order is (row, heuristic id): already sorted by id per row.
    for ((key, _), estimate) in newest {
        let list = out.entry(key).or_default();
        if list.len() < MAX_ALTERNATES {
            list.push(estimate.clone());
        }
    }
    out
}

/// A stable digest of the selected estimate set — what "changed since the
/// last emission" compares. Over each row's identity and its `estimate_id`,
/// which is derived from `(repo, issue, kind, heuristic, as_of)`: a new
/// estimate always changes it, and a pass that re-reads the same estimates
/// never does. Visibility is not part of it (a tag, not an estimate). Pure.
#[must_use]
pub fn fingerprint(selected: &[EstimateSummary]) -> u64 {
    fingerprint_with(selected, &Alternates::new())
}

/// [`fingerprint`] that also covers every row's alternates' `estimate_id`s
/// (#10390), so a shadow-only refresh yields a new snapshot. With no
/// alternates it equals [`fingerprint`]. Pure.
#[must_use]
pub fn fingerprint_with(selected: &[EstimateSummary], alternates: &Alternates) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for estimate in selected {
        estimate.repo.to_ascii_lowercase().hash(&mut hasher);
        estimate.issue.hash(&mut hasher);
        estimate.kind.hash(&mut hasher);
        estimate.estimate_id.hash(&mut hasher);
        let key = (estimate.repo.to_ascii_lowercase(), estimate.issue, estimate.kind);
        for alt in alternates.get(&key).into_iter().flatten() {
            alt.estimate_id.hash(&mut hasher);
        }
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

/// Cut rank under the [`MAX_ROWS`] cap; lower survives first (#10052).
/// `land` estimates (with a `p50`) answer the dashboard's ETA column, so they
/// outrank `land` refusals, which outrank every `start`/`finish` row.
fn cap_rank(estimate: &EstimateSummary) -> u8 {
    match (estimate.kind, estimate.p50_sec.is_some()) {
        (Kind::Land, true) => 0,
        (Kind::Land, false) => 1,
        _ => 2,
    }
}

/// Bytes [`MAX_RECORD_BYTES`] keeps back for the record's own fields
/// (`as_of`, the counters, `rows_truncated_by_kind`, the keys and brackets),
/// so the rows and alternates can be budgeted on their own sizes. Those
/// fields serialize to under 300 B.
const RECORD_OVERHEAD_BYTES: usize = 1024;

/// Build the record for `selected` (already in row order). `visibility` tags
/// each repo; a repo absent from it is [`RepoVisibility::Private`], the
/// safe default.
///
/// The [`MAX_ROWS`] cap is applied by priority ([`cap_rank`]), not by sort
/// order, so no repo loses its `land` estimates for sorting late (#10052);
/// the rows stay in that priority order on the wire (#10928). Pure.
#[must_use]
pub fn build_record(
    selected: &[EstimateSummary],
    visibility: &HashMap<String, RepoVisibility>,
) -> EtaSnapshotRecord {
    build_record_with(selected, &Alternates::new(), visibility)
}

/// [`build_record`] with each row's shadow `alternates` attached (#10390).
/// Alternates never count as rows: a row cut by the cap takes them with it.
///
/// Bounded by [`MAX_RECORD_BYTES`] (#10928) in two passes over the rows in
/// priority order: rows without their alternates are admitted until
/// [`MAX_ROWS`] or the budget, and then each admitted row keeps its
/// alternates while they still fit. The first row whose alternates do not
/// fit, and every row after it, is sent without them and counted in
/// `alternates_truncated`: a row's ETA outranks every row's shadow estimates.
///
/// A row's own stage forecast (`stages`, #10929) is an extra too: it never
/// costs a row, and it rides, in priority order, while it fits, before any
/// alternate.
#[must_use]
pub fn build_record_with(
    selected: &[EstimateSummary],
    alternates: &Alternates,
    visibility: &HashMap<String, RepoVisibility>,
) -> EtaSnapshotRecord {
    let as_of = selected
        .iter()
        .map(|estimate| estimate.as_of)
        .max()
        .unwrap_or_else(chrono::Utc::now);
    let mut ranked: Vec<&EstimateSummary> = selected.iter().collect();
    ranked.sort_by_cached_key(|estimate| {
        let repo = estimate.repo.to_ascii_lowercase();
        (cap_rank(estimate), repo, estimate.issue, estimate.kind)
    });
    let mut used = RECORD_OVERHEAD_BYTES;
    // (row, its size without alternates, its size with them, its stages)
    let mut admitted: Vec<(EtaSnapshotRow, usize, usize, Stages)> = Vec::new();
    for estimate in ranked.iter().take(MAX_ROWS) {
        let mut row = to_row(estimate, alternates, visibility);
        let alts = std::mem::take(&mut row.alternates);
        let stages = std::mem::take(&mut row.stages);
        // `+ 1`: the comma between rows.
        let bare = super::sender::json_len(&row) + 1;
        if used + bare > MAX_RECORD_BYTES {
            break;
        }
        used += bare;
        row.alternates = alts;
        let full = super::sender::json_len(&row) + 1;
        admitted.push((row, bare, full, stages));
    }
    for (row, _, _, stages) in &mut admitted {
        // `+ 10`: the `"stages":` key and its comma.
        let cost = super::sender::json_len(&*stages) + 10;
        if !stages.is_empty() && used + cost <= MAX_RECORD_BYTES {
            used += cost;
            row.stages = std::mem::take(stages);
        }
    }
    let mut rows_truncated_by_kind: BTreeMap<Kind, usize> = BTreeMap::new();
    for dropped in ranked.iter().skip(admitted.len()) {
        *rows_truncated_by_kind.entry(dropped.kind).or_insert(0) += 1;
    }
    let mut alternates_truncated = 0;
    let mut fits = true;
    let rows: Vec<EtaSnapshotRow> = admitted
        .into_iter()
        .map(|(mut row, bare, full, _)| {
            if row.alternates.is_empty() {
                return row;
            }
            fits = fits && used + (full - bare) <= MAX_RECORD_BYTES;
            if fits {
                used += full - bare;
            } else {
                row.alternates.clear();
                alternates_truncated += 1;
            }
            row
        })
        .collect();
    EtaSnapshotRecord {
        as_of,
        rows_truncated: selected.len().saturating_sub(rows.len()),
        alternates_truncated,
        rows_truncated_by_kind,
        rows,
    }
}

/// A row's per-stage forecast (#10929).
type Stages = BTreeMap<crate::eta::Stage, EtaSnapshotStage>;

/// One row for `estimate`, with every alternate it has.
fn to_row(
    estimate: &EstimateSummary,
    alternates: &Alternates,
    visibility: &HashMap<String, RepoVisibility>,
) -> EtaSnapshotRow {
    EtaSnapshotRow {
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
        stages: EtaSnapshotStage::all(&estimate.stage_predictions),
        alternates: alternates
            .get(&(estimate.repo.to_ascii_lowercase(), estimate.issue, estimate.kind))
            .into_iter()
            .flatten()
            .map(|alt| EtaSnapshotAlternate {
                heuristic: alt.heuristic.clone(),
                tier: crate::eta::shadow_fleet::builtin_tier(&alt.heuristic),
                estimate_id: alt.estimate_id.clone(),
                as_of: alt.as_of,
                p25: alt.p25_sec,
                p50: alt.p50_sec,
                p75: alt.p75_sec,
                p90: alt.p90_sec,
                no_estimate_reason: alt.no_estimate_reason,
            })
            .collect(),
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
    input: Option<SnapshotInput>,
    last: Option<u64>,
) -> Option<(Vec<EstimateSummary>, Alternates, u64)> {
    let (pending, current, registered) = input?;
    let selected = select_current(&pending, &current);
    if selected.is_empty() {
        return None;
    }
    let alternates = select_alternates(&pending, &current, &registered);
    let digest = fingerprint_with(&selected, &alternates);
    is_changed(digest, last).then_some((selected, alternates, digest))
}

/// [`super::eta::snapshot_input`]'s answer: pending estimates, each kind's
/// `current` heuristic id, and each kind's registered heuristic ids.
pub type SnapshotInput = (Vec<EstimateSummary>, BTreeMap<Kind, String>, RegisteredIds);

/// The `loom.eta.health.snapshot_*` gauges' view of a built record. Pure.
#[must_use]
pub fn stats(record: &EtaSnapshotRecord) -> super::ops::eta_health::SnapshotStats {
    let with_alternates = record
        .rows
        .iter()
        .filter(|r| !r.alternates.is_empty())
        .count();
    super::ops::eta_health::SnapshotStats {
        rows: record.rows.len() as u64,
        alternates_rows: with_alternates as u64,
        rows_truncated: record.rows_truncated as u64,
        alternates_truncated: record.alternates_truncated as u64,
        bytes: super::sender::json_len(record) as u64,
    }
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
    let Some((selected, alternates, digest)) = decide(super::eta::snapshot_input(), last) else {
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
    let record = build_record_with(&selected, &alternates, &visibility);
    let stats = stats(&record);
    log::debug!(
        "eta.snapshot: {} row(s) ({} truncated: {:?}; {} without their alternates), {} B, as_of={}",
        record.rows.len(),
        record.rows_truncated,
        record.rows_truncated_by_kind,
        record.alternates_truncated,
        stats.bytes,
        record.as_of
    );
    if record.rows_truncated > 0 {
        log::warn!(
            "eta.snapshot: dropped {} row(s) {:?} at the {MAX_ROWS}-row / {MAX_RECORD_BYTES}-byte cap; \
             the dashboard has no fresh ETA for them (#10928)",
            record.rows_truncated,
            record.rows_truncated_by_kind,
        );
    }
    super::ops::eta_health::note_snapshot(stats);
    sink.push(record);
    *LAST_EMITTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(digest);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "eta_snapshot_tests.rs"]
mod tests;
