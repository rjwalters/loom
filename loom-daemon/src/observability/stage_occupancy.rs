//! `eta.stage_occupancy` emission (Issue #12866): a per-stage occupancy gauge
//! of the items still waiting in `triage_wait`, `approval_wait` and
//! `operator_hold`, over OTLP, so the ETA model sees the right-censored
//! population `eta.stage_outcome` (stage exit only) cannot. Record shape,
//! stage definitions and zero-versus-unknown rules:
//! [`crate::telemetry::kinds::stage_occupancy`].
//!
//! # Where this runs, and what it costs
//!
//! Driven from the collector's periodic sample
//! (`observability::collector::sample_snapshots`), never from a role or
//! work-finder tick, on the same cadence and with the same listing machinery as
//! the `loom:blocked` rows of `queue.snapshot` ([`super::queue_blocked`]):
//!
//! - **Mirror mode** (#12307): the counts come from the tick's published
//!   snapshot, which holds every open issue of each repo it read. No forge call.
//! - **Otherwise**: one ETag-cached, all-pages listing per stage label per repo
//!   (`loom:triage`, `loom:curated`, `loom:operator`). An unchanged listing is a
//!   free `304`, and the `loom:triage` / `loom:curated` first pages are the very
//!   cache entries the role-runner triggers keep warm. All-pages matters: a
//!   count taken from a truncated page would be a wrong number, so a partial or
//!   failed listing leaves the stage **unknown** (no row), never zero.
//!
//! The work finder's own listing covers only `loom:issue`, so it cannot supply
//! these stages; that gap is why this reads the queue labels itself.
//!
//! With no OTLP exporter no sink is registered and [`record`] returns before
//! doing any work.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Utc};

use super::queue::{DurableQueue, FanoutQueue, QueueSink};
use super::queue_blocked::{snapshot_rows, Rows};
use crate::forge_listing::RestIssue;
use crate::telemetry::kinds::stage_occupancy::{
    OccupancyScope, StageOccupancyRecord, StageStats, STAGES, STAGE_APPROVAL_WAIT,
    STAGE_OCCUPANCY_SCHEMA_VERSION, STAGE_TRIAGE_WAIT,
};
use crate::telemetry::{RepoVisibility, TelemetryEnvelope, TelemetryRecord};
use crate::workspace_pool::WorkspacePool;

/// `forge_call_stats` caller name for this gauge's listings.
pub const CALLER: &str = "stage_occupancy";

/// Offers `eta.stage_occupancy` envelopes, stamped with this daemon's host id,
/// to the OTLP exporters' queues.
#[derive(Clone)]
pub struct StageOccupancySink {
    queue: Arc<dyn QueueSink>,
    host_id: String,
}

impl StageOccupancySink {
    #[must_use]
    pub fn new(queue: Arc<dyn QueueSink>, host_id: impl Into<String>) -> Self {
        StageOccupancySink {
            queue,
            host_id: host_id.into(),
        }
    }

    /// Enqueue one record.
    pub fn push(&self, record: StageOccupancyRecord) {
        self.queue.offer(TelemetryEnvelope::new(
            self.host_id.clone(),
            TelemetryRecord::StageOccupancy(record),
        ));
    }
}

static GLOBAL_SINK: OnceLock<StageOccupancySink> = OnceLock::new();

/// Register the OTLP queues (called once from [`super::spawn_task`]). With no
/// OTLP exporter nothing is registered, and [`record`] returns before doing any
/// work.
pub fn register_sink(otlp_queues: Vec<Arc<DurableQueue>>, host_id: &str) {
    if otlp_queues.is_empty() {
        return;
    }
    let _ =
        GLOBAL_SINK.set(StageOccupancySink::new(Arc::new(FanoutQueue::new(otlp_queues)), host_id));
}

/// The label whose listing holds `stage`'s population.
#[must_use]
pub fn stage_label(stage: &str) -> &'static str {
    match stage {
        STAGE_TRIAGE_WAIT => "loom:triage",
        STAGE_APPROVAL_WAIT => "loom:curated",
        _ => "loom:operator",
    }
}

fn has_label(item: &RestIssue, label: &str) -> bool {
    item.labels.iter().any(|l| l == label)
}

/// Whether `item` is in `stage`: an open issue (listings include PRs) with the
/// stage's label, and for `approval_wait` without `loom:issue` (an item that
/// carries both is already approved and sits in the ready queue). Pure.
#[must_use]
pub fn in_stage(stage: &str, item: &RestIssue) -> bool {
    !item.is_pull_request
        && item.state.eq_ignore_ascii_case("open")
        && has_label(item, stage_label(stage))
        && (stage != STAGE_APPROVAL_WAIT || !has_label(item, "loom:issue"))
}

/// Seconds `item` has been in `stage` as of `now`: since its stage label was
/// applied when the listing carries that (`labels_at`), else since creation.
/// `None` when neither parses; a future instant (clock skew) is `0`. Pure.
#[must_use]
pub fn age_sec(stage: &str, item: &RestIssue, now: DateTime<Utc>) -> Option<u64> {
    let parse = |s: &str| {
        DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    };
    let entered = item
        .labels_at
        .as_ref()
        .and_then(|at| at.get(stage_label(stage)))
        .and_then(|s| parse(s))
        .or_else(|| item.created_at.as_deref().and_then(parse))?;
    Some(u64::try_from((now - entered).num_seconds()).unwrap_or(0))
}

