//! `fleet.state` emission (Issue #10196, slice 2). This host's in-flight items
//! and open-PR census are sent to the **OTLP** exporters on the collector's
//! 5-minute snapshot pass. Record semantics are in
//! [`crate::telemetry::kinds::fleet_state`].
//!
//! The module reads state and does nothing else. It runs right after the ETA
//! pass (`super::eta::record`) from the same timer, and takes everything from
//! state that already exists:
//!
//! - **items, census, plan slots** come from the ETA tracker
//!   ([`super::eta::fleet_state_input`]). The tracker already follows every
//!   live item's stage and entry instant and stores the pass's review
//!   listings. No forge read is made here, and the estimator is not touched.
//! - **slot**: the sweep registries' overflow marks (#9244), a local
//!   in-memory read.
//! - **visibility**: the TTL-cached `derive_visibility`, read only for repos
//!   in a record that is actually sent.
//!
//! [`decide`] is pure. It sends a full anchor on the first pass of a process
//! and whenever the last anchor is [`ANCHOR_INTERVAL_SECS`] old or older. In
//! between it sends a delta only when rows, a census or the plan slots
//! changed. Two cases produce no record at all, and neither is an empty
//! record: ETA disabled (`autonomous.eta.enabled = false`, so there is no
//! tracker) and no OTLP exporter (no sink registered). With ETA enabled, an
//! anchor with zero rows is still sent, because it truthfully says "nothing in
//! flight here".

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, Duration, Utc};

use super::queue::{DurableQueue, FanoutQueue, QueueSink};
use crate::eta::tracker::live::LiveItem;
use crate::eta::tracker::RepoPrCensus;
use crate::eta::Stage;
use crate::telemetry::kinds::fleet_state::{
    FleetPrCensus, FleetSlot, FleetSlots, FleetStateRecord, FleetStateRepo, FleetStateRow,
    ANCHOR_INTERVAL_SECS, FLEET_STATE_SCHEMA, MAX_ROWS,
};
use crate::telemetry::{RepoVisibility, TelemetryEnvelope, TelemetryRecord};

/// What one pass reads from the ETA tracker.
#[derive(Debug, Clone, Default)]
pub struct FleetInput {
    /// This host's id.
    pub host_id: String,
    /// Live tracker items.
    pub items: Vec<LiveItem>,
    /// The last fleet view's census, with its observation instant.
    pub census: Option<(DateTime<Utc>, Vec<RepoPrCensus>)>,
    /// The last plan's `(max_concurrent, occupancy)`.
    pub slots: Option<(u32, Option<u32>)>,
}

/// One repo's full state at one pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoView {
    /// The census, when the repo was completely listed.
    pub census: Option<FleetPrCensus>,
    /// Rows by issue.
    pub rows: BTreeMap<u32, FleetStateRow>,
}

/// The full state at one pass, before visibility tagging.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FleetView {
    /// When the census listings were read.
    pub census_at: Option<DateTime<Utc>>,
    /// Plan slot use.
    pub slots: Option<FleetSlots>,
    /// Per-repo state, by lowercased slug.
    pub repos: BTreeMap<String, RepoView>,
    /// Rows dropped by the [`MAX_ROWS`] cap.
    pub rows_truncated: usize,
}

/// What the previous record left behind, which the next delta is relative
/// to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Emitted {
    /// The full state the last record brought a reader to.
    pub view: FleetView,
    /// That record's `as_of`.
    pub as_of: DateTime<Utc>,
    /// The `as_of` of the anchor its chain started from.
    pub anchor_as_of: DateTime<Utc>,
}

/// Cut rank under the [`MAX_ROWS`] cap; lower survives first. Rows this host
/// runs come first, since only this host can report them. `ready_wait` comes
/// last, because the work finder's own `queue.snapshot` also covers it.
fn cap_rank(row: &FleetStateRow) -> u8 {
    match (row.host.is_some(), row.stage) {
        (true, _) => 0,
        (false, Stage::ReadyWait) => 2,
        (false, _) => 1,
    }
}

