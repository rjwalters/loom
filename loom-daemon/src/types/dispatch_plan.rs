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

use crate::work_finder::step_item::Step;

/// Where a row stands in the plan.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum PlanState {
    /// A sweep is running for it (dispatched this tick, or already in flight).
    Running,
    /// Among the first `max_concurrent` capacity-deferred rows in plan
    /// order: what the next tick admits once slots free.
    Next,
    /// Deferred for a capacity-style reason, behind the `next` rows.
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
    /// The build back-off (#9410) held new issue builds on review/merge debt.
    BuildBackoff,
    /// Its affected files overlap same-repo work admitted or in flight (#9781).
    FileOverlap,
    /// A gate this client does not know (forward compatibility). This
    /// includes `out_of_slice`, retired with the dispatch slice (#11682), and
    /// `ramp`, `saturation` and `repo_cap`, retired with those caps
    /// (#12406): a historical record carrying one parses here.
    #[serde(other)]
    Unknown,
}

/// The step a plan row advances its item by (Issue #11910): which role
/// session the row is for, under step dispatch (#11894 §12).
///
/// The serialised names are [`Step`]'s own (`builder`, `judge`,
/// `champion_merge`, …), so a plan row and the classifier speak one
/// vocabulary. A name this build does not know — a newer daemon's step — is
/// kept verbatim in [`PlanStep::Unknown`] rather than failing the parse or
/// collapsing into another step: two different unknown steps stay two fold
/// keys in the fleet merge.
///
/// A row with **no** `step` field means `builder` (see [`RowPlan::step`]):
/// every plan published before step dispatch was a Builder plan, so older
/// plans and older readers keep working (an additive wire change).
///
/// A `step` that is not a string at all (a number, an object, …) is kept as
/// an unknown step named by its JSON text rather than rejecting the row, the
/// same tolerance [`PlanState`] gives a value it does not know (#11909).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(untagged)]
pub enum PlanStep {
    Known(Step),
    Unknown(String),
}

impl<'de> Deserialize<'de> for PlanStep {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(d)?;
        if let Ok(step) = serde_json::from_value::<Step>(value.clone()) {
            return Ok(PlanStep::Known(step));
        }
        Ok(PlanStep::Unknown(match value {
            serde_json::Value::String(name) => name,
            other => other.to_string(),
        }))
    }
}

impl Default for PlanStep {
    /// The step an older plan's row was always for.
    fn default() -> Self {
        PlanStep::Known(Step::Builder)
    }
}

impl PlanStep {
    /// The wire name — the fold key's third part in the fleet merge.
    #[must_use]
    pub fn name(&self) -> String {
        match self {
            PlanStep::Known(step) => serde_json::to_value(step)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
            PlanStep::Unknown(name) => name.clone(),
        }
    }
}

/// One comparator key that placed a row, in comparator order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanKey {
    pub name: String,
    pub value: serde_json::Value,
}

/// The per-row plan fields, flattened into `ReadyQueueRow` and
/// `QueueSnapshotRow` so they ride every existing queue surface.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RowPlan {
    /// 1-based position in the shaped pass-2 order. `None` ⇒ not dispatchable on this host this tick. Distinct from
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
    /// The shard that owns the workspace, when sharding is configured.
    #[serde(default)]
    pub owning_shard: Option<u32>,
    /// When a time-boxed hold clears (Issue #9311): the recheck interval,
    /// dispatch backoff (including the open-PR-guard-armed subset), no-op
    /// cooldown, decline cooldown, or PR-less-retry window. `None` for a
    /// `position`-bearing row (capacity deferrals
    /// name a gate, not a clock) and for every other `blocked` disposition
    /// this field does not (yet) trace — additive, never a substitute for
    /// `gate` or `plan_state`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_until: Option<DateTime<Utc>>,
    /// The step this row advances its item by (Issue #11910). Absent on the
    /// wire means `builder` — read it through [`RowPlan::step`], never
    /// directly. Not serialised when `None`, so a plan built without step
    /// dispatch is byte-for-byte what older readers already parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<PlanStep>,
}

