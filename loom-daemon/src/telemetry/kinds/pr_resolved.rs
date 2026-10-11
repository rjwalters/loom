//! `pr.resolved` (#10519, re-homed by #11126): a PR that left the review
//! listings, with its forge merge or close instant.
//!
//! # Why this kind exists
//!
//! A SigNoz reader needs a merge or close instant for every PR. The loom-ui
//! webhook export does not cover every window, so this record puts the same
//! fact in SigNoz. When both exist, the webhook row is primary.
//!
//! # Producer
//!
//! `observability::fleet_state::outcomes`: a PR that left a repo's review
//! listings between two complete `fleet.state` listings is read once
//! (`pulls/{n}`) for its state and its `merged_at` / `closed_at`. Every host
//! that observes it emits it; `loom.fact_id` collapses the duplicates.
//!
//! # Times
//!
//! - `resolved_at` is the event time: the forge's `merged_at` for a merge,
//!   its `closed_at` for a close, so `resolution_sec` is `0`.
//! - `closed_at` is the forge's `closed_at`, part of the fact key, so a PR
//!   closed, reopened and closed again is two facts.
//! - `observed_at` is when this daemon observed it, the knowable-at time. It
//!   is the OTLP `observed_timestamp`, and it is never earlier than
//!   `resolved_at`.
//!
//! **OTLP only.** **Provenance is required**: a record whose build provenance
//! does not validate is never emitted.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::provenance::Provenance;

/// How the PR left the review listings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrResolution {
    /// Merged; `resolved_at` is the forge's `merged_at`.
    Merged,
    /// Closed without a merge; `resolved_at` is the forge's `closed_at`.
    Closed,
}

impl PrResolution {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            PrResolution::Merged => "merged",
            PrResolution::Closed => "closed",
        }
    }
}

/// One PR, resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrResolvedRecord {
    /// `owner/repo`.
    pub repo: String,
    /// The PR.
    pub pr_number: u32,
    /// The issue the PR body links, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<u32>,
    /// `merged` or `closed`.
    pub state: PrResolution,
    /// The event time (see the module docs).
    pub resolved_at: DateTime<Utc>,
    /// When this daemon observed it: the knowable-at time.
    pub observed_at: DateTime<Utc>,
    /// How late `resolved_at` can be, in seconds: `0` for a forge instant.
    pub resolution_sec: i64,
    /// The forge's `closed_at` (a merge closes the PR too). Part of the
    /// `loom.fact_id` key; absent on a record from an older build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<DateTime<Utc>>,
    /// The observing build.
    pub loom: Provenance,
}

impl PrResolvedRecord {
    /// Whether the record carries valid provenance.
    #[must_use]
    pub fn has_provenance(&self) -> bool {
        self.loom.is_valid()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "pr_resolved_tests.rs"]
mod tests;
