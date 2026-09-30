//! Wire types for the dispatch plan (Issue #9288): the order Loom will work
//! in, as a projection of the work finder's real tick.
//!
//! Nothing here ranks anything. `crate::work_finder::dispatch_plan::annotate`
//! reads the `TickReport` the tick produced — including the shaped pass-2
//! order the tick actually iterated — and fills these fields in. Every field
//! is `#[serde(default)]`, so a payload from an older daemon (or an older
//! client reading a newer one) still parses; this is an additive change with
//! no schema-version bump.
//!
//! Scope: the plan covers `loom:issue` (the work finder's listing) plus the
//! forge-side `loom:blocked` rows `queue.snapshot` appends. `loom:curated` and
//! `loom:triage` have **no dispatcher order** and are never listed —
//! [`DispatchPlanContext::scope`] names what the plan covers.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Where a row stands in the plan.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PlanState {
    /// A sweep is running for it (dispatched this tick, or already in flight).
    Running,
    /// Among the first `max_admissions_per_tick` capacity-deferred rows in
    /// plan order: what the next tick admits once slots free.
    Next,
    /// Deferred for a capacity-style reason, behind the `next` rows
    /// (including out-of-slice and repo-capped rows).
    Queued,
    /// Held by something specific to the issue, its repo, or its history.
    Blocked,
    /// A payload without the field (an older daemon), or a newer state.
    #[default]
    #[serde(other)]
    Unknown,
}

/// Which admission gate holds a deferred row.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PlanGate {
    /// The shared concurrency cap was full.
    Capacity,
    /// The per-tick admission ramp cap was reached.
    Ramp,
    /// The saturation admission brake held new admissions.
    Saturation,
    /// The build back-off (#9410) held new issue builds on review/merge debt.
    BuildBackoff,
    /// Its own repo was at `maxConcurrentPerRepo`.
    RepoCap,
    /// Outside this host's preferred repo slice while the slice had work.
    OutOfSlice,
    /// A gate this client does not know (forward compatibility).
    #[serde(other)]
    Unknown,
}

/// One comparator key that placed a row, in comparator order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanKey {
    pub name: String,
    pub value: serde_json::Value,
}

/// A workspace's per-repo cap and its top-of-tick occupancy.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoCapView {
    /// `maxConcurrentPerRepo`, or `None` when uncapped.
    #[serde(default)]
    pub cap: Option<usize>,
    /// Live sweeps in the workspace at the top of the tick.
    #[serde(default)]
    pub occupancy: usize,
}

/// The per-row plan fields, flattened into `ReadyQueueRow` and
/// `QueueSnapshotRow` so they ride every existing queue surface.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RowPlan {
    /// 1-based position in the shaped pass-2 order (out-of-slice rows follow
    /// it). `None` ⇒ not dispatchable on this host this tick. Distinct from
    /// `rank`, which is the bare comparator rank over every row.
    #[serde(default)]
    pub position: Option<u32>,
    #[serde(default)]
    pub plan_state: PlanState,
    /// The comparator keys, in comparator order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<PlanKey>,
    #[serde(default)]
    pub gate: Option<PlanGate>,
    /// Whether the workspace is in this host's preferred repo slice.
    #[serde(default)]
    pub in_slice: Option<bool>,
    /// Whether the workspace had a live sweep at the top of the tick (the
    /// track-affinity input).
    #[serde(default)]
    pub hot: Option<bool>,
    /// The shard that owns the workspace, when sharding is configured.
    #[serde(default)]
    pub owning_shard: Option<u32>,
    #[serde(default)]
    pub repo_cap: Option<RepoCapView>,
    /// When a time-boxed hold clears (Issue #9311): the recheck interval,
    /// dispatch backoff (including the open-PR-guard-armed subset), no-op
    /// cooldown, decline cooldown, or PR-less-retry window. `None` for a
    /// `position`-bearing row (capacity/ramp/repo-cap/out-of-slice deferrals
    /// name a gate, not a clock) and for every other `blocked` disposition
    /// this field does not (yet) trace — additive, never a substitute for
    /// `gate` or `plan_state`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_until: Option<DateTime<Utc>>,
}

