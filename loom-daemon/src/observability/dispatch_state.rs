//! `eta.dispatch_state` emission (Issue #12918): each classified open item's
//! dispatch gate, with the release time where one exists, over OTLP.
//!
//! # Where this runs
//!
//! Called by the step driver once per tick, after every workspace is weighed
//! and shaped (`step_dispatch::driver`), with rows built by
//! `step_dispatch::state_rows`. Everything the rows need is already in the
//! tick's memory: the [`Weighing`](crate::step_dispatch::item_draw::Weighing),
//! the merged cooldowns, and the ladder markers parsed from the bodies the
//! snapshot already carries.
//!
//! # No new forge calls (hard rule)
//!
//! Repo identity resolves through the **synchronous** [`cached_repo_ref`],
//! which never fetches; a workspace not yet cached is skipped this tick (and
//! so is not tracked, so it is emitted in full once it resolves). So
//! `loom.forge.calls` stays flat.
//!
//! # What is emitted
//!
//! One `eta.dispatch_state` log per classified item (pooled and weight-0, no
//! row cap) when:
//! - it is first seen, or any of (`in_pool`, `exclusion`, `gate_until`,
//!   re-evaluation rung) changed since its last emission (`changed`), or
//! - the reconciliation window elapsed (`refresh`), so every item is
//!   re-stated periodically and a reader never depends on a single log.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, Duration, Utc};

use super::queue::{DurableQueue, FanoutQueue, QueueSink};
use super::repo_ref::cached_repo_ref;
use crate::telemetry::kinds::dispatch_state::{
    Artifact, DispatchStateRecord, StateTransition, DISPATCH_STATE_SCHEMA_VERSION,
};
use crate::telemetry::{RepoVisibility, TelemetryEnvelope, TelemetryRecord};

/// Env override for the reconciliation cadence, in seconds.
pub const REFRESH_SECS_ENV: &str = "LOOM_DISPATCH_STATE_REFRESH_SECS";
/// Default reconciliation cadence: ten minutes (matching
/// `dispatch_eligibility`).
pub const DEFAULT_REFRESH_SECS: i64 = 600;

/// Offers `eta.dispatch_state` envelopes, stamped with this daemon's host id,
/// to the OTLP exporters' queues.
#[derive(Clone)]
pub struct DispatchStateSink {
    queue: Arc<dyn QueueSink>,
    host_id: String,
}

impl DispatchStateSink {
    #[must_use]
    pub fn new(queue: Arc<dyn QueueSink>, host_id: impl Into<String>) -> Self {
        DispatchStateSink {
            queue,
            host_id: host_id.into(),
        }
    }

    /// Enqueue one record.
    pub fn push(&self, record: DispatchStateRecord) {
        self.queue.offer(TelemetryEnvelope::new(
            self.host_id.clone(),
            TelemetryRecord::DispatchState(record),
        ));
    }
}

static GLOBAL_SINK: OnceLock<DispatchStateSink> = OnceLock::new();

/// Register the OTLP queues (called once from [`super::spawn_task`]). With no
/// OTLP exporter nothing is registered, and [`observe`] returns before doing
/// any work.
pub fn register_sink(otlp_queues: Vec<Arc<DurableQueue>>, host_id: &str) {
    if otlp_queues.is_empty() {
        return;
    }
    let _ =
        GLOBAL_SINK.set(DispatchStateSink::new(Arc::new(FanoutQueue::new(otlp_queues)), host_id));
}

/// Whether an OTLP sink is registered: the step driver builds no rows without
/// one.
#[must_use]
pub fn enabled() -> bool {
    GLOBAL_SINK.get().is_some()
}

/// One classified item as the step driver reduced it, before the repo is
/// resolved. Built by `step_dispatch::state_rows`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateRow {
    /// The workspace's index in the tick.
    pub ws_idx: usize,
    pub number: u32,
    pub artifact: Artifact,
    /// `Step::as_str`; `None` for a pass.
    pub step: Option<String>,
    pub in_pool: bool,
    /// `Exclusion::as_str`.
    pub exclusion: Option<String>,
    pub gate_until: Option<DateTime<Utc>>,
    pub reeval_rung: Option<u32>,
    pub reeval_next: Option<DateTime<Utc>>,
    pub iw: Option<u64>,
    pub weight: Option<u64>,
}

/// A [`StateRow`] with its repo resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRow {
    pub slug: String,
    pub visibility: RepoVisibility,
    pub row: StateRow,
}

/// What a change is detected on: (`in_pool`, `exclusion`, `gate_until`,
/// re-evaluation rung).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Signature {
    in_pool: bool,
    exclusion: Option<String>,
    gate_until: Option<DateTime<Utc>>,
    rung: Option<u32>,
}

impl Signature {
    fn of(row: &StateRow) -> Self {
        Signature {
            in_pool: row.in_pool,
            exclusion: row.exclusion.clone(),
            gate_until: row.gate_until,
            rung: row.reeval_rung,
        }
    }
}

#[derive(Debug, Clone)]
struct Tracked {
    signature: Signature,
    emitted_at: DateTime<Utc>,
}

/// Per-`(slug, number)` last-emitted signature and emission time. A pure
/// [`Self::diff`] driven by a process-global instance only at the
/// [`observe`] seam.
#[derive(Debug, Default)]
pub struct StateTracker {
    last: HashMap<(String, u32), Tracked>,
}

