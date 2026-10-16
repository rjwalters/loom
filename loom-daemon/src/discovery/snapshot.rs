//! The snapshot value: one repo's open items as a tick read them, from either
//! backend.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use crate::forge_listing::RestIssue;
use crate::forge_pull_listing::RestPull;

/// Where a repo's rows came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The fleet mirror's worklist.
    Worklist,
    /// A per-repo forge listing.
    Listing,
}

/// A repo's freshness as the mirror last reported it. Recorded, never acted
/// on: a stale repo is served anyway, because claims and merges re-read the
/// forge live.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatermarkView {
    pub last_event_at: Option<DateTime<Utc>>,
    pub last_reconciled_at: Option<DateTime<Utc>>,
    pub latest_seq: u64,
}

impl WatermarkView {
    /// The newer of the two evidence times.
    #[must_use]
    pub fn newest(&self) -> Option<DateTime<Utc>> {
        self.last_event_at.max(self.last_reconciled_at)
    }

    /// How old the newest evidence is at `now`, in seconds (never negative).
    #[must_use]
    pub fn age_secs(&self, now: DateTime<Utc>) -> Option<u64> {
        let age = (now - self.newest()?).num_seconds();
        Some(u64::try_from(age).unwrap_or(0))
    }
}

/// The check summary of a PR's head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checks {
    /// `pending`, `success` or `failure`, as sent.
    pub state: String,
    pub started_at: Option<String>,
}

/// The fields discovery reads that a listing row has no slot for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemExtras {
    pub assignees: Vec<String>,
    pub blocked_by_total: Option<u32>,
    pub blocked_by_open: Option<u32>,
    /// PRs only.
    pub checks: Option<Checks>,
}

/// One repo of the tick's snapshot.
#[derive(Debug, Clone)]
pub struct RepoSnapshot {
    /// The forge slug as the caller named it.
    pub slug: String,
    pub source: Source,
    /// Every open issue and PR (PRs carry [`RestIssue::is_pull_request`]),
    /// each with its `labels_at` where the source has it.
    pub rows: Vec<RestIssue>,
    /// The open PRs' head, base and draft facts; `None` when the open-PR
    /// listing could not be read (Direct only).
    pub pulls: Option<Vec<RestPull>>,
    /// Number → the fields a row cannot carry.
    pub extras: BTreeMap<u32, ItemExtras>,
    /// The mirror's watermark for the repo (Mirror only).
    pub watermark: Option<WatermarkView>,
}

/// What the tick span records about discovery.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotStats {
    pub backend: &'static str,
    /// Worklist request latency, Mirror only.
    pub worklist_latency_ms: Option<u64>,
    /// The worklist answered `304`, Mirror only.
    pub not_modified: Option<bool>,
    /// The request failed and the previous snapshot stood in.
    pub reused_previous: bool,
    pub repos: usize,
    pub items: usize,
    /// Age of the stalest repo's newest evidence, Mirror only.
    pub oldest_watermark_age_secs: Option<u64>,
    /// Repos whose watermark is older than the mirror's bound.
    pub stale_repos: usize,
    /// Per-repo forge listing calls made for discovery: 0 in Mirror mode.
    /// (Per-item gate reads, such as hold markers and CI at a Judge head,
    /// are not listings and are counted elsewhere.)
    pub discovery_forge_reads: u64,
}
