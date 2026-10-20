//! `eta.dispatch_eligibility` emission (Issue #12790): the dispatcher's
//! schedulability decision for each waiting ready item, over OTLP, so ETA can
//! predict `ready_wait` — its worst-fit phase.
//!
//! # Where this runs, and why not the tick loop
//!
//! Driven from the collector's periodic sample
//! (`observability::collector::sample_snapshots`, the same cadence as
//! [`super::queue_snapshot::record`] and [`super::ops::disposition::record`]),
//! reading [`crate::work_finder::last_tick_summary`] — never from the
//! work-finder tick itself, which must never block on a forge probe. The
//! dispatcher already computed each row's [`QueueDisposition`]; this fact only
//! journals it.
//!
//! # No new forge calls (hard rule)
//!
//! Repo roots are resolved through the **synchronous** [`cached_repo_ref`],
//! which never triggers a fetch — the collector's `queue_snapshot` /
//! `ops::disposition` passes already warmed that cache this interval. A row
//! whose root is not yet cached is simply dropped (the next pass will have it).
//! So `loom.forge.calls` stays flat.
//!
//! # What is emitted
//!
//! One `eta.dispatch_eligibility` log per **waiting** ready item (coarse state
//! `ready` or `blocked`; a `running`/dispatched row never gets its own fact,
//! though it still counts toward others' work-ahead because it occupies the
//! pool), when:
//! - its **eligibility class** flips (`changed`, including first sight), or
//! - the refresh window elapsed (`refresh`, re-stating the unchanged class).
//!
//! Each record carries when the current class began ([`block_start`]), the
//! effective eligible work-ahead (host pool and repo pool), and the resource
//! pool, so a reader can split `ready_wait` dwell at the eligibility boundary
//! and reconstruct, as of any instant, how many eligible competitors were
//! ahead.
//!
//! [`block_start`]: crate::telemetry::kinds::dispatch_eligibility::DispatchEligibilityRecord::block_start

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, Duration, Utc};

use super::queue::{DurableQueue, FanoutQueue, QueueSink};
use super::repo_ref::cached_repo_ref;
use crate::telemetry::kinds::dispatch_eligibility::{
    DispatchEligibilityRecord, Eligibility, EligibilityTransition,
    DISPATCH_ELIGIBILITY_SCHEMA_VERSION,
};
use crate::telemetry::{RepoVisibility, TelemetryEnvelope, TelemetryRecord};
use crate::types::WorkFinderTickSummary;

/// Env override for the refresh cadence, in seconds.
pub const REFRESH_SECS_ENV: &str = "LOOM_DISPATCH_ELIGIBILITY_REFRESH_SECS";
/// Default refresh cadence: ten minutes (matching `ops::disposition`).
pub const DEFAULT_REFRESH_SECS: i64 = 600;

/// Offers `eta.dispatch_eligibility` envelopes, stamped with this daemon's host
/// id, to the OTLP exporters' queues.
#[derive(Clone)]
pub struct DispatchEligibilitySink {
    queue: Arc<dyn QueueSink>,
    host_id: String,
}

impl DispatchEligibilitySink {
    #[must_use]
    pub fn new(queue: Arc<dyn QueueSink>, host_id: impl Into<String>) -> Self {
        DispatchEligibilitySink {
            queue,
            host_id: host_id.into(),
        }
    }

    /// Enqueue one record.
    pub fn push(&self, record: DispatchEligibilityRecord) {
        self.queue.offer(TelemetryEnvelope::new(
            self.host_id.clone(),
            TelemetryRecord::DispatchEligibility(record),
        ));
    }
}

static GLOBAL_SINK: OnceLock<DispatchEligibilitySink> = OnceLock::new();

/// Register the OTLP queues (called once from [`super::spawn_task`]). With no
/// OTLP exporter nothing is registered, and [`record`] returns before doing any
/// work.
pub fn register_sink(otlp_queues: Vec<Arc<DurableQueue>>, host_id: &str) {
    if otlp_queues.is_empty() {
        return;
    }
    let _ = GLOBAL_SINK
        .set(DispatchEligibilitySink::new(Arc::new(FanoutQueue::new(otlp_queues)), host_id));
}