/// The full state of `input`. `overflow` holds the ids of the sweeps that
/// hold this host's overflow slot. Pure.
#[must_use]
pub fn build_view(input: &FleetInput, overflow: &BTreeSet<String>) -> FleetView {
    let mut rows: Vec<(String, FleetStateRow)> = input
        .items
        .iter()
        .map(|item| {
            let slot = item.running_sweep_id.as_ref().map(|id| {
                if overflow.contains(id) {
                    FleetSlot::Overflow
                } else {
                    FleetSlot::Regular
                }
            });
            (
                item.repo.to_ascii_lowercase(),
                FleetStateRow {
                    issue: item.issue,
                    stage: item.stage,
                    entered_at: item.entered_at,
                    entered_at_lower_bound: item.entered_at_lower_bound,
                    pr: item.pr,
                    host: slot.map(|_| input.host_id.clone()),
                    slot,
                },
            )
        })
        .collect();
    // Stable: within a rank the tracker's (repo, issue) order holds.
    rows.sort_by_key(|(_, row)| cap_rank(row));
    let rows_truncated = rows.len().saturating_sub(MAX_ROWS);
    rows.truncate(MAX_ROWS);

    let mut repos: BTreeMap<String, RepoView> = BTreeMap::new();
    for (repo, row) in rows {
        repos.entry(repo).or_default().rows.insert(row.issue, row);
    }
    let census_at = input.census.as_ref().map(|(at, census)| {
        for repo in census {
            repos
                .entry(repo.repo.to_ascii_lowercase())
                .or_default()
                .census = Some(FleetPrCensus {
                open: repo.open,
                by_stage: repo.by_stage.clone(),
            });
        }
        *at
    });
    FleetView {
        census_at,
        slots: input.slots.map(|(max_concurrent, occupancy)| FleetSlots {
            max_concurrent,
            occupancy,
        }),
        repos,
        rows_truncated,
    }
}

/// Whether the pass at `now` must send a full anchor: on the first pass of
/// this process, or once the last anchor is [`ANCHOR_INTERVAL_SECS`] old.
#[must_use]
pub fn needs_anchor(last: Option<&Emitted>, now: DateTime<Utc>) -> bool {
    last.is_none_or(|last| now - last.anchor_as_of >= Duration::seconds(ANCHOR_INTERVAL_SECS))
}

fn repo_entry(repo: &str, census: Option<FleetPrCensus>) -> FleetStateRepo {
    FleetStateRepo {
        repo: repo.to_string(),
        // Tagged by the caller once the record is known to be sent; private
        // until then is the safe default.
        visibility: RepoVisibility::Private,
        census,
        rows: Vec::new(),
        removed: Vec::new(),
    }
}

/// The record to send for `view` at `now`, given what was last sent, or
/// `None` when nothing changed and no anchor is due. Every repo is tagged
/// [`RepoVisibility::Private`] until the caller resolves it. Pure.
///
/// The change test ignores `census_at`, which advances every pass; it is
/// still carried on every record that is sent. A change in `rows_truncated`
/// does count: it distinguishes complete from partial state on replay, so a
/// delta (possibly with an empty `repos`) is sent when it moves.
#[must_use]
pub fn decide(
    view: &FleetView,
    last: Option<&Emitted>,
    now: DateTime<Utc>,
) -> Option<FleetStateRecord> {
    let header = |anchor: bool, anchor_as_of, prev_as_of, repos| FleetStateRecord {
        schema: FLEET_STATE_SCHEMA.to_string(),
        as_of: now,
        anchor,
        anchor_as_of,
        prev_as_of,
        census_at: view.census_at,
        slots: view.slots,
        repos,
        rows_truncated: view.rows_truncated,
    };
    let last = match last {
        Some(last) if !needs_anchor(Some(last), now) => last,
        _ => {
            let repos = view
                .repos
                .iter()
                .map(|(repo, state)| FleetStateRepo {
                    rows: state.rows.values().cloned().collect(),
                    ..repo_entry(repo, state.census.clone())
                })
                .collect();
            return Some(header(true, now, None, repos));
        }
    };

    let empty = RepoView::default();
    let names: BTreeSet<&String> = view.repos.keys().chain(last.view.repos.keys()).collect();
    let mut repos = Vec::new();
    for repo in names {
        let now_state = view.repos.get(repo).unwrap_or(&empty);
        let was = last.view.repos.get(repo).unwrap_or(&empty);
        let rows: Vec<FleetStateRow> = now_state
            .rows
            .iter()
            .filter(|(issue, row)| was.rows.get(issue) != Some(row))
            .map(|(_, row)| row.clone())
            .collect();
        let removed: Vec<u32> = was
            .rows
            .keys()
            .filter(|issue| !now_state.rows.contains_key(issue))
            .copied()
            .collect();
        if rows.is_empty() && removed.is_empty() && now_state.census == was.census {
            continue;
        }
        repos.push(FleetStateRepo {
            rows,
            removed,
            ..repo_entry(repo, now_state.census.clone())
        });
    }
    if repos.is_empty()
        && view.slots == last.view.slots
        && view.rows_truncated == last.view.rows_truncated
    {
        return None;
    }
    Some(header(false, last.anchor_as_of, Some(last.as_of), repos))
}

