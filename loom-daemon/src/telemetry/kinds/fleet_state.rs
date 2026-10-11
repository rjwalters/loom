//! `fleet.state` (Issue #10196): one host's own view of the queue and of the
//! work it holds, exported to SigNoz as OTLP logs so the fleet can be rebuilt
//! at any instant `t` from SigNoz alone.
//!
//! # Why a new kind
//!
//! `queue.snapshot` and `eta.snapshot` are native-HTTPS only (dashboard state
//! keys), and neither has an entered-at, host or slot field. This kind is the
//! slim state record a replay reader (`defaults/docs/telemetry-replay.md`)
//! needs: one row per `(repo, issue)` this host can see (the sweeps it holds,
//! the PRs under a Loom review label, its ready queue with each item's planner
//! rank and ranking inputs) plus a per-repo open-PR census.
//!
//! # Every host emits its own view
//!
//! Nothing is elected. Each daemon reports what it holds and what it sees;
//! two hosts reporting the same PR in review are two true views, and readers
//! reconcile (`telemetry-replay.md`). The record does not depend on the ETA
//! subsystem: it is emitted whenever an OTLP exporter exists.
//!
//! # Anchors, deltas and chunks
//!
//! The emitter (`observability::fleet_state`) runs after every work-finder
//! tick (default 60 s, `autonomous.workFinder.intervalSecs`) and on the
//! collector's 5-minute snapshot pass; `tick_interval_secs` names the
//! cadence. It sends a **full anchor** (`anchor: true`, every row) on its
//! first pass, whenever the planner stamps change, and at least every
//! [`ANCHOR_INTERVAL_SECS`]. Between anchors it sends a **delta**
//! (`anchor: false`) only when something changed: the changed or added rows,
//! the issues that left (`removed`), and the full census of each repo it names.
//! A repo flagged `ready_replace` carries its whole `ready_wait` set instead
//! of a ready diff ([`FleetStateRepo::ready_replace`]).
//!
//! There is **no row cap**. A record whose JSON would exceed
//! [`CHUNK_BYTES`] is split by [`split_into_chunks`] into several records that
//! share `as_of` and carry `chunk_index` / `chunk_count`; nothing is dropped.
//! The chunk size exists only because one OTLP/gRPC message is limited to
//! about 4 MB. A reader treats a record as complete only when it has all
//! `chunk_count` chunks.
//!
//! # Regime stamps
//!
//! Every record carries `planner_version`, `planner_config_hash` and, when the
//! host reads a fleet store, `fleet_config_hash`. A change in any of them is a
//! regime boundary, and the emitter starts a new anchor there.
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

use crate::telemetry::{RepoVisibility, SweepStartFacts};

/// Schema tag carried in every record.
pub const FLEET_STATE_SCHEMA: &str = "fleet-state/v1";

/// The longest gap between two full anchors, in seconds. A reader never has
/// to look back further than this, plus one pass interval, to find a
/// reconstruction base, so a lost delta is wrong for at most this long.
pub const ANCHOR_INTERVAL_SECS: i64 = 300;

/// The most bytes of JSON one record carries before it is split into chunks
/// (~1 MB, a quarter of the ~4 MB OTLP/gRPC message limit). A row is ~100-180
/// bytes, so a 3000-row anchor is one record; chunking engages only when the
/// queue grows several-fold, and never drops a row.
pub const CHUNK_BYTES: usize = 1_000_000;

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
    "loom.fleet.chunk_index",
    "loom.fleet.chunk_count",
];

/// The stage an item is in, as `fleet.state` reports it. Neutral (not tied to
/// any estimator); the wire strings are the stage names the replay contract
/// has always used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum FleetStage {
    /// Ready (`loom:issue`), in this host's dispatch plan.
    #[serde(rename = "ready_wait")]
    ReadyWait,
    /// A sweep on this host, from dispatch until its Curator phase completes.
    #[serde(rename = "sweep.curator")]
    SweepCurator,
    /// A sweep on this host, Curator done, Builder not yet done.
    #[serde(rename = "sweep.builder")]
    SweepBuilder,
    /// A PR waiting for, or receiving, a Judge verdict.
    #[serde(rename = "review_wait")]
    ReviewWait,
    /// Doctor rework after `loom:changes-requested`.
    #[serde(rename = "doctor")]
    Doctor,
    /// Approved (`loom:pr`), waiting to merge.
    #[serde(rename = "merge_wait")]
    MergeWait,
    /// Approved, held for a human by an operator hold label.
    #[serde(rename = "merge_hold")]
    MergeHold,
    /// Pre-ready: from the issue's creation (or latest reopen) until
    /// `loom:curated`, or `loom:issue` for a one-step promotion (#11368).
    /// Only ever an `eta.stage_outcome` stage; never a `fleet.state` row.
    #[serde(rename = "triage_wait")]
    TriageWait,
    /// Pre-ready: from `loom:curated` until `loom:issue` (#11368). Only ever
    /// an `eta.stage_outcome` stage; never a `fleet.state` row.
    #[serde(rename = "approval_wait")]
    ApprovalWait,
}

