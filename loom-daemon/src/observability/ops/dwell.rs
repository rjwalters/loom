//! Ready-queue dwell and starvation telemetry (Issue #8856).
//!
//! The work finder already records, per tick, every ready issue it listed and
//! what it did with it (#8852, [`TickReport::queue`]). This module adds the
//! time axis: how long each issue has been **waiting**, how long the issues
//! dispatched this tick waited, and how many have waited past the starvation
//! threshold.
//!
//! # Where dwell starts: no forge calls
//!
//! Each waiting `(repo, issue)` gets a clock the first tick it is seen
//! waiting, seeded with `min(now, updatedAt)` from the listing the tick
//! already fetched. Applying `loom:issue` bumps `updatedAt`, so the seed is
//! never earlier than the real start of the wait: dwell is a **lower bound**.
//! It under-reports when the issue was touched after it became ready (a
//! comment), and never over-reports, so a starvation alert cannot fire
//! falsely. Once seeded, the clock stays put while the issue keeps waiting.
//!
//! A clock is dropped when the issue leaves the listing, is dispatched, starts
//! running, or moves to a state that is not waiting (see [`wait_class`]). A
//! later re-seed reads `updatedAt` again, so it may include time the issue
//! spent in a non-waiting hold that did not touch it (a peer claim, say);
//! that is still a lower bound on time since `loom:issue` was applied. A
//! workspace whose listing failed this tick keeps its clocks, so a transient
//! `gh` failure does not reset dwell. A daemon restart loses the clocks; they
//! re-seed from `updatedAt`, which is still a lower bound. A per-issue
//! `labeled` event lookup would be more precise but costs a REST call per
//! ready issue per restart per host, and was rejected at curation.
//!
//! Only the multi-workspace tick records queue rows, so only it feeds this
//! module; the single-workspace loop (no roots) emits nothing here.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};

use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::types::QueueDisposition;
use crate::work_finder::{ready_queue, TickReport};

/// Env override for the starvation threshold, in seconds.
pub const STARVATION_SECS_ENV: &str = "LOOM_QUEUE_STARVATION_SECS";
/// Default starvation threshold: six hours.
pub const DEFAULT_STARVATION_SECS: i64 = 6 * 60 * 60;

/// Whether a queue row counts as waiting, and in which coarse state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WaitClass {
    /// Held only by capacity-style limits.
    Ready,
    /// Held by an automatic, issue- or repo-specific hold.
    Blocked,
}

impl WaitClass {
    /// The `state` label value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Blocked => "blocked",
        }
    }
}

/// The wait class of `disposition`, or `None` when the row is not waiting:
/// running (`dispatched`, `in_flight`), or held deliberately or elsewhere
/// (a park or hard-exclusion label, a decline, another host's affinity or
/// claim, an open PR, the issue's own recheck interval). Those holds are not
/// starvation, so they carry no clock.
#[must_use]
pub fn wait_class(disposition: QueueDisposition) -> Option<WaitClass> {
    use QueueDisposition as D;
    match disposition {
        D::DeferredCapacity
        | D::DeferredRampCap
        | D::DeferredSaturation
        | D::DeferredOutOfSlice => Some(WaitClass::Ready),
        D::WorkspaceHalted
        | D::WorkspaceCommandsMissing
        | D::Quarantined
        | D::DispatchBackoff
        | D::NoopCooldown
        | D::PrlessRetry
        | D::DispatchError => Some(WaitClass::Blocked),
        D::Dispatched
        | D::InFlight
        | D::Parked
        | D::HardExclusion
        | D::Declined
        | D::HostConstraint
        | D::PeerClaim
        | D::OpenPr
        | D::OpenPrBackoff
        | D::RecheckInterval
        | D::LabelledBlocked
        | D::Unknown => None,
    }
}

/// The snake_case wire name of `disposition` (the `reason` label value).
fn disposition_name(disposition: QueueDisposition) -> String {
    serde_json::to_value(disposition)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

/// One queue row, as the tracker needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DwellRow {
    pub repo: String,
    pub issue: u32,
    pub disposition: QueueDisposition,
    /// The listing's `updatedAt`, when it supplied a parseable one.
    pub updated_at: Option<DateTime<Utc>>,
}

/// One waiting row's dwell this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Waiting {
    pub class: WaitClass,
    pub disposition: QueueDisposition,
    pub secs: i64,
}

/// What one tick observed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DwellObservation {
    /// Every waiting row with its dwell so far.
    pub waiting: Vec<Waiting>,
    /// Dwell of each issue dispatched this tick.
    pub dispatch_waits: Vec<i64>,
}

/// Per-`(repo, issue)` dwell clocks.
#[derive(Debug, Default)]
pub struct DwellTracker {
    since: HashMap<(String, u32), DateTime<Utc>>,
}