/// Offers `fleet.state` envelopes to the OTLP exporters' queues.
#[derive(Clone)]
pub struct FleetStateSink {
    queue: Arc<dyn QueueSink>,
    host_id: String,
}

impl FleetStateSink {
    #[must_use]
    pub fn new(queue: Arc<dyn QueueSink>, host_id: impl Into<String>) -> Self {
        FleetStateSink {
            queue,
            host_id: host_id.into(),
        }
    }

    /// Enqueue one record.
    pub fn push(&self, record: FleetStateRecord) {
        self.queue.offer(TelemetryEnvelope::new(
            self.host_id.clone(),
            TelemetryRecord::FleetState(record),
        ));
    }
}

static SINK: OnceLock<FleetStateSink> = OnceLock::new();

/// Register the OTLP queues (called once from [`super::spawn_task`]). With no
/// OTLP exporter nothing is registered, and [`record`] returns before reading
/// anything.
pub fn register_sink(otlp_queues: Vec<Arc<DurableQueue>>, host_id: &str) {
    if otlp_queues.is_empty() {
        return;
    }
    let _ = SINK.set(FleetStateSink::new(Arc::new(FanoutQueue::new(otlp_queues)), host_id));
}

/// What the previous record left behind.
static LAST: Mutex<Option<Emitted>> = Mutex::new(None);

/// The ids of the sweeps holding this host's overflow slot, across every
/// provisioned registry. Each registry lock is released before the next.
fn overflow_ids(workspace_pool: &crate::workspace_pool::WorkspacePool) -> BTreeSet<String> {
    workspace_pool
        .provisioned_registries()
        .into_iter()
        .flat_map(|registry| {
            registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .overflow_sweep_ids()
        })
        .collect()
}

/// One `fleet.state` pass.
pub(super) async fn record(workspace_pool: &crate::workspace_pool::WorkspacePool) {
    let Some(sink) = SINK.get() else {
        return;
    };
    let Some(input) = super::eta::fleet_state_input() else {
        return;
    };
    let view = build_view(&input, &overflow_ids(workspace_pool));
    let now = Utc::now();
    let last = LAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let Some(mut record) = decide(&view, last.as_ref(), now) else {
        return;
    };
    for repo in &mut record.repos {
        repo.visibility = super::collector::resolve_visibility(&repo.repo).await;
    }
    log::debug!(
        "fleet.state: anchor={} repos={} rows={} removed={} truncated={}",
        record.anchor,
        record.repos.len(),
        record.row_count(),
        record.removed_count(),
        record.rows_truncated
    );
    let anchor_as_of = record.anchor_as_of;
    sink.push(record);
    *LAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Emitted {
        view,
        as_of: now,
        anchor_as_of,
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "fleet_state_tests.rs"]
mod tests;