impl FleetStage {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FleetStage::ReadyWait => "ready_wait",
            FleetStage::SweepCurator => "sweep.curator",
            FleetStage::SweepBuilder => "sweep.builder",
            FleetStage::ReviewWait => "review_wait",
            FleetStage::Doctor => "doctor",
            FleetStage::MergeWait => "merge_wait",
            FleetStage::MergeHold => "merge_hold",
            FleetStage::TriageWait => "triage_wait",
            FleetStage::ApprovalWait => "approval_wait",
        }
    }
}

/// Which dispatch slot a sweep holds on its host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetSlot {
    /// One of the host's `maxConcurrent` slots.
    Regular,
    /// The host's single `loom:operator-priority` overflow slot (#9244).
    Overflow,
}

/// Why a held item is held, as `fleet.state` reports it. Neutral (not tied to
/// the ETA subsystem); the wire strings are the long-standing snake_case hold
/// names, so a reader can join them with historical hold data. All eight
/// values are carried even when this host's emitter can only tell some of
/// them from labels (the marker-refined `merge_risk` / `critical_file` /
/// `ac_hold` split is left to the reader).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FleetHoldKind {
    /// `loom:operator`.
    Operator,
    /// `loom:operator-only` (or `-mechanical`).
    OperatorOnly,
    /// `loom:operator-decision`.
    OperatorDecision,
    /// Champion's merge-risk hold.
    MergeRisk,
    /// Champion's critical-file hold.
    CriticalFile,
    /// Champion's acceptance-criteria hold.
    AcHold,
    /// `loom:blocked` alone.
    Blocked,
    /// A hold label none of the above names.
    Other,
}

impl FleetHoldKind {
    /// Every kind.
    pub const ALL: [FleetHoldKind; 8] = [
        FleetHoldKind::Operator,
        FleetHoldKind::OperatorOnly,
        FleetHoldKind::OperatorDecision,
        FleetHoldKind::MergeRisk,
        FleetHoldKind::CriticalFile,
        FleetHoldKind::AcHold,
        FleetHoldKind::Blocked,
        FleetHoldKind::Other,
    ];

    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FleetHoldKind::Operator => "operator",
            FleetHoldKind::OperatorOnly => "operator_only",
            FleetHoldKind::OperatorDecision => "operator_decision",
            FleetHoldKind::MergeRisk => "merge_risk",
            FleetHoldKind::CriticalFile => "critical_file",
            FleetHoldKind::AcHold => "ac_hold",
            FleetHoldKind::Blocked => "blocked",
            FleetHoldKind::Other => "other",
        }
    }
}

/// A repo's `main` CI status as this host's main-health gate last saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MainCi {
    /// The gate evaluated `main` and it is green.
    Green,
    /// The gate verified `main` red (dispatch halted).
    Red,
    /// The gate has not evaluated `main`, or could not.
    Unknown,
}

fn is_zero(n: &u8) -> bool {
    *n == 0
}

/// One `(repo, issue)` as this host sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetStateRow {
    /// Issue number.
    pub issue: u32,
    /// The stage the item is in.
    pub stage: FleetStage,
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
    /// runs it. An item seen only through a listing has no known host, so the
    /// field is absent rather than guessed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The dispatch slot the sweep holds. Present exactly when `host` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<FleetSlot>,
    /// A sweep row only: its model, effort, runtime and attempt lineage, the
    /// values its `sweep.started` carried (#11280). Flattened, so the keys
    /// sit beside `host` / `slot` under `sweep.outcome`'s names. Absent for a
    /// sweep this daemon did not dispatch (adopted after a restart). Additive
    /// on `fleet-state/v1`.
    #[serde(flatten)]
    pub start: SweepStartFacts,
    /// `ready_wait` only: this host's planner rank, the 1-based position in
    /// the work finder's dispatch order on its last tick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<u32>,
    /// `ready_wait` only: starred (`loom:operator-priority`, any level).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub star: bool,
    /// `ready_wait` only: when it was starred, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub star_at: Option<DateTime<Utc>>,
    /// `ready_wait` only: the effective operator priority level (0 absent).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub level: u8,
    /// `ready_wait` only: the repo's fleet priority (the workspace priority
    /// tier; lower dispatches first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fleet_priority: Option<u32>,
    /// `ready_wait` only: the issue's creation instant, the planner's age
    /// input. Age at a record is `as_of - created_at`; the instant is sent
    /// instead of the age so an unchanged row stays unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    /// `ready_wait` only: a red-main fix the planner boosted this tick.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub main_red_fix: bool,
    /// The item is under a hold (operator or `loom:blocked`) now. Absent when
    /// it is not. Additive on `fleet-state/v1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_kind: Option<FleetHoldKind>,
    /// When the current hold began. Present exactly when `hold_kind` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_since: Option<DateTime<Utc>>,
    /// `held_since` is a lower bound (first seen held), not an observed
    /// transition. Absent when exact.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub held_since_lower_bound: bool,
    /// The hold was released; the instant this host first saw it clear. Set
    /// on the row that clears the hold, with `hold_kind` and `held_since`
    /// absent, and kept while the row is otherwise unchanged. A row that
    /// leaves while held gets none: its removal is the end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_released_at: Option<DateTime<Utc>>,
}

