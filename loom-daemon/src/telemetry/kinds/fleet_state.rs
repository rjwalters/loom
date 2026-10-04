//! `fleet.state` (Issue #10196, slice 2): one host's in-flight items and its
//! repos' open-PR census, exported to SigNoz as OTLP logs so the fleet can be
//! rebuilt at any instant `t` from SigNoz alone.
//!
//! # Why a new kind
//!
//! `queue.snapshot` and `eta.snapshot` are native-HTTPS only (dashboard state
//! keys). `queue.snapshot` holds only the ready queue, and neither has an
//! entered-at, host or slot field. This kind is the slim state record a replay
//! reader (`defaults/docs/telemetry-replay.md`) needs. It holds one row per
//! in-flight `(repo, issue)` with stage, entered-at, PR, host and slot, plus a
//! per-repo open-PR census.
//!
//! # Anchors and deltas
//!
//! The emitter (`observability::fleet_state`) runs on the collector's 5-minute
//! snapshot pass. It sends a **full anchor** (`anchor: true`, every row) on its
//! first pass and then at least every [`ANCHOR_INTERVAL_SECS`]. Between anchors
//! it sends a **delta** (`anchor: false`) only when something changed. A delta
//! holds the changed or added rows, the issues that left (`removed`), and the
//! full census of each repo it names. A pass with no change sends nothing.
//!
//! A reader reconstructs `t` by taking the newest anchor knowable before `t`
//! and applying, in `as_of` order, every delta whose `anchor_as_of` names that
//! anchor. Each delta's `prev_as_of` names the record it applies on top of, so
//! a lost delta shows up as a broken chain. The reader then falls back to the
//! next anchor instead of rebuilding wrong state.
//!
//! # Anti-leak rules
//!
//! Every field is written by the daemon: a slug, an enum, a number or an
//! instant. No forge free text (titles, label text, bodies) is carried. `repo`
//! is the forge `owner/repo` slug, never a local path, and each repo entry
//! carries its own [`RepoVisibility`] (missing decodes to `Private`).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::eta::Stage;
use crate::telemetry::RepoVisibility;

/// Schema tag carried in every record.
pub const FLEET_STATE_SCHEMA: &str = "fleet-state/v1";

/// The longest gap between two full anchors, in seconds. A reader never has
/// to look back further than this, plus one snapshot interval, to find a
/// reconstruction base.
pub const ANCHOR_INTERVAL_SECS: i64 = 3600;

/// Most rows one record carries. Measured: a row is ~70–130 bytes of JSON
/// (≈130 with `host` and `slot`), so a full anchor stays under ~65 KB.
/// Rows past the cap are counted in [`FleetStateRecord::rows_truncated`].
/// Rows held by this host's sweeps are kept first, then PR stages, then
/// `ready_wait`.
pub const MAX_ROWS: usize = 500;

/// Log attribute keys this kind exports besides the generic `loom.kind` /
/// `loom.record_id`. The collector's `transform/privacy` log `keep_keys` must
/// list each one (`defaults/observability/collector/config.yaml`,
/// contract-tested).
pub const FLEET_STATE_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.fleet.schema",
    "loom.fleet.anchor",
    "loom.fleet.anchor_as_of",
    "loom.fleet.repos",
    "loom.fleet.rows",
    "loom.fleet.removed",
    "loom.fleet.rows_truncated",
];

/// Which dispatch slot a sweep holds on its host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetSlot {
    /// One of the host's `maxConcurrent` slots.
    Regular,
    /// The host's single `loom:operator-priority` overflow slot (#9244).
    Overflow,
}

/// One in-flight item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetStateRow {
    /// Issue number.
    pub issue: u32,
    /// The ETA stage the item is in.
    pub stage: Stage,
    /// When it entered `stage`.
    pub entered_at: DateTime<Utc>,
    /// `entered_at` is a lower bound (first seen mid-stage, e.g. after a
    /// restart), not an observed transition. Absent when exact.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub entered_at_lower_bound: bool,
    /// The PR the work is in, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u32>,
    /// The host whose sweep holds the item. Present only when **this** host
    /// runs it. An item seen only through a review listing (a PR in review,
    /// built anywhere) has no known host, so the field is absent rather than
    /// guessed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The dispatch slot the sweep holds. Present exactly when `host` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<FleetSlot>,
}

/// One repo's open-PR census at the ETA pass's listing.
///
/// It counts the open PRs under a Loom review label (`loom:review-requested`,
/// `loom:changes-requested`, `loom:pr`), the listings the ETA pass already
/// reads. An open PR carrying none of them is not counted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetPrCensus {
    /// Distinct open PRs under a review label.
    pub open: u32,
    /// `open` by stage: `review_wait`, `doctor`, `merge_wait`, and `held` for
    /// a PR whose labels name no single stage (a hold).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_stage: BTreeMap<String, u32>,
}

/// One repo's part of a record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetStateRepo {
    /// Forge `owner/repo`, lowercased.
    pub repo: String,
    #[serde(default)]
    pub visibility: RepoVisibility,
    /// The census. Absent means unknown at `as_of`, because the repo's
    /// listings were incomplete or not read. It never means zero. A delta
    /// always restates the census of each repo it names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub census: Option<FleetPrCensus>,
    /// On an anchor, every row. On a delta, rows added or changed since the
    /// previous record. Ordered by issue.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<FleetStateRow>,
    /// Delta only: issues that left since the previous record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<u32>,
}

/// Host-level slot use from the last work-finder dispatch plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetSlots {
    /// The configured concurrency cap the plan was held to.
    pub max_concurrent: u32,
    /// Occupied slots when the tick finished, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occupancy: Option<u32>,
}

/// `fleet.state`: one host's in-flight items and open-PR census.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetStateRecord {
    /// Always [`FLEET_STATE_SCHEMA`].
    pub schema: String,
    /// The instant the state describes (the pass's tracker read).
    pub as_of: DateTime<Utc>,
    /// `true` for a full anchor, `false` for a delta.
    pub anchor: bool,
    /// The `as_of` of the anchor this record belongs to. Equal to `as_of` on
    /// an anchor.
    pub anchor_as_of: DateTime<Utc>,
    /// Delta only: the `as_of` of the record this delta applies on top of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_as_of: Option<DateTime<Utc>>,
    /// When the census listings were read. Absent when no listing pass has
    /// completed yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub census_at: Option<DateTime<Utc>>,
    /// Slot use from the last dispatch plan, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slots: Option<FleetSlots>,
    /// On an anchor, every repo with rows or a census. On a delta, only the
    /// repos that changed. Ordered by repo.
    #[serde(default)]
    pub repos: Vec<FleetStateRepo>,
    /// In-flight rows dropped by the [`MAX_ROWS`] cap at this pass.
    #[serde(default)]
    pub rows_truncated: usize,
}

impl FleetStateRecord {
    /// Total rows carried.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.repos.iter().map(|r| r.rows.len()).sum()
    }

    /// Total `removed` entries carried.
    #[must_use]
    pub fn removed_count(&self) -> usize {
        self.repos.iter().map(|r| r.removed.len()).sum()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "fleet_state_tests.rs"]
mod tests;