/// One waiting ready item reduced to what the record and the tracker need.
/// Built by [`build_rows`] from a [`WorkFinderTickSummary`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EligRow {
    /// Forge `owner/repo`. Never a local path.
    pub slug: String,
    pub visibility: RepoVisibility,
    pub issue: u32,
    /// 1-based bare comparator rank on the host this tick. Comparator order
    /// only: the step-mode weighted draw (`item_draw`) does not use it, so it
    /// is not queue position.
    pub rank: u32,
    pub eligibility: Eligibility,
    /// [`QueueDisposition::as_str`].
    pub reason: String,
    /// `ready` or `blocked`.
    pub state: String,
    /// Eligible items with a smaller comparator rank across the host
    /// concurrency pool. Comparator order, not used by the step-mode weighted
    /// draw, not queue position (see `pick.decision.item_draw` for the draw
    /// weight share).
    pub eligible_work_ahead: u32,
    /// Eligible items with a smaller comparator rank within this item's own
    /// repo pool. Comparator order, not queue position (see
    /// [`Self::eligible_work_ahead`]).
    pub eligible_work_ahead_repo: u32,
    /// The issue's priority level (`Level::as_str`).
    pub level: String,
    /// The host's effective concurrency cap this tick.
    pub max_concurrent: u32,
}

fn u32_from(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Build this tick's eligibility rows from `summary`'s ranked queue, resolving
/// each row's repo through `resolve` (the synchronous cache; a row whose root
/// does not resolve is dropped). Only **waiting** items (coarse state `ready`
/// or `blocked`) get a row; a `running` row never does, but every non-`blocked`
/// row — running included — counts toward the eligible work-ahead of the rows
/// ahead of which it sits, because it occupies the pool. Pure.
#[must_use]
pub fn build_rows(
    summary: &WorkFinderTickSummary,
    resolve: impl Fn(&str) -> Option<crate::telemetry::queue_snapshot::QueueRepoRef>,
) -> Vec<EligRow> {
    // The denominator: (rank, root, is-eligible-for-counting) for every queue
    // row, resolved or not. A non-`blocked` row (running or ready) competes for
    // and occupies the pool, so it counts; a `blocked` row never does.
    let competitors: Vec<(u32, &str, bool)> = summary
        .queue
        .iter()
        .map(|row| (u32_from(row.rank), row.repo.as_str(), row.disposition.state() != "blocked"))
        .collect();

    let mut rows = Vec::new();
    for row in &summary.queue {
        let state = row.disposition.state();
        // A running (dispatched / in-flight) row has left `ready_wait`; it is
        // not the subject of a fact, only part of the denominator above.
        if state == "running" {
            continue;
        }
        let Some(repo) = resolve(&row.repo) else {
            continue;
        };
        let rank = u32_from(row.rank);
        let eligible_work_ahead = u32_from(
            competitors
                .iter()
                .filter(|(r, _, eligible)| *eligible && *r < rank)
                .count(),
        );
        let eligible_work_ahead_repo = u32_from(
            competitors
                .iter()
                .filter(|(r, root, eligible)| *eligible && *r < rank && *root == row.repo.as_str())
                .count(),
        );
        rows.push(EligRow {
            slug: repo.repo,
            visibility: repo.visibility,
            issue: row.issue,
            rank,
            eligibility: Eligibility::from_state(state),
            reason: row.disposition.as_str().to_string(),
            state: state.to_string(),
            eligible_work_ahead,
            eligible_work_ahead_repo,
            level: row.level.as_str().to_string(),
            max_concurrent: u32_from(summary.max_concurrent),
        });
    }
    rows
}

/// One row's outcome for a [`record`] sample: the row plus how it relates to
/// the previous emission and when its current class began.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Emission {
    pub row: EligRow,
    pub transition: EligibilityTransition,
    pub block_start: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tracked {
    class: Eligibility,
    block_start: DateTime<Utc>,
    emitted_at: DateTime<Utc>,
}

/// Per-`(slug, issue)` last-emitted eligibility class, when that class began,
/// and when it was last emitted (Issue #12790). Modeled on
/// [`super::ops::disposition::DispositionTracker`]: a pure [`Self::diff`]
/// driven by a process-global instance only at the [`record`] seam.
#[derive(Debug, Default)]
pub struct EligibilityTracker {
    last: HashMap<(String, u32), Tracked>,
}

impl EligibilityTracker {
    /// Fold one sample's rows into the tracker, returning every row that must
    /// be emitted this sample: a class flip (or first sight) as `changed`, or
    /// an unchanged class past the refresh window as `refresh`.
    ///
    /// `block_start` is anchored to `tick_at` (the work-finder tick the rows
    /// came from) on a flip, and preserved across same-class samples.
    /// `failed_slugs` are repos whose listing this tick was not whole: a key in
    /// one is kept untouched (neither emitted nor forgotten), so a transient
    /// listing gap never re-anchors an item's class.
    pub fn diff(
        &mut self,
        rows: &[EligRow],
        failed_slugs: &[String],
        tick_at: DateTime<Utc>,
        now: DateTime<Utc>,
        refresh: Duration,
    ) -> Vec<Emission> {
        let mut next: HashMap<(String, u32), Tracked> = HashMap::new();
        let mut emissions = Vec::new();

        for row in rows {
            let key = (row.slug.clone(), row.issue);
            let previous = self.last.get(&key).copied();
            let (transition, block_start) = match previous {
                Some(state) if state.class == row.eligibility => {
                    // Same class: refresh only once the window elapsed.
                    if now.signed_duration_since(state.emitted_at) >= refresh {
                        (Some(EligibilityTransition::Refresh), state.block_start)
                    } else {
                        (None, state.block_start)
                    }
                }
                // First sight, or a class flip: re-anchor to this tick.
                _ => (Some(EligibilityTransition::Changed), tick_at),
            };
            let emitted_at = match (transition, previous) {
                (Some(_), _) | (None, None) => now,
                (None, Some(state)) => state.emitted_at,
            };
            next.insert(
                key,
                Tracked {
                    class: row.eligibility,
                    block_start,
                    emitted_at,
                },
            );
            if let Some(transition) = transition {
                emissions.push(Emission {
                    row: row.clone(),
                    transition,
                    block_start,
                });
            }
        }

        // Carry forward keys in a not-whole repo's listing; drop the rest (a
        // reappearance reads as first sight).
        let seen: HashSet<(String, u32)> = rows.iter().map(|r| (r.slug.clone(), r.issue)).collect();
        for (key, state) in &self.last {
            if seen.contains(key) {
                continue;
            }
            if failed_slugs.iter().any(|slug| slug == &key.0) {
                next.insert(key.clone(), *state);
            }
        }

        self.last = next;
        emissions
    }
}

/// The refresh threshold: `raw` seconds when it is a positive integer,
/// otherwise [`DEFAULT_REFRESH_SECS`].
#[must_use]
pub fn refresh_secs(raw: Option<&str>) -> i64 {
    raw.and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_REFRESH_SECS)
}