impl StateTracker {
    /// Fold one tick's rows into the tracker, returning the index into `rows`
    /// of every row to emit and why: a new or changed signature as `changed`,
    /// an unchanged one past `refresh` as `refresh`.
    ///
    /// `observed` are the slugs this tick classified whole: a tracked key in
    /// one that has no row now is forgotten (the item left the open set); a
    /// key in any other repo is kept untouched, so a held or unlisted
    /// workspace never re-announces its items when it returns.
    pub fn diff(
        &mut self,
        rows: &[ResolvedRow],
        observed: &HashSet<String>,
        now: DateTime<Utc>,
        refresh: Duration,
    ) -> Vec<(usize, StateTransition)> {
        let mut next: HashMap<(String, u32), Tracked> = HashMap::new();
        let mut out = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            let key = (r.slug.clone(), r.row.number);
            let signature = Signature::of(&r.row);
            let (transition, emitted_at) = match self.last.get(&key) {
                Some(prev) if prev.signature == signature => {
                    if now.signed_duration_since(prev.emitted_at) >= refresh {
                        (Some(StateTransition::Refresh), now)
                    } else {
                        (None, prev.emitted_at)
                    }
                }
                _ => (Some(StateTransition::Changed), now),
            };
            if let Some(t) = transition {
                out.push((i, t));
            }
            next.insert(
                key,
                Tracked {
                    signature,
                    emitted_at,
                },
            );
        }
        for (key, tracked) in &self.last {
            if !observed.contains(&key.0) {
                next.entry(key.clone()).or_insert_with(|| tracked.clone());
            }
        }
        self.last = next;
        out
    }
}

/// The reconciliation threshold: `raw` seconds when it is a positive integer,
/// otherwise [`DEFAULT_REFRESH_SECS`].
#[must_use]
pub fn refresh_secs(raw: Option<&str>) -> i64 {
    raw.and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_REFRESH_SECS)
}

/// Turn one row into its wire record.
#[must_use]
pub fn build_record(
    r: &ResolvedRow,
    transition: StateTransition,
    host: &str,
    sampled_at: DateTime<Utc>,
) -> DispatchStateRecord {
    let row = &r.row;
    DispatchStateRecord {
        schema_version: DISPATCH_STATE_SCHEMA_VERSION,
        repo: r.slug.clone(),
        host: host.to_string(),
        number: row.number,
        artifact: row.artifact,
        step: row.step.clone(),
        in_pool: row.in_pool,
        exclusion: row.exclusion.clone(),
        gate_until: row.gate_until,
        reeval_rung: row.reeval_rung,
        reeval_next: row.reeval_next,
        iw: row.iw,
        weight: row.weight,
        transition,
        sampled_at,
        visibility: r.visibility,
    }
}

/// Resolve each row's workspace through `resolve` (a row whose workspace does
/// not resolve is dropped), and the slugs of the `observed` workspaces that
/// did. Pure.
#[must_use]
pub fn resolve_rows(
    rows: Vec<StateRow>,
    observed: &BTreeSet<usize>,
    resolve: impl Fn(usize) -> Option<crate::telemetry::queue_snapshot::QueueRepoRef>,
) -> (Vec<ResolvedRow>, HashSet<String>) {
    let mut by_idx: HashMap<usize, Option<crate::telemetry::queue_snapshot::QueueRepoRef>> =
        HashMap::new();
    let mut resolved = Vec::with_capacity(rows.len());
    for row in rows {
        let repo = by_idx
            .entry(row.ws_idx)
            .or_insert_with(|| resolve(row.ws_idx))
            .clone();
        if let Some(repo) = repo {
            resolved.push(ResolvedRow {
                slug: repo.repo,
                visibility: repo.visibility,
                row,
            });
        }
    }
    let slugs = observed
        .iter()
        .filter_map(|idx| resolve(*idx).map(|r| r.repo))
        .collect();
    (resolved, slugs)
}

static TRACKER: OnceLock<Mutex<StateTracker>> = OnceLock::new();

/// Journal this tick's classified items: emit an `eta.dispatch_state` record
/// for every row [`StateTracker::diff`] says needs one. `observed` are the
/// workspaces classified this tick (a held one is not), `roots` the tick's
/// workspace roots by index. A no-op without the OTLP sink. Makes no forge
/// call.
pub fn observe(rows: Vec<StateRow>, observed: &BTreeSet<usize>, roots: &[PathBuf]) {
    let Some(sink) = GLOBAL_SINK.get() else {
        return;
    };
    let (resolved, slugs) = resolve_rows(rows, observed, |idx| {
        roots
            .get(idx)
            .and_then(|root| cached_repo_ref(&root.display().to_string()))
    });
    let now = Utc::now();
    let refresh = Duration::seconds(refresh_secs(std::env::var(REFRESH_SECS_ENV).ok().as_deref()));
    let emissions = TRACKER
        .get_or_init(|| Mutex::new(StateTracker::default()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .diff(&resolved, &slugs, now, refresh);
    for (i, transition) in emissions {
        sink.push(build_record(&resolved[i], transition, &sink.host_id, now));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "dispatch_state_tests.rs"]
mod tests;