impl FleetStateRow {
    /// A row with only the stage facts set.
    #[must_use]
    pub fn new(issue: u32, stage: FleetStage, entered_at: DateTime<Utc>) -> Self {
        FleetStateRow {
            issue,
            stage,
            entered_at,
            entered_at_lower_bound: false,
            pr: None,
            host: None,
            slot: None,
            start: SweepStartFacts::default(),
            rank: None,
            star: false,
            star_at: None,
            level: 0,
            fleet_priority: None,
            created_at: None,
            main_red_fix: false,
            hold_kind: None,
            held_since: None,
            held_since_lower_bound: false,
            hold_released_at: None,
        }
    }
}

/// One repo's open-PR census from this host's review listings.
///
/// It counts the open PRs under a Loom review label (`loom:review-requested`,
/// `loom:changes-requested`, `loom:pr`). An open PR carrying none of them is
/// not counted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetPrCensus {
    /// Distinct open PRs under a review label.
    pub open: u32,
    /// `open` by stage: `review_wait`, `doctor`, `merge_wait`, `merge_hold`,
    /// and `held` for a PR whose labels name no single stage.
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
    /// This repo's `main` CI status from the main-health gate. Absent when
    /// the host did not read it (an older emitter). Additive on `fleet-state/v1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main_ci: Option<MainCi>,
    /// The census. Absent means unknown at `as_of`, because the repo's
    /// listings were incomplete or not read. It never means zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub census: Option<FleetPrCensus>,
    /// The work finder's last tick walked this repo's ready listing to its
    /// last page (#11139), so its `ready_wait` rows are the whole queue. A
    /// reader must not treat a `false` repo's `ready_wait` rows as its whole
    /// queue. Always sent; missing (an older emitter) decodes to `false`.
    #[serde(default)]
    pub ready_complete: bool,
    /// `rows` carries this repo's **entire** `ready_wait` set, not a diff:
    /// a reader first drops every `ready_wait` row it holds for the repo,
    /// then applies `removed` and `rows`. It never infers a ready removal
    /// from absence otherwise, and `removed` names no `ready_wait` row of
    /// the repo. Set exactly when `ready_complete` is `false`; on a chunked
    /// record the drop happens once, before any chunk's rows. Missing (an
    /// older emitter) decodes to `false`: plain diffing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ready_replace: bool,
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

/// Host-level capacity beside [`FleetSlots`] (which it never repeats). Every
/// field is a discrete fact: a continuous utilisation fraction would turn
/// every pass into a delta, so none is carried.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetCapacity {
    /// Sweeps this host runs now.
    #[serde(default)]
    pub live_workers: u32,
    /// Token accounts the rotation ranking calls healthy. Absent when the
    /// ranking is unreadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accounts_usable: Option<u32>,
    /// Token accounts the ranking calls exhausted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accounts_exhausted: Option<u32>,
    /// Host-load breaker phase (`closed`, `open`, `cooldown`). Absent when
    /// none is registered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_breaker: Option<String>,
    /// Forge rate-limit breaker phase. Absent when none is registered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_breaker: Option<String>,
    /// The admission brake is holding new dispatch. Absent when none is
    /// registered or it is disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_brake_held: Option<bool>,
}

/// The planner regime a record was observed under.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannerStamps {
    /// The daemon version whose planner produced the ranks.
    #[serde(default)]
    pub planner_version: String,
    /// 12 hex of sha256 over the canonical JSON of the planner-relevant
    /// effective config (`autonomous.workFinder`, `autonomous.mergeSequencing`).
    #[serde(default)]
    pub planner_config_hash: String,
    /// The fleet store commit this host's last config pass resolved. Absent
    /// when the host reads no fleet store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fleet_config_hash: Option<String>,
}

fn one() -> u32 {
    1
}