/// Turn one [`Emission`] into its wire record.
#[must_use]
pub fn build_record(
    emission: &Emission,
    host: &str,
    sampled_at: DateTime<Utc>,
    tick_at: DateTime<Utc>,
) -> DispatchEligibilityRecord {
    let row = &emission.row;
    DispatchEligibilityRecord {
        schema_version: DISPATCH_ELIGIBILITY_SCHEMA_VERSION,
        repo: row.slug.clone(),
        issue: row.issue,
        host: host.to_string(),
        eligibility: row.eligibility,
        reason: row.reason.clone(),
        state: row.state.clone(),
        block_start: emission.block_start,
        transition: emission.transition,
        rank: row.rank,
        eligible_work_ahead: row.eligible_work_ahead,
        eligible_work_ahead_repo: row.eligible_work_ahead_repo,
        resource_pool: row.slug.clone(),
        max_concurrent: row.max_concurrent,
        level: row.level.clone(),
        sampled_at,
        tick_at,
        visibility: row.visibility,
    }
}

static TRACKER: OnceLock<Mutex<EligibilityTracker>> = OnceLock::new();
static LAST_SAMPLED_TICK: Mutex<Option<DateTime<Utc>>> = Mutex::new(None);

/// Sample the last work-finder tick and emit an `eta.dispatch_eligibility`
/// record for every waiting ready item [`EligibilityTracker::diff`] says needs
/// one. A no-op (no reads, no tracker mutation) without the OTLP sink, and a
/// no-op when the work finder has not ticked since the previous sample. Makes
/// no forge call: repo roots resolve only through the synchronous cache.
pub(super) fn record() {
    let Some(sink) = GLOBAL_SINK.get() else {
        return;
    };
    let Some(summary) = crate::work_finder::last_tick_summary() else {
        return;
    };
    let previous_sample = *LAST_SAMPLED_TICK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !super::queue_snapshot::is_new_tick(summary.at, previous_sample) {
        return;
    }
    let rows = build_rows(&summary, |root| cached_repo_ref(root));
    let failed_slugs: Vec<String> = summary
        .listing_not_whole()
        .filter_map(|root| cached_repo_ref(root).map(|r| r.repo))
        .collect();
    let now = Utc::now();
    let refresh = Duration::seconds(refresh_secs(std::env::var(REFRESH_SECS_ENV).ok().as_deref()));
    let emissions = TRACKER
        .get_or_init(|| Mutex::new(EligibilityTracker::default()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .diff(&rows, &failed_slugs, summary.at, now, refresh);
    for emission in &emissions {
        sink.push(build_record(emission, &sink.host_id, now, summary.at));
    }
    *LAST_SAMPLED_TICK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(summary.at);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "dispatch_eligibility_tests.rs"]
mod tests;
