//! `eta.stage_occupancy` (#12866): a per-stage occupancy gauge — how many
//! items are waiting in a pre-ready stage right now, and for how long.
//!
//! # Why this exists
//!
//! `eta.stage_outcome` fires only when an item *leaves* a stage, so the items
//! still waiting — the longest-dwell, right-censored population — are
//! invisible to the ETA model, and so is the backlog a new item must clear.
//! `fleet.state` carries no row for these stages (`triage_wait` and
//! `approval_wait` are documented "never a `fleet.state` row"), and
//! `backlog.level` was retired (#12370). This gauge supplies the denominator:
//! per repo and stage, the current `count`, the `oldest_age_sec` and fixed
//! age-bucket counts.
//!
//! # Stages
//!
//! | `stage` | Population |
//! |---|---|
//! | `triage_wait` | open issues carrying `loom:triage` |
//! | `approval_wait` | open issues carrying `loom:curated` and not `loom:issue` |
//! | `operator_hold` | open issues carrying `loom:operator` |
//!
//! The first two reuse the [`FleetStage`](super::fleet_state::FleetStage) wire
//! names; `operator_hold` has no `FleetStage` counterpart.
//!
//! # Age
//!
//! An item's age is `sampled_at` minus its **stage entry**: the time the
//! stage's label was applied when the forge listing carries it
//! (`labels_at`, the state mirror), else the issue's creation time. Creation
//! is an upper bound on the time spent in the stage, exact for `triage_wait`
//! (which starts at creation) and for any item that never left the stage.
//!
//! # Zero versus unknown
//!
//! A scanned repo with an empty stage emits `count: 0`. A repo whose listing
//! failed or was partial emits **nothing** for the stage: absent means
//! unknown, never zero (the `FleetPrCensus` convention). The global rollup
//! (`scope: global`) sums only the repos that were counted and names how many
//! were not in `repos_unknown`, so a non-zero value marks it a lower bound.
//!
//! # Anti-leak
//!
//! Slugs, enums and numbers only. No title, body, label text, author or any
//! other forge text rides in the record. Only `loom.repo` and
//! `loom.repo.visibility` are exported as attributes
//! ([`STAGE_OCCUPANCY_LOG_ATTRIBUTE_KEYS`]); the rest is the body JSON.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::RepoVisibility;

/// Payload version carried in the record's own `schema_version` field
/// (distinct from the envelope's gate, which is `NEW_KIND_SCHEMA_VERSION`).
pub const STAGE_OCCUPANCY_SCHEMA_VERSION: u32 = 1;

/// `stage` wire name: open `loom:triage` issues.
pub const STAGE_TRIAGE_WAIT: &str = "triage_wait";
/// `stage` wire name: open `loom:curated` issues without `loom:issue`.
pub const STAGE_APPROVAL_WAIT: &str = "approval_wait";
/// `stage` wire name: open `loom:operator` issues.
pub const STAGE_OPERATOR_HOLD: &str = "operator_hold";

/// Every stage this gauge reports, in emission order.
pub const STAGES: [&str; 3] = [STAGE_TRIAGE_WAIT, STAGE_APPROVAL_WAIT, STAGE_OPERATOR_HOLD];

/// Upper edges of the age buckets, in seconds: 1 h, 6 h, 24 h, 3 d, 7 d, 30 d.
/// A bucket is lower-inclusive and upper-exclusive, so an age of exactly one
/// edge falls in the bucket above it. [`StageOccupancyRecord::age_buckets`]
/// has `AGE_BUCKET_EDGES_SEC.len() + 1` entries: `[0, 1h)`, `[1h, 6h)`,
/// `[6h, 24h)`, `[24h, 3d)`, `[3d, 7d)`, `[7d, 30d)`, `[30d, ∞)`.
pub const AGE_BUCKET_EDGES_SEC: [u64; 6] = [3_600, 21_600, 86_400, 259_200, 604_800, 2_592_000];

/// Every log attribute key `eta.stage_occupancy` exports. `host` rides on the
/// OTLP Resource as `host.id`; `loom.kind` is added centrally. Both keys here
/// are already on the gateway's log `keep_keys`.
pub const STAGE_OCCUPANCY_LOG_ATTRIBUTE_KEYS: &[&str] = &["loom.repo", "loom.repo.visibility"];

/// Whether a row covers one repo or every counted repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OccupancyScope {
    /// One repo's stage.
    Repo,
    /// The rollup across every repo counted in this sample.
    Global,
}

/// The bucket index of an age: how many [`AGE_BUCKET_EDGES_SEC`] edges are at
/// or below it. Pure.
#[must_use]
pub fn bucket_index(age_sec: u64) -> usize {
    AGE_BUCKET_EDGES_SEC.partition_point(|edge| *edge <= age_sec)
}

/// The occupancy of one stage: count, oldest age and bucket counts. Built from
/// item ages; sums across repos with [`Self::merge`]. Pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageStats {
    pub count: u32,
    /// `None` when `count` is `0`.
    pub oldest_age_sec: Option<u64>,
    pub age_buckets: Vec<u32>,
}

impl Default for StageStats {
    fn default() -> Self {
        StageStats {
            count: 0,
            oldest_age_sec: None,
            age_buckets: vec![0; AGE_BUCKET_EDGES_SEC.len() + 1],
        }
    }
}