/// `fleet.state`: one host's view of its held work, review PRs and queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetStateRecord {
    /// Always [`FLEET_STATE_SCHEMA`].
    pub schema: String,
    /// The instant the state describes. Shared by every chunk of a record.
    pub as_of: DateTime<Utc>,
    /// `true` for a full anchor, `false` for a delta.
    pub anchor: bool,
    /// The `as_of` of the anchor this record belongs to. Equal to `as_of` on
    /// an anchor.
    pub anchor_as_of: DateTime<Utc>,
    /// Delta only: the `as_of` of the record this delta applies on top of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_as_of: Option<DateTime<Utc>>,
    /// 0-based position of this chunk among the record's chunks.
    #[serde(default)]
    pub chunk_index: u32,
    /// How many chunks the record at `as_of` was split into (1 = unsplit).
    #[serde(default = "one")]
    pub chunk_count: u32,
    /// The planner regime stamps.
    #[serde(flatten)]
    pub stamps: PlannerStamps,
    /// The seconds between the emitter's passes, the sampling resolution: the
    /// work finder's tick interval, or 300 when it has not ticked. Absent
    /// from an older emitter, which sampled every 300 s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick_interval_secs: Option<u64>,
    /// When the review listings were read. Absent when none completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub census_at: Option<DateTime<Utc>>,
    /// Slot use from the last dispatch plan, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slots: Option<FleetSlots>,
    /// Host capacity beside `slots`, when read. Additive on `fleet-state/v1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity: Option<FleetCapacity>,
    /// On an anchor, every repo with rows or a census. On a delta, only the
    /// repos that changed. Ordered by repo. A chunk carries a slice of them;
    /// one repo's rows may span chunks.
    #[serde(default)]
    pub repos: Vec<FleetStateRepo>,
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

fn json_len<T: Serialize>(value: &T) -> usize {
    serde_json::to_vec(value).map_or(0, |v| v.len())
}

/// Split `record` into chunks whose JSON is at most `max_bytes` each (one
/// chunk, unchanged but for `chunk_index: 0, chunk_count: 1`, when it fits).
/// Every chunk shares the header (`as_of`, `anchor`, stamps, `slots`, ...);
/// the repos' rows and `removed` entries are packed in order, so the union of
/// the chunks is exactly `record`: nothing is dropped. A repo whose entries
/// span chunks repeats its `repo`, `visibility`, `census`, `ready_complete`
/// and `ready_replace` in each. Pure.
#[must_use]
pub fn split_into_chunks(mut record: FleetStateRecord, max_bytes: usize) -> Vec<FleetStateRecord> {
    record.chunk_index = 0;
    record.chunk_count = 1;
    if json_len(&record) <= max_bytes {
        return vec![record];
    }
    let repos = std::mem::take(&mut record.repos);
    // Header plus room for multi-digit chunk numbers.
    let budget = max_bytes.saturating_sub(json_len(&record) + 32);
    let mut chunks: Vec<Vec<FleetStateRepo>> = Vec::new();
    let mut current: Vec<FleetStateRepo> = Vec::new();
    let mut used = 0;
    for repo in repos {
        let shell = FleetStateRepo {
            rows: Vec::new(),
            removed: Vec::new(),
            ..repo.clone()
        };
        // `,"rows":[]` and `,"removed":[]` plus the separating comma.
        let shell_len = json_len(&shell) + 24;
        let mut piece: Option<FleetStateRepo> = None;
        let units = repo
            .rows
            .into_iter()
            .map(Ok)
            .chain(repo.removed.into_iter().map(Err));
        for unit in units {
            let unit_len = match &unit {
                Ok(row) => json_len(row),
                Err(issue) => json_len(issue),
            } + 1;
            let opening = if piece.is_none() { shell_len } else { 0 };
            if used + opening + unit_len > budget && (piece.is_some() || !current.is_empty()) {
                current.extend(piece.take());
                chunks.push(std::mem::take(&mut current));
                used = 0;
            }
            let entry = piece.get_or_insert_with(|| {
                used += shell_len;
                shell.clone()
            });
            match unit {
                Ok(row) => entry.rows.push(row),
                Err(issue) => entry.removed.push(issue),
            }
            used += unit_len;
        }
        // A repo with neither rows nor removals (a census-only entry).
        let piece = piece.unwrap_or_else(|| {
            if used + shell_len > budget && !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
                used = 0;
            }
            used += shell_len;
            shell
        });
        current.push(piece);
    }
    if !current.is_empty() || chunks.is_empty() {
        chunks.push(current);
    }
    let count = u32::try_from(chunks.len()).unwrap_or(u32::MAX);
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, repos)| FleetStateRecord {
            chunk_index: u32::try_from(i).unwrap_or(u32::MAX),
            chunk_count: count,
            repos,
            ..record.clone()
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "fleet_state_tests.rs"]
mod tests;