/// One stage's stats over a repo listing. Pure.
#[must_use]
pub fn stage_stats(stage: &str, listing: &[RestIssue], now: DateTime<Utc>) -> StageStats {
    StageStats::from_ages(
        listing
            .iter()
            .filter(|item| in_stage(stage, item))
            .map(|item| age_sec(stage, item, now)),
    )
}

/// What one repo contributed: `None` per stage whose listing was unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoOccupancy {
    pub slug: String,
    pub visibility: RepoVisibility,
    /// Indexed like [`STAGES`].
    pub stages: [Option<StageStats>; 3],
}

/// The records for one sample: a row per known `(repo, stage)` and a global
/// row per stage over the repos known for it (absent when none are). Pure.
#[must_use]
pub fn build_records(
    host: &str,
    sampled_at: DateTime<Utc>,
    repos: &[RepoOccupancy],
) -> Vec<StageOccupancyRecord> {
    let record = |scope, repo: Option<&String>, stage: &str, stats: &StageStats, visibility| {
        StageOccupancyRecord {
            schema_version: STAGE_OCCUPANCY_SCHEMA_VERSION,
            host: host.to_string(),
            scope,
            repo: repo.cloned(),
            stage: stage.to_string(),
            count: stats.count,
            oldest_age_sec: stats.oldest_age_sec,
            age_buckets: stats.age_buckets.clone(),
            repos_counted: None,
            repos_unknown: None,
            sampled_at,
            visibility,
        }
    };
    let mut out = Vec::new();
    for (i, stage) in STAGES.iter().enumerate() {
        let mut total = StageStats::default();
        let (mut counted, mut unknown) = (0u32, 0u32);
        let mut all_public = true;
        for repo in repos {
            let Some(stats) = &repo.stages[i] else {
                unknown += 1;
                continue;
            };
            counted += 1;
            total.merge(stats);
            all_public &= repo.visibility == RepoVisibility::Public;
            out.push(record(OccupancyScope::Repo, Some(&repo.slug), stage, stats, repo.visibility));
        }
        if counted > 0 {
            let visibility = if all_public {
                RepoVisibility::Public
            } else {
                RepoVisibility::Private
            };
            let mut global = record(OccupancyScope::Global, None, stage, &total, visibility);
            global.repos_counted = Some(counted);
            global.repos_unknown = Some(unknown);
            out.push(global);
        }
    }
    out
}

/// One all-pages, ETag-cached listing of open items carrying `label` in the
/// repo at `root`, off the async runtime. `None` on a failed or partial walk.
async fn list_all(root: PathBuf, label: &'static str) -> Option<Vec<RestIssue>> {
    let shown = root.display().to_string();
    let result = tokio::task::spawn_blocking(move || {
        let _discovery = crate::gh_invocation::purpose::discovery();
        let gh = PathBuf::from(crate::gh_invocation::gh_bin());
        crate::forge_listing::list_issues_cached_all_as(
            CALLER,
            &gh,
            Some(&root),
            None,
            label,
            "open",
        )
    })
    .await;
    match result {
        Ok(Ok(listing)) => Some(listing),
        Ok(Err(error)) => {
            log::debug!(
                "observability: stage occupancy listing {label} in {shown} failed: {error}"
            );
            None
        }
        Err(join_error) => {
            log::debug!(
                "observability: stage occupancy listing {label} in {shown} panicked: {join_error}"
            );
            None
        }
    }
}

/// Every managed repo's occupancy as of `now`.
async fn collect(
    workspace_pool: &WorkspacePool,
    slug_cache: &mut HashMap<String, String>,
    now: DateTime<Utc>,
) -> Vec<RepoOccupancy> {
    let mut repos = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for root in super::collector::provisioned_roots(workspace_pool) {
        let root_str = root.to_string_lossy().to_string();
        let Some(slug) = super::collector::resolve_repo_slug_cached(slug_cache, &root_str).await
        else {
            continue;
        };
        if !seen.insert(slug.clone()) {
            continue;
        }
        let mut stages: [Option<StageStats>; 3] = [None, None, None];
        match snapshot_rows(&root) {
            Rows::Snapshot(rows) => {
                for (i, stage) in STAGES.iter().enumerate() {
                    stages[i] = Some(stage_stats(stage, &rows, now));
                }
            }
            // Mirror mode, and no tick has read this repo: unknown, not zero.
            Rows::Skip => {}
            Rows::List => {
                for (i, stage) in STAGES.iter().enumerate() {
                    if let Some(rows) = list_all(root.clone(), stage_label(stage)).await {
                        stages[i] = Some(stage_stats(stage, &rows, now));
                    }
                }
            }
        }
        let visibility = super::collector::resolve_visibility(&slug).await;
        repos.push(RepoOccupancy {
            slug,
            visibility,
            stages,
        });
    }
    repos
}

/// Take one occupancy sample and offer its records. A no-op without the OTLP
/// sink.
pub(super) async fn record(
    workspace_pool: &WorkspacePool,
    slug_cache: &mut HashMap<String, String>,
) {
    let Some(sink) = GLOBAL_SINK.get() else {
        return;
    };
    let now = Utc::now();
    let repos = collect(workspace_pool, slug_cache, now).await;
    for record in build_records(&sink.host_id, now, &repos) {
        sink.push(record);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "stage_occupancy_tests.rs"]
mod tests;
