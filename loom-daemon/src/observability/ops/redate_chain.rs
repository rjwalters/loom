//! Merge-chain re-date pressure (Issue #10163): how many PRs the #8508
//! re-date remedy is cycling, how many times the worst one has been re-dated,
//! and how long the slowest one took to land.
//!
//! The 2026-10-04 livelock (chain head #9832 re-dated three times in ~40 min
//! while `main` kept moving, 31 approved PRs queued behind it) was invisible
//! on the dashboard: nothing exported re-date counts at all. This emitter
//! turns the per-PR view in [`crate::merge_pr::redate::chain_telemetry`] into
//! host-level gauges.
//!
//! # Cost and posture
//!
//! On the collector's `host.health` cadence, and only when the OTLP ops sink
//! is registered: per managed repo, one local `git log` over the trailing
//! [`WINDOW_HOURS`] plus one local `git log` per re-dated PR (capped by
//! `chain_telemetry::MAX_LANDING_LOOKUPS`). No fetch and no forge call, so it
//! sees PR branches as of the clone's last fetch. Repos are deduplicated by
//! canonical path; every host that manages a repo reports it, so read these
//! gauges with `max` across hosts, not `sum`.
//!
//! # Points (all gauges, never labelled by PR, repo or sha)
//!
//! | metric | labels | value |
//! |---|---|---|
//! | `loom.merge.redate_prs` | `state` ∈ `landed`, `pending`, `stuck` | PRs with ≥ 1 re-date in the window; `stuck` = pending with at least the default re-date budget spent (a subset of `pending`) |
//! | `loom.merge.redates_max` | `state` ∈ `landed`, `pending` | most re-dates on one PR |
//! | `loom.merge.time_to_land_max` | — | longest first-re-date → landing merge, seconds; omitted when nothing landed |
//!
//! `redate_prs` and `redates_max` are emitted every sample, zeros included, so
//! a quiet window reads `0` rather than as a missing series.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use chrono::{TimeDelta, Utc};

use crate::merge_pr::redate::chain_telemetry::{self, ChainStat};
use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::workspace_pool::WorkspacePool;

/// Trailing window the gauges cover, hours. Matches the default
/// `merge-pr redate-report --since`.
pub const WINDOW_HOURS: i64 = 24;

/// The ref landings are resolved against: the clone's `origin/HEAD` when it
/// is set, else `origin/main`.
fn base_ref(root: &Path) -> String {
    let resolves = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "--quiet", "origin/HEAD"])
        .output()
        .is_ok_and(|o| o.status.success());
    if resolves {
        "origin/HEAD"
    } else {
        "origin/main"
    }
    .to_string()
}

/// The gauges for `stats` (every repo on this host, concatenated).
#[must_use]
pub fn points(stats: &[ChainStat]) -> Vec<MetricPoint> {
    let (landed, pending): (Vec<&ChainStat>, Vec<&ChainStat>) =
        stats.iter().partition(|s| s.landed_at.is_some());
    let max_of = |set: &[&ChainStat]| set.iter().map(|s| s.redates).max().unwrap_or(0);
    let count = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
    let stuck = stats.iter().filter(|s| s.stuck).count();
    let mut out = vec![
        MetricPoint::int(MetricName::MergeRedatePrs, count(landed.len())).label("state", "landed"),
        MetricPoint::int(MetricName::MergeRedatePrs, count(pending.len()))
            .label("state", "pending"),
        MetricPoint::int(MetricName::MergeRedatePrs, count(stuck)).label("state", "stuck"),
        MetricPoint::int(MetricName::MergeRedatesMax, count(max_of(&landed)))
            .label("state", "landed"),
        MetricPoint::int(MetricName::MergeRedatesMax, count(max_of(&pending)))
            .label("state", "pending"),
    ];
    if let Some(secs) = landed.iter().filter_map(|s| s.time_to_land_secs).max() {
        out.push(MetricPoint::int(MetricName::MergeTimeToLandMax, secs));
    }
    out
}

/// Collect every repo's per-PR stats. A repo whose `git log` fails is skipped.
fn sample(roots: &[PathBuf]) -> Vec<ChainStat> {
    let since = Utc::now() - TimeDelta::hours(WINDOW_HOURS);
    let mut seen = BTreeSet::new();
    let mut all = Vec::new();
    for root in roots {
        let key = root.canonicalize().unwrap_or_else(|_| root.clone());
        if !seen.insert(key) {
            continue;
        }
        if let Ok(stats) = chain_telemetry::collect(root, &base_ref(root), since) {
            all.extend(stats);
        }
    }
    all
}

/// Sample every managed repo and export the gauges. A no-op (no git at all)
/// without the OTLP ops sink.
pub(in crate::observability) async fn record(workspace_pool: &WorkspacePool) {
    let Some(sink) = super::global_ops_sink() else {
        return;
    };
    let roots = crate::observability::collector::provisioned_roots(workspace_pool);
    if let Ok(stats) = tokio::task::spawn_blocking(move || sample(&roots)).await {
        sink.emit_metrics(points(&stats));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "redate_chain_tests.rs"]
mod tests;
