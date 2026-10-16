//! The snapshot value: one repo's open items as a tick read them, from either
//! backend.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use super::chain::Tier;
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

/// What the worklist said about one repo (`status`). Anything but
/// [`RepoStatus::Ok`] sends that repo to the fallback tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepoStatus {
    /// The mirror vouches for the repo's rows.
    #[default]
    Ok,
    /// The mirror has never heard of the repo, or cannot vouch for it.
    Unknown,
    /// The mirror is still loading the repo.
    Backfilling,
    /// The mirror failed on this repo.
    Error,
}

impl RepoStatus {
    /// The wire value; an unrecognised one reads as [`Self::Error`] (not
    /// vouched for), an absent one as [`Self::Ok`].
    #[must_use]
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            None | Some("ok") => Self::Ok,
            Some("unknown") => Self::Unknown,
            Some("backfilling") => Self::Backfilling,
            Some(_) => Self::Error,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Unknown => "unknown",
            Self::Backfilling => "backfilling",
            Self::Error => "error",
        }
    }
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
    /// The tier that answered for this repo.
    pub tier: Tier,
    /// The worklist's `status` for the repo ([`RepoStatus::Ok`] off the
    /// worklist).
    pub status: RepoStatus,
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
    /// The tick's tier: the most degraded tier any read repo came from
    /// (`mirror` < `gateway` < `gh` < `none`).
    pub tier: &'static str,
    /// Why some repo left the first tier: `transport`, `status`, `auth`,
    /// `malformed`, `not_live`, `repo_status`, `reserve`, `breaker`, ...
    pub fallback_reason: Option<String>,
    /// The repos listed through a fallback tier this tick (not paced out).
    pub fallback_repos: Vec<String>,
    /// `ok`, `not_modified` or `error`; Mirror only.
    pub worklist_outcome: Option<&'static str>,
    /// The failure's class when [`Self::worklist_outcome`] is `error`.
    pub worklist_error_class: Option<&'static str>,
    /// Worklist request latency, Mirror only.
    pub worklist_latency_ms: Option<u64>,
    /// The worklist answered `304`, Mirror only.
    pub not_modified: Option<bool>,
    /// Repos the tick read, by the worklist's `status` (the repos the
    /// gather asked for; a repo the worklist omitted counts as `unknown`).
    pub repos: usize,
    pub repos_ok: usize,
    pub repos_unknown: usize,
    pub repos_backfilling: usize,
    pub repos_error: usize,
    pub items: usize,
    /// Age of the stalest repo's newest evidence, Mirror only.
    pub oldest_watermark_age_secs: Option<u64>,
    /// Repos whose watermark is older than the mirror's bound.
    pub stale_repos: usize,
    /// Per-repo forge listing calls made for discovery: 0 while every repo
    /// is served by the mirror tier. (Per-item gate reads, such as hold
    /// markers and CI at a Judge head, are not listings and are counted
    /// elsewhere.)
    pub discovery_forge_reads: u64,
}
