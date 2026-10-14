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
//! redaction layer (`loom-ui:src/redaction.ts`) can apply the per-row
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
use crate::types::{DispatchPlanContext, QueueDisposition, RowPlan};

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
    pub workspace_weight: u32,
    /// Deprecated (#9244): always `false`. Its label was deleted in #11105;
    /// kept on the wire only for compatibility.
    pub urgent: bool,
    /// The issue's priority level (#11510): `default`, `important` or
    /// `very-important` (the ready-queue row's `level`; a blocked row reads
    /// its own labels). Absent (an older daemon) reads as `default`.
    #[serde(default)]
    pub level: crate::priority_pick::Level,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    pub disposition: QueueDisposition,
    /// `running` / `ready` / `blocked`.
    pub state: String,
    /// Human-readable reason ([`QueueDisposition::reason`]).
    pub reason: String,
    /// Structured specifics only: the park label (`parked`) or the open PR
    /// number (`open_pr`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Why a `workspace_halted` row is held (#11417): the closed
    /// `work_finder::halt_cause` vocabulary, same tokens as the SigNoz
    /// `loom.queue.halt_cause` attribute. Absent on other dispositions and
    /// when the cause is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub halt_cause: Option<String>,
    /// The dispatch-plan fields (Issue #9288), flattened beside `rank`:
    /// `position`, `plan_state`, `keys`, `gate`, `hot`, `owning_shard`,
    /// `repo_cap` (`in_slice` is retired and absent since #11682). The `keys` names are the four
    /// `candidate_keys` (#11508, #11547): `main_red_fix` (bool, true only for
    /// a verified red-main fix while its repo's `main` is red), `level`
    /// (0 default, 1 `loom:important`, 2 `loom:very-important`),
    /// `created_at`, `number` (this row's `issue`).
    #[serde(flatten, default)]
    pub plan: RowPlan,
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
    /// Repos whose ready listing came back partial on this tick (#11139: a
    /// later page failed, the page cap, a mid-walk change). Some of their
    /// rows are present, but not all: a missing row is not evidence the
    /// issue left the queue. The snapshot is whole only when this,
    /// `listing_failed` and both `*_unresolved` counters are empty/zero.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listing_incomplete: Vec<QueueRepoRef>,
    /// Workspaces in `listing_incomplete` whose slug could not be resolved.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub listing_incomplete_unresolved: usize,
    pub rows: Vec<QueueSnapshotRow>,
    /// Rows dropped because their repo slug could not be resolved.
    #[serde(default)]
    pub unresolved_rows: usize,
    /// Rows dropped by the [`MAX_ROWS`] cap.
    #[serde(default)]
    pub rows_truncated: usize,
    /// The tick's dispatch plan block (Issue #9288): slots, tick interval,
    /// shard posture, scope and key ordering. Absent from a pre-#9288 daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<DispatchPlanContext>,
}

/// `detail` survives only for dispositions whose detail is structured: the
/// park label, the open PR, a `labelled_blocked` row's allowlisted hold
/// labels (#8957), a `deferred_file_overlap` row's shared `## Affected Files`
/// paths (#9781; the daemon formats `file overlap: <paths>`, never forge free
/// text), and a `workspace_halted` row's closed-vocabulary hold
/// cause (#9017 — `main_red`, `gate_pending`, `token_pool`, …, emitted by
/// `work_finder::halt_cause`, never free-form text), and an `already_landed`
/// row's evidence (#11429: `forge_closed` or `journal_landed`).
#[must_use]
pub fn exportable_detail(disposition: QueueDisposition, detail: Option<&str>) -> Option<String> {
    match disposition {
        QueueDisposition::Parked
        | QueueDisposition::OpenPr
        | QueueDisposition::LabelledBlocked
        | QueueDisposition::DeferredFileOverlap
        | QueueDisposition::WorkspaceHalted
        | QueueDisposition::AlreadyLanded => detail.map(str::to_string),
        _ => None,
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip_serializing_if signature
fn is_zero(n: &usize) -> bool {
    *n == 0
}