impl RowPlan {
    /// The row's step, with an absent field read as `builder`.
    #[must_use]
    pub fn step(&self) -> PlanStep {
        self.step.clone().unwrap_or_default()
    }
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
    pub any_halted: bool,
    /// Whether the host's single overflow slot (#9244) is unused. Inert
    /// since #11508: nothing is admitted past the configured cap any more,
    /// so this only reports an overflow sweep an older daemon started.
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

    /// A non-string `step` is an unknown step, never a rejected row (#11909).
    #[test]
    fn a_non_string_step_parses_as_unknown() {
        let row: RowPlan = serde_json::from_value(serde_json::json!({ "step": 7 })).unwrap();
        assert_eq!(row.step(), PlanStep::Unknown("7".to_string()));
        let row: RowPlan =
            serde_json::from_value(serde_json::json!({ "step": { "x": 1 } })).unwrap();
        assert_eq!(row.step(), PlanStep::Unknown("{\"x\":1}".to_string()));
        let row: RowPlan = serde_json::from_value(serde_json::json!({ "step": "judge" })).unwrap();
        assert_eq!(row.step(), PlanStep::Known(Step::Judge));
        let row: RowPlan = serde_json::from_value(serde_json::json!({ "step": "scout" })).unwrap();
        assert_eq!(row.step(), PlanStep::Unknown("scout".to_string()));
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

    /// The dispatch slice's wire values are retired (#11682) but historical
    /// records and the shared merge fixture still carry them: `out_of_slice`
    /// parses as an unknown gate and a stray `in_slice` is ignored.
    #[test]
    fn retired_slice_values_still_parse() {
        let row: RowPlan = serde_json::from_value(serde_json::json!({
            "gate": "out_of_slice", "in_slice": false, "owning_shard": 1
        }))
        .unwrap();
        assert_eq!(row.gate, Some(PlanGate::Unknown));
        assert_eq!(row.owning_shard, Some(1));
        assert!(serde_json::to_value(&row)
            .unwrap()
            .get("in_slice")
            .is_none());
    }

    #[test]
    fn wire_names_are_snake_case() {
        assert_eq!(serde_json::to_value(PlanState::Next).unwrap(), "next");
        assert_eq!(serde_json::to_value(PlanGate::FileOverlap).unwrap(), "file_overlap");
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

    /// Issue #11910: a row with no `step` (an older plan) reads as
    /// `builder`, and is still serialised without one.
    #[test]
    fn a_row_without_a_step_reads_as_builder() {
        let row: RowPlan =
            serde_json::from_value(serde_json::json!({ "position": 1, "plan_state": "next" }))
                .unwrap();
        assert_eq!(row.step, None);
        assert_eq!(row.step(), PlanStep::Known(Step::Builder));
        assert_eq!(row.step().name(), "builder");
        let value = serde_json::to_value(&row).unwrap();
        assert!(value.get("step").is_none(), "absent, not null: {value}");
    }

    /// The `step` field uses the classifier's own names, and a step this
    /// build does not know is kept verbatim rather than failing the row.
    #[test]
    fn step_uses_the_classifier_names_and_keeps_unknown_ones() {
        let row: RowPlan =
            serde_json::from_value(serde_json::json!({ "step": "champion_merge" })).unwrap();
        assert_eq!(row.step(), PlanStep::Known(Step::ChampionMerge));
        assert_eq!(serde_json::to_value(&row).unwrap()["step"], "champion_merge");

        let newer: RowPlan =
            serde_json::from_value(serde_json::json!({ "step": "auditor_sweep" })).unwrap();
        assert_eq!(newer.step(), PlanStep::Unknown("auditor_sweep".into()));
        assert_eq!(newer.step().name(), "auditor_sweep");
        assert_eq!(serde_json::to_value(&newer).unwrap()["step"], "auditor_sweep");
    }
}
