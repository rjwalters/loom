//! `queue.snapshot`: the work finder's ranked ready queue as a record kind for
//! the fleet dashboard (Issue #8852, phase 2).
//!
//! Phase 1 made the queue visible on the host (`loom-daemon queue`, the
//! `serve` panel). This record carries the same rows to the native HTTPS
//! backend, so the fleet dashboard (phase 3) can show what each host has
//! queued, running and blocked, and why.
//!
//! **Native-HTTPS only.** This mirrors `metric.points`, which is OTLP-only.
//! SigNoz gets the queue as low-cardinality gauges instead
//! (`loom.queue.issues{state,reason}`, `observability::ops::queue`). Per-issue
//! rows are high-cardinality and name repositories, so they go only where the
//! redaction layer (`dashboard/src/redaction.ts`) can apply the per-row
//! `visibility` tag.
//!
//! Anti-leak rules, enforced where the record is built
//! (`observability::queue_snapshot`):
//! - `repo` is the forge `owner/repo` slug and never a local path. A row whose
//!   slug cannot be resolved is dropped and counted in `unresolved_rows`.
//! - Every row carries its own [`RepoVisibility`]. A missing or unknown tag
//!   decodes to `Private`.
//! - `detail` is kept only for the structured dispositions (the park label and
//!   the open PR number). Free-form dispatch-error text is never exported.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::RepoVisibility;
use crate::types::QueueDisposition;

/// Most rows one record carries. Rows past this are counted in
/// [`QueueSnapshotRecord::rows_truncated`].
pub const MAX_ROWS: usize = 200;

/// A repository named by the snapshot, with its visibility tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueRepoRef {
    /// Forge `owner/repo`.
    pub repo: String,
    #[serde(default)]
    pub visibility: RepoVisibility,
}

/// Row counts per coarse state ([`QueueDisposition::state`]), over **every**
/// row the tick recorded, including rows dropped as unresolved or truncated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueStateCounts {
    pub running: usize,
    pub ready: usize,
    pub blocked: usize,
}

/// One ready issue, in the work finder's dispatch order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueSnapshotRow {
    /// 1-based position in dispatch order, as ranked on the host. It is not
    /// renumbered after rows are dropped, so a gap means a row was dropped.
    pub rank: usize,
    /// Forge `owner/repo`.
    pub repo: String,
    #[serde(default)]
    pub visibility: RepoVisibility,
    pub issue: u32,
    pub workspace_priority: u32,
    pub urgent: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// The `tier:*` label, which is informational only and does not affect
    /// dispatch order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    pub disposition: QueueDisposition,
    /// `running` / `ready` / `blocked`.
    pub state: String,
    /// Human-readable reason ([`QueueDisposition::reason`]).
    pub reason: String,
    /// Structured specifics only: the park label (`parked`) or the open PR
    /// number (`open_pr`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// `queue.snapshot`: one host's ready queue as of its last work-finder tick.
/// Host-scoped. Each row carries its own repo and visibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueSnapshotRecord {
    /// When the tick this snapshot describes completed. This is the
    /// freshness stamp. A snapshot is emitted only after a new tick, so an
    /// ageing `tick_at` means the work finder has stopped ticking.
    pub tick_at: DateTime<Utc>,
    /// The concurrency cap the tick ran under.
    pub max_concurrent: usize,
    /// Ready issues the tick listed (including those in repos whose rows were
    /// dropped).
    pub seen: usize,
    pub counts: QueueStateCounts,
    /// Repos whose ready listing failed on this tick. Their backlog is absent,
    /// so the snapshot is incomplete rather than empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listing_failed: Vec<QueueRepoRef>,
    /// Workspaces in `listing_failed` whose slug could not be resolved.
    #[serde(default)]
    pub listing_failed_unresolved: usize,
    pub rows: Vec<QueueSnapshotRow>,
    /// Rows dropped because their repo slug could not be resolved.
    #[serde(default)]
    pub unresolved_rows: usize,
    /// Rows dropped by the [`MAX_ROWS`] cap.
    #[serde(default)]
    pub rows_truncated: usize,
}

/// `detail` survives only for dispositions whose detail is structured.
#[must_use]
pub fn exportable_detail(disposition: QueueDisposition, detail: Option<&str>) -> Option<String> {
    match disposition {
        QueueDisposition::Parked | QueueDisposition::OpenPr => detail.map(str::to_string),
        _ => None,
    }
}