impl StageStats {
    /// Stats for items of the given ages (seconds, clamped to `>= 0`). An item
    /// with no usable timestamp (`None`) counts toward `count` but not toward
    /// `oldest_age_sec` or a bucket, so the buckets sum to `count` unless a row
    /// carried none: an unknown age is never fabricated as a young one.
    #[must_use]
    pub fn from_ages(ages: impl IntoIterator<Item = Option<u64>>) -> Self {
        let mut stats = StageStats::default();
        for age in ages {
            stats.count = stats.count.saturating_add(1);
            if let Some(age) = age {
                stats.oldest_age_sec = Some(stats.oldest_age_sec.map_or(age, |o| o.max(age)));
                stats.age_buckets[bucket_index(age)] += 1;
            }
        }
        stats
    }

    /// Fold `other` into `self` (the global rollup).
    pub fn merge(&mut self, other: &StageStats) {
        self.count = self.count.saturating_add(other.count);
        self.oldest_age_sec = match (self.oldest_age_sec, other.oldest_age_sec) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        for (mine, theirs) in self.age_buckets.iter_mut().zip(&other.age_buckets) {
            *mine = mine.saturating_add(*theirs);
        }
    }
}

/// The `eta.stage_occupancy` record: one stage of one repo, or the global
/// rollup of one stage, as of one sample.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageOccupancyRecord {
    /// [`STAGE_OCCUPANCY_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The host whose collector took the sample.
    pub host: String,
    /// [`OccupancyScope::Repo`] or [`OccupancyScope::Global`].
    pub scope: OccupancyScope,
    /// Forge `owner/repo`; absent on a global row. Never a local path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// One of [`STAGES`].
    pub stage: String,
    /// Items in the stage at the sample instant (`0` is a real zero).
    pub count: u32,
    /// Age of the oldest item; absent when `count` is `0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_age_sec: Option<u64>,
    /// Item counts per age bucket; edges are [`AGE_BUCKET_EDGES_SEC`].
    pub age_buckets: Vec<u32>,
    /// Global rows: repos counted into this row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repos_counted: Option<u32>,
    /// Global rows: scanned repos whose listing was unknown for this stage and
    /// so are **not** in the sums. Non-zero marks the row a lower bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repos_unknown: Option<u32>,
    /// The collector sample instant (the record time).
    pub sampled_at: DateTime<Utc>,
    /// The repo's visibility (a global row is public only when every counted
    /// repo is), so the redaction layer can tag the row.
    #[serde(default)]
    pub visibility: RepoVisibility,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn bucket_boundaries_are_lower_inclusive() {
        assert_eq!(bucket_index(0), 0);
        assert_eq!(bucket_index(3_599), 0);
        assert_eq!(bucket_index(3_600), 1);
        assert_eq!(bucket_index(21_599), 1);
        assert_eq!(bucket_index(21_600), 2);
        assert_eq!(bucket_index(86_399), 2);
        assert_eq!(bucket_index(86_400), 3);
        assert_eq!(bucket_index(259_200), 4);
        assert_eq!(bucket_index(604_800), 5);
        assert_eq!(bucket_index(2_591_999), 5);
        assert_eq!(bucket_index(2_592_000), 6);
        assert_eq!(bucket_index(u64::MAX), 6);
        assert_eq!(StageStats::default().age_buckets.len(), AGE_BUCKET_EDGES_SEC.len() + 1);
    }

    #[test]
    fn empty_stage_is_a_zero_with_no_oldest() {
        let stats = StageStats::from_ages([]);
        assert_eq!(stats.count, 0);
        assert_eq!(stats.oldest_age_sec, None);
        assert_eq!(stats.age_buckets, vec![0; 7]);
    }

    #[test]
    fn stats_count_oldest_and_buckets() {
        let stats = StageStats::from_ages([10, 3_600, 90_000, 90_000, 3_000_000].map(Some));
        assert_eq!(stats.count, 5);
        assert_eq!(stats.oldest_age_sec, Some(3_000_000));
        assert_eq!(stats.age_buckets, vec![1, 1, 0, 2, 0, 0, 1]);
    }

    #[test]
    fn an_item_without_a_timestamp_counts_but_is_not_bucketed() {
        let stats = StageStats::from_ages([None, Some(7_200)]);
        assert_eq!(stats.count, 2);
        assert_eq!(stats.oldest_age_sec, Some(7_200));
        assert_eq!(stats.age_buckets.iter().sum::<u32>(), 1);
    }

    #[test]
    fn merge_sums_and_keeps_the_oldest() {
        let mut a = StageStats::from_ages([10, 5_000].map(Some));
        a.merge(&StageStats::from_ages([Some(100_000)]));
        a.merge(&StageStats::default());
        assert_eq!(a.count, 3);
        assert_eq!(a.oldest_age_sec, Some(100_000));
        assert_eq!(a.age_buckets, vec![1, 1, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn record_round_trips_and_omits_absent_fields() {
        let at = "2026-10-20T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let record = StageOccupancyRecord {
            schema_version: STAGE_OCCUPANCY_SCHEMA_VERSION,
            host: "host-a".into(),
            scope: OccupancyScope::Repo,
            repo: Some("rjwalters/loom".into()),
            stage: STAGE_TRIAGE_WAIT.into(),
            count: 0,
            oldest_age_sec: None,
            age_buckets: vec![0; 7],
            repos_counted: None,
            repos_unknown: None,
            sampled_at: at,
            visibility: RepoVisibility::Public,
        };
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(value["scope"], "repo");
        assert_eq!(value["count"], 0);
        assert!(value.get("oldest_age_sec").is_none());
        assert!(value.get("repos_unknown").is_none());
        let back: StageOccupancyRecord = serde_json::from_value(value).unwrap();
        assert_eq!(back, record);
    }
}