/// Admission capacity as of the tick.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanSlots {
    #[serde(default)]
    pub max_concurrent: usize,
    /// Occupied slots when the tick finished.
    #[serde(default)]
    pub occupancy: Option<usize>,
    /// `max_concurrent - occupancy`, saturating.
    #[serde(default)]
    pub free: Option<usize>,
    #[serde(default)]
    pub max_admissions_per_tick: Option<usize>,
    #[serde(default)]
    pub saturation_held: bool,
    #[serde(default)]
    pub any_halted: bool,
    /// Whether the host's single `loom:operator-priority` overflow slot
    /// (#9244) is unused: a starred issue can still start past the
    /// configured cap.
    #[serde(default)]
    pub overflow_free: Option<bool>,
}

/// This host's shard posture.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanShard {
    /// `false` when unsharded (this host's slice is every workspace).
    #[serde(default)]
    pub configured: bool,
    #[serde(default)]
    pub host_shard: Option<u32>,
    #[serde(default)]
    pub shard_count: Option<u32>,
}

/// The per-tick plan block.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DispatchPlanContext {
    #[serde(default)]
    pub slots: PlanSlots,
    #[serde(default)]
    pub tick_interval_secs: Option<u64>,
    #[serde(default)]
    pub shard: PlanShard,
    /// The labels the plan covers. `loom:curated` / `loom:triage` are
    /// unordered and never appear.
    #[serde(default)]
    pub scope: Vec<String>,
    /// The comparator key names, in order.
    #[serde(default)]
    pub ordering: Vec<String>,
    /// `false` when some repo's listing failed, so the plan is missing its
    /// backlog.
    #[serde(default)]
    pub complete: bool,
}

/// What [`DispatchPlanContext::scope`] covers.
pub const PLAN_SCOPE: [&str; 2] = ["loom:issue", "loom:blocked"];

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_payload_defaults_every_field() {
        let row: RowPlan = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(row, RowPlan::default());
        assert_eq!(row.plan_state, PlanState::Unknown);
        let ctx: DispatchPlanContext = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(ctx, DispatchPlanContext::default());
    }

    #[test]
    fn unknown_enum_values_parse_as_unknown() {
        let row: RowPlan = serde_json::from_value(serde_json::json!({
            "plan_state": "someday", "gate": "moon_phase"
        }))
        .unwrap();
        assert_eq!(row.plan_state, PlanState::Unknown);
        assert_eq!(row.gate, Some(PlanGate::Unknown));
    }

    #[test]
    fn wire_names_are_snake_case() {
        assert_eq!(serde_json::to_value(PlanState::Next).unwrap(), "next");
        assert_eq!(serde_json::to_value(PlanGate::RepoCap).unwrap(), "repo_cap");
        assert_eq!(serde_json::to_value(PlanGate::OutOfSlice).unwrap(), "out_of_slice");
    }

    /// `held_until` (Issue #9311) round-trips as RFC-3339 when present, and
    /// is simply absent from the serialized payload when `None` — the same
    /// `#[serde(default)]` convention as every other `RowPlan` field, so a
    /// plan with no held item is byte-identical to pre-#9311 output.
    #[test]
    fn held_until_round_trips_and_is_absent_when_none() {
        let at = "2026-09-29T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let held = RowPlan {
            held_until: Some(at),
            ..RowPlan::default()
        };
        let value = serde_json::to_value(&held).unwrap();
        assert_eq!(value["held_until"], "2026-09-29T12:00:00Z");
        let round_tripped: RowPlan = serde_json::from_value(value).unwrap();
        assert_eq!(round_tripped.held_until, Some(at));

        let unheld = RowPlan::default();
        let value = serde_json::to_value(&unheld).unwrap();
        assert!(
            value.get("held_until").is_none(),
            "held_until must be absent when None, not null: {value:?}"
        );
        let round_tripped: RowPlan = serde_json::from_value(value).unwrap();
        assert_eq!(round_tripped.held_until, None);
    }
}