impl DwellTracker {
    /// Fold one tick's rows into the clocks. `incomplete_repos` are the
    /// workspaces whose listing failed this tick; their clocks are kept.
    pub fn observe(
        &mut self,
        rows: &[DwellRow],
        incomplete_repos: &[String],
        now: DateTime<Utc>,
    ) -> DwellObservation {
        let mut next: HashMap<(String, u32), DateTime<Utc>> = self
            .since
            .iter()
            .filter(|((repo, _), _)| incomplete_repos.contains(repo))
            .map(|(key, since)| (key.clone(), *since))
            .collect();
        let mut observation = DwellObservation::default();
        for row in rows {
            let key = (row.repo.clone(), row.issue);
            let since = self
                .since
                .get(&key)
                .copied()
                .unwrap_or_else(|| row.updated_at.map_or(now, |u| u.min(now)));
            let secs = (now - since).num_seconds().max(0);
            if row.disposition == QueueDisposition::Dispatched {
                observation.dispatch_waits.push(secs);
            } else if let Some(class) = wait_class(row.disposition) {
                next.insert(key, since);
                observation.waiting.push(Waiting {
                    class,
                    disposition: row.disposition,
                    secs,
                });
            }
        }
        self.since = next;
        observation
    }

    /// Number of live clocks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.since.len()
    }

    /// Whether no clock is live.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.since.is_empty()
    }
}

/// The tick's points. `loom.queue.starved` is emitted for both states even at
/// zero, so an alert on it resolves; every other name only when it has data.
#[must_use]
pub fn points(observation: &DwellObservation, starvation_secs: i64) -> Vec<MetricPoint> {
    let mut points = Vec::new();
    let mut by_reason: BTreeMap<String, i64> = BTreeMap::new();
    for class in [WaitClass::Ready, WaitClass::Blocked] {
        let rows: Vec<&Waiting> = observation
            .waiting
            .iter()
            .filter(|w| w.class == class)
            .collect();
        if let Some(oldest) = rows.iter().map(|w| w.secs).max() {
            points.push(
                MetricPoint::int(MetricName::QueueOldestWait, oldest)
                    .label("state", class.as_str()),
            );
        }
        let starved: Vec<&&Waiting> = rows.iter().filter(|w| w.secs >= starvation_secs).collect();
        for w in &starved {
            *by_reason
                .entry(disposition_name(w.disposition))
                .or_default() += 1;
        }
        let count = i64::try_from(starved.len()).unwrap_or(i64::MAX);
        points
            .push(MetricPoint::int(MetricName::QueueStarved, count).label("state", class.as_str()));
    }
    points.extend(by_reason.into_iter().map(|(reason, count)| {
        MetricPoint::int(MetricName::QueueStarvedByReason, count).label("reason", reason)
    }));
    if !observation.dispatch_waits.is_empty() {
        let total = observation
            .dispatch_waits
            .iter()
            .copied()
            .fold(0_i64, i64::saturating_add);
        let samples = i64::try_from(observation.dispatch_waits.len()).unwrap_or(i64::MAX);
        points.push(MetricPoint::int(MetricName::QueueDispatchWait, total));
        points.push(MetricPoint::int(MetricName::QueueDispatchWaitSamples, samples));
    }
    points
}

/// The starvation threshold: `raw` seconds when it is a positive integer,
/// otherwise [`DEFAULT_STARVATION_SECS`].
#[must_use]
pub fn starvation_secs(raw: Option<&str>) -> i64 {
    raw.and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_STARVATION_SECS)
}

/// The tracker rows for `report`'s queue, with repos named from `roots`.
#[must_use]
pub fn rows_from_report(report: &TickReport, roots: &[PathBuf]) -> Vec<DwellRow> {
    report
        .queue
        .iter()
        .map(|row| DwellRow {
            repo: ready_queue::repo_names(&[row.key.workspace_idx], roots)
                .pop()
                .unwrap_or_default(),
            issue: row.key.number,
            // An unresolved row is a capacity deferral (`ready_queue::finish`).
            disposition: row
                .disposition
                .unwrap_or(QueueDisposition::DeferredCapacity),
            updated_at: row
                .updated_at
                .as_deref()
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&Utc)),
        })
        .collect()
}

static TRACKER: OnceLock<Mutex<DwellTracker>> = OnceLock::new();

/// Export one multi-workspace tick's dwell signals. Returns immediately when no
/// ops sink is registered (the tracker is then never touched) or when `roots`
/// is empty (the single-workspace loop records no queue rows).
pub fn record_tick(report: &TickReport, roots: &[PathBuf], started_at: DateTime<Utc>) {
    let Some(sink) = super::global_ops_sink() else {
        return;
    };
    if roots.is_empty() {
        return;
    }
    let rows = rows_from_report(report, roots);
    let incomplete = ready_queue::repo_names(&report.listing_failed, roots);
    let observation = TRACKER
        .get_or_init(|| Mutex::new(DwellTracker::default()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .observe(&rows, &incomplete, Utc::now());
    let threshold = starvation_secs(std::env::var(STARVATION_SECS_ENV).ok().as_deref());
    sink.emit_metrics_since(points(&observation, threshold), Some(started_at));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
