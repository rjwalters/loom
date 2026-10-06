//! `pr.resolved` (#10519): a PR the daemon saw leave the review listings,
//! with its merge or close instant.
//!
//! # Why this kind exists
//!
//! The SigNoz timeline reader (`eta::fleet_signoz_timeline`) needs a merge or
//! close instant for every PR. The webhook's `closed` rows carry one with the
//! exact receipt time, but the webhook export does not cover every window
//! (no `rjwalters/loom` before 2026-09-27). The stage journal's `pr.resolved`
//! rows stay on the host that wrote them. This record puts the same fact in
//! SigNoz.
//!
//! # No new forge read
//!
//! A record is built only from the `pr.resolved` journal rows the ETA pass
//! already writes. The pass gets those from its existing review listing (the
//! PR left it) and the PR read the tracker already makes for such a PR
//! ([`from_journal`]). Nothing here reads the forge.
//!
//! # Times
//!
//! - `resolved_at` is the event time. For a merge it is the forge's
//!   `merged_at`, so `resolution_sec` is `0`. For a close the forge read
//!   carries no close instant, so it is the pass that saw the closed PR, and
//!   `resolution_sec` is the listing interval: the close happened at most that
//!   long before.
//! - `observed_at` is when this daemon observed it, the knowable-at time
//!   (`eta::point_in_time`). It is the OTLP `observed_timestamp`, and it is
//!   never earlier than `resolved_at`.
//!
//! **OTLP only**, like the `eta.*` log kinds, and emitted only by the fleet's
//! ETA authority, because only the authority runs the pass. **Provenance is
//! required**: a record whose build provenance does not validate is never
//! emitted.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use crate::eta::journal::JournalEntry;
use crate::eta::Provenance;

/// How the PR left the review listings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrResolution {
    /// Merged; `resolved_at` is the forge's `merged_at`.
    Merged,
    /// Closed without a merge; `resolved_at` is the observation.
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
    /// The issue the tracker follows the PR for, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<u32>,
    /// `merged` or `closed`.
    pub state: PrResolution,
    /// The event time (see the module docs).
    pub resolved_at: DateTime<Utc>,
    /// When this daemon observed it: the knowable-at time.
    pub observed_at: DateTime<Utc>,
    /// How late `resolved_at` can be, in seconds: `0` for a merge, the
    /// listing interval for a close.
    pub resolution_sec: i64,
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

/// The `pr.resolved` records for one pass's journal rows, observed at
/// `observed_at`. `close_resolution_sec` is the listing interval.
///
/// One record per `(repo, PR, state)`: a merged PR that was held also writes
/// a `merge_hold` row with the same fact, and it is not counted twice. Rows
/// with no PR number or with another state (`open`) give no record.
#[must_use]
pub fn from_journal(
    rows: &[JournalEntry],
    observed_at: DateTime<Utc>,
    close_resolution_sec: i64,
    loom: &Provenance,
) -> Vec<PrResolvedRecord> {
    let mut seen = BTreeSet::new();
    let mut records = Vec::new();
    for row in rows.iter().filter(|row| row.event == "pr.resolved") {
        let Some(pr_number) = row
            .pr_number
            .or_else(|| row.raw["pr"].as_u64().and_then(|n| u32::try_from(n).ok()))
        else {
            continue;
        };
        let (state, resolved_at, resolution_sec) = match row.raw["state"].as_str() {
            Some("merged") => (PrResolution::Merged, row.left_at.unwrap_or(row.observed_at), 0),
            Some("closed") => (PrResolution::Closed, observed_at, close_resolution_sec.max(0)),
            _ => continue,
        };
        if !seen.insert((row.repo.to_ascii_lowercase(), pr_number, state)) {
            continue;
        }
        records.push(PrResolvedRecord {
            repo: row.repo.clone(),
            pr_number,
            issue: row.issue,
            state,
            resolved_at: resolved_at.min(observed_at),
            observed_at,
            resolution_sec,
            loom: loom.clone(),
        });
    }
    records
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "pr_resolved_tests.rs"]
mod tests;
