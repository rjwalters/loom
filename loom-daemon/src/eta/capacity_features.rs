//! Fleet capacity and health at `as_of` (#10959, slice 1 of the capacity
//! features): live worker slots, main-branch CI state, CI duration, and the
//! authority's token pool / rate-limit state.
//!
//! One builder, [`build`], reads a [`Timeline`] (SigNoz rows knowable at the
//! timeline's cutoff, which **is** `as_of`: the builder takes no separate
//! instant, so a row observed after `as_of` cannot reach a value). The
//! persisted series ([`super::capacity_log`]) stores its output; the fit and
//! the tracker read that one series, so a backfilled row and a live row for
//! one instant are the same function of the same rows.
//!
//! # Definitions
//!
//! Every value that can be unknown is an `Option`; `None` is "not known",
//! never zero.
//!
//! - `slots_fleet`: the sum of `max_concurrent` over hosts whose latest
//!   `queue.snapshot` ticked within [`HOST_LIVE_TICKS`] intervals
//!   ([`QUEUE_TICK_SEC`]) before `as_of`. A host that stated `max_concurrent
//!   0` counts 0 slots (it is live, with no capacity); a host that stated
//!   nothing is not counted. When hosts have been seen but none is live the
//!   fleet has 0 slots; when none was ever seen the value is `None`.
//! - `hosts_live`: the hosts counted above (stated or not).
//! - `running_fleet` / `slot_util_fleet`: the live hosts' `running` count and
//!   `running / slots` (`None` when slots are 0 or unknown).
//! - `main_red`: some latest workflow run of the default branch concluded
//!   `failure`, `timed_out` or `startup_failure`; `None` when the branch has
//!   no run. `since_main_green_sec` is 0 when not red, else the time since
//!   the latest successful run of the branch (any workflow), capped at
//!   [`SINCE_MAIN_GREEN_CAP_SEC`].
//! - `ci_dur_p50_24h_ms`: the median `duration_ms` of the default branch's
//!   runs completed in the 24 h before `as_of`.
//! - `pool_usable`, `pool_exhausted`, `breaker_open`, `reader_quota_min`:
//!   the authority host's own readings ([`with_stall`], from
//!   [`super::stall_features`]); fleet-wide values live only in metrics no
//!   ETA reader queries yet (deferred).

use super::explanation::{FeatureOmitted, Features};
use super::fleet_signoz_timeline::{CiState, Timeline};
use super::stall_features::StallSnapshot;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// The `queue.snapshot` cadence: one tick per ten minutes.
pub const QUEUE_TICK_SEC: i64 = 600;

/// A host is live while its latest snapshot is at most this many ticks old.
pub const HOST_LIVE_TICKS: i64 = 2;

/// Cap on `since_main_green_sec` (7 days), like `SINCE_MERGE_CAP_SEC`.
pub const SINCE_MAIN_GREEN_CAP_SEC: i64 = 7 * 24 * 3600;

/// Refs tried, in order, as the repo's default branch.
pub const DEFAULT_REFS: [&str; 2] = ["main", "master"];

/// The look-back of the CI duration percentile.
pub const CI_DURATION_WINDOW_SEC: i64 = 24 * 3600;

/// The capacity features at one instant for one repo.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CapacityFeatures {
    pub hosts_live: u32,
    pub slots_fleet: Option<u32>,
    pub running_fleet: Option<u32>,
    pub slot_util_fleet: Option<f64>,
    pub main_red: Option<bool>,
    pub since_main_green_sec: Option<i64>,
    pub ci_dur_p50_24h_ms: Option<i64>,
    pub pool_usable: Option<u32>,
    pub pool_exhausted: Option<bool>,
    pub breaker_open: Option<bool>,
    pub reader_quota_min: Option<u32>,
}

fn is_red(state: &CiState) -> bool {
    state.latest.values().any(|r| {
        matches!(r.run.conclusion.as_deref(), Some("failure" | "timed_out" | "startup_failure"))
    })
}

/// The timeline-derived features for `repo` as knowable at `timeline.cutoff`.
#[must_use]
pub fn build(timeline: &Timeline, repo: &str) -> CapacityFeatures {
    let as_of = timeline.cutoff;
    let mut out = CapacityFeatures::default();

    let live_from = as_of - Duration::seconds(QUEUE_TICK_SEC * HOST_LIVE_TICKS);
    let live: Vec<_> = timeline
        .hosts
        .values()
        .filter(|h| h.tick_at >= live_from && h.tick_at <= as_of)
        .collect();
    out.hosts_live = u32::try_from(live.len()).unwrap_or(u32::MAX);
    if !timeline.hosts.is_empty() {
        let slots: u32 = live.iter().filter_map(|h| h.capacity.max_concurrent).sum();
        let running: u32 = live.iter().filter_map(|h| h.capacity.running).sum();
        out.slots_fleet = Some(slots);
        out.running_fleet = Some(running);
        out.slot_util_fleet = (slots > 0).then(|| f64::from(running) / f64::from(slots));
    }

    let ci = DEFAULT_REFS
        .iter()
        .find_map(|r| timeline.ci_for_ref(repo, r))
        .filter(|s| !s.latest.is_empty());
    if let Some(state) = ci {
        let red = is_red(state);
        out.main_red = Some(red);
        out.since_main_green_sec = Some(if red {
            state
                .last_success_at
                .map_or(SINCE_MAIN_GREEN_CAP_SEC, |at| {
                    (as_of - at.min(as_of))
                        .num_seconds()
                        .clamp(0, SINCE_MAIN_GREEN_CAP_SEC)
                })
        } else {
            0
        });
        let from = as_of - Duration::seconds(CI_DURATION_WINDOW_SEC);
        let mut durations: Vec<i64> = state
            .run_durations
            .iter()
            .filter(|(at, _)| *at > from && *at <= as_of)
            .map(|(_, ms)| *ms)
            .collect();
        durations.sort_unstable();
        if !durations.is_empty() {
            out.ci_dur_p50_24h_ms = Some(durations[(durations.len() - 1) / 2]);
        }
    }
    out
}

/// Fill the authority-only fields from `snapshot` (the same reads and
/// omission rules as `eta.estimate`'s stall features), as of `as_of`.
#[must_use]
pub fn with_stall(
    mut base: CapacityFeatures,
    snapshot: &StallSnapshot,
    repo: &str,
    as_of: DateTime<Utc>,
) -> CapacityFeatures {
    let mut features = Features::default();
    let mut omitted: Vec<FeatureOmitted> = Vec::new();
    super::stall_features::write_to(Some(snapshot), repo, as_of, &mut features, &mut omitted);
    base.pool_usable = features.pool_usable_accounts;
    base.pool_exhausted = features.pool_exhausted;
    base.breaker_open = features.breaker_state.as_deref().map(|s| s != "closed");
    base.reader_quota_min = features.ratelimit_min_remaining;
    base
}

/// The authority-only fields of `base` replaced by `other`'s, and `other`'s
/// timeline fields ignored: merging a live stall reading into a backfilled
/// row of the same instant.
#[must_use]
pub fn merge_stall(mut base: CapacityFeatures, other: &CapacityFeatures) -> CapacityFeatures {
    base.pool_usable = other.pool_usable;
    base.pool_exhausted = other.pool_exhausted;
    base.breaker_open = other.breaker_open;
    base.reader_quota_min = other.reader_quota_min;
    base
}
