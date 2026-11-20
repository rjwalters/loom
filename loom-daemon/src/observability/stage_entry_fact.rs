//! `eta.stage_entry` emission (Issue #12869): an item's arrival in a pipeline
//! stage, observed by diffing the step driver's per-tick discovery snapshot.
//!
//! Driven from the step driver right after it publishes the tick's snapshot,
//! so it makes no forge call and needs no label-write chokepoint. Arrival is
//! tick-granular. A repo's first read after a daemon start (or after the
//! mirror could not vouch for it) only seeds the baseline: an item already in
//! a stage has an unknown arrival and emits nothing.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, Utc};

use super::queue::{DurableQueue, FanoutQueue, QueueSink};
use crate::discovery::{RepoSnapshot, RepoStatus, TickSnapshot};
use crate::telemetry::kinds::stage_entry::{
    EnteredAtSource, StageEntryRecord, STAGE_ENTRY_SCHEMA_VERSION, STAGE_LABELS,
};
use crate::telemetry::{RepoVisibility, TelemetryEnvelope, TelemetryRecord};

/// Offers `eta.stage_entry` envelopes, stamped with this daemon's host id, to
/// the OTLP exporters' queues.
#[derive(Clone)]
pub struct StageEntrySink {
    queue: Arc<dyn QueueSink>,
    host_id: String,
}

impl StageEntrySink {
    #[must_use]
    pub fn new(queue: Arc<dyn QueueSink>, host_id: impl Into<String>) -> Self {
        StageEntrySink {
            queue,
            host_id: host_id.into(),
        }
    }

    pub fn push(&self, record: StageEntryRecord) {
        self.queue.offer(TelemetryEnvelope::new(
            self.host_id.clone(),
            TelemetryRecord::StageEntry(record),
        ));
    }
}

static GLOBAL_SINK: OnceLock<StageEntrySink> = OnceLock::new();
static TRACKER: Mutex<Option<ArrivalTracker>> = Mutex::new(None);

/// Register the OTLP queues (called once from [`super::spawn_task`]). With no
/// OTLP exporter nothing is registered and [`observe`] does no work.
pub fn register_sink(otlp_queues: Vec<Arc<DurableQueue>>, host_id: &str) {
    if otlp_queues.is_empty() {
        return;
    }
    let _ = GLOBAL_SINK.set(StageEntrySink::new(Arc::new(FanoutQueue::new(otlp_queues)), host_id));
}

/// One arrival the diff found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arrival {
    pub issue: u32,
    pub stage: &'static str,
    pub entered_at: DateTime<Utc>,
    pub source: EnteredAtSource,
}

/// Which stage labels each issue carried on the previous read of a repo.
#[derive(Debug, Default)]
pub struct ArrivalTracker {
    /// Lowercased repo slug → issue → stage labels it carried.
    seen: HashMap<String, HashMap<u32, HashSet<&'static str>>>,
}

impl ArrivalTracker {
    /// Diff one repo's complete read against the previous one. The first read
    /// of a repo seeds the baseline and returns nothing. Pure.
    pub fn diff(&mut self, repo: &RepoSnapshot, now: DateTime<Utc>) -> Vec<Arrival> {
        let mut current: HashMap<u32, HashSet<&'static str>> = HashMap::new();
        for row in repo.rows.iter().filter(|r| !r.is_pull_request) {
            let stages: HashSet<&'static str> = STAGE_LABELS
                .iter()
                .copied()
                .filter(|s| row.labels.iter().any(|l| l == s))
                .collect();
            if !stages.is_empty() {
                current.insert(row.number, stages);
            }
        }
        let previous = self
            .seen
            .insert(repo.slug.to_ascii_lowercase(), current.clone());
        let Some(previous) = previous else {
            return Vec::new();
        };
        let mut arrivals = Vec::new();
        for (&issue, stages) in &current {
            for &stage in stages {
                if previous.get(&issue).is_some_and(|p| p.contains(stage)) {
                    continue;
                }
                let (entered_at, source) = repo
                    .rows
                    .iter()
                    .find(|r| r.number == issue)
                    .and_then(|r| r.labels_at.as_ref())
                    .and_then(|m| m.get(stage))
                    .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                    .map_or((now, EnteredAtSource::Observed), |t| {
                        (t.with_timezone(&Utc), EnteredAtSource::Mirror)
                    });
                arrivals.push(Arrival {
                    issue,
                    stage,
                    entered_at,
                    source,
                });
            }
        }
        arrivals.sort_by(|a, b| (a.issue, a.stage).cmp(&(b.issue, b.stage)));
        arrivals
    }

    /// Forget a repo's baseline (its read could not be vouched for), so the
    /// next complete read re-seeds instead of reporting the gap as arrivals.
    pub fn forget(&mut self, slug: &str) {
        self.seen.remove(&slug.to_ascii_lowercase());
    }
}

/// Emit this tick's arrivals. A no-op without the OTLP sink.
pub fn observe(snapshot: &TickSnapshot) {
    let Some(sink) = GLOBAL_SINK.get() else {
        return;
    };
    let now = Utc::now();
    let mut guard = TRACKER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tracker = guard.get_or_insert_with(ArrivalTracker::default);
    for repo in snapshot.repos() {
        if repo.status != RepoStatus::Ok {
            tracker.forget(&repo.slug);
            continue;
        }
        let arrivals = tracker.diff(&repo, now);
        if arrivals.is_empty() {
            continue;
        }
        // A pure cache read; private unless positively known public.
        let visibility = crate::telemetry::visibility::cached_visibility(&repo.slug)
            .unwrap_or(RepoVisibility::Private);
        for a in arrivals {
            sink.push(StageEntryRecord {
                schema_version: STAGE_ENTRY_SCHEMA_VERSION,
                repo: repo.slug.clone(),
                issue: a.issue,
                stage: a.stage.to_string(),
                entered_at: a.entered_at,
                entered_at_source: a.source,
                observed_at: now,
                visibility,
            });
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "stage_entry_fact_tests.rs"]
mod tests;
