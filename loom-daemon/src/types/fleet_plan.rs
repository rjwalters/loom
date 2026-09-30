//! Wire types for the **fleet** dispatch plan (Issue #9310): what the whole
//! fleet is working on, folded from the per-host plans Issue #9288 publishes.
//!
//! A host's [`RowPlan`](super::RowPlan) is host-local by construction — its
//! `position` is an index into *that* host's shaped pass-2 order, and two
//! hosts' positions are not comparable. [`HostPlan`] labels one host's rows
//! with the host they came from and the shard posture they were ranked
//! under; [`crate::work_finder::dispatch_plan_merge::merge_plans`] folds a
//! slice of them into a [`FleetPlan`].
//!
//! Nothing here ranks anything either: the merge is a pure fold over rows the
//! hosts already ranked. The rule itself is documented once, in
//! `defaults/docs/dispatch-plan.md`, and implemented once, in
//! `work_finder/dispatch_plan_merge.rs`; the dashboard's `workQueue.ts` is a
//! port of that one rule, pinned to the same JSON fixture.
//!
//! Every field is `#[serde(default)]` for the same reason the Phase A types
//! are: a payload from an older daemon still parses.

use serde::{Deserialize, Serialize};

use super::{PlanShard, PlanState, RowPlan};

/// One row of one host's plan, keyed by the fleet-stable `repo#issue`.
///
/// The plan fields are flattened beside `repo`/`issue`, exactly as
/// [`ReadyQueueRow`](super::ReadyQueueRow) flattens them, so a queue row and
/// a host-plan row are the same shape on the wire.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostPlanRow {
    /// Forge `owner/repo`. The fold key's first half.
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub issue: u32,
    #[serde(flatten)]
    pub plan: RowPlan,
}

/// One host's published dispatch plan: who ranked it, under what shard
/// posture, and the rows.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostPlan {
    /// The host id the fleet knows this daemon by. The last tie-break in
    /// every comparison below, so it must be unique across the slice.
    #[serde(default)]
    pub host_id: String,
    /// The shard posture the rows were ranked under — the `owning_shard`
    /// tie-break compares each row's owner against this host's own shard.
    #[serde(default)]
    pub shard: PlanShard,
    #[serde(default)]
    pub rows: Vec<HostPlanRow>,
}

/// One host's view of one fleet item.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FleetPlanObservation {
    #[serde(default)]
    pub host_id: String,
    #[serde(flatten)]
    pub plan: RowPlan,
}

/// One `repo#issue` as the fleet sees it: the observation that classifies it
/// plus every other host's view of the same issue.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FleetPlanItem {
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub issue: u32,
    /// The primary's `plan_state` — what the fleet says this issue is doing.
    #[serde(default)]
    pub plan_state: PlanState,
    /// The most advanced observation (see the merge rule).
    #[serde(default)]
    pub primary: FleetPlanObservation,
    /// Every other host's observation, best first by the same rule.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub others: Vec<FleetPlanObservation>,
}

/// The merged, ordered fleet plan.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FleetPlan {
    /// The contributing host ids, sorted — including hosts that contributed
    /// no rows, so an idle host is visibly present rather than missing.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// One item per `repo#issue`, in fleet order.
    #[serde(default)]
    pub items: Vec<FleetPlanItem>,
}

impl PlanState {
    /// Rank for "most advanced state wins": lower is more advanced. An
    /// [`PlanState::Unknown`] state (an older or newer daemon) ranks last, so
    /// it never displaces a state this build understands.
    #[must_use]
    pub fn advancement(self) -> u8 {
        match self {
            PlanState::Running => 0,
            PlanState::Next => 1,
            PlanState::Queued => 2,
            PlanState::Blocked => 3,
            PlanState::Unknown => 4,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_host_plan_row_flattens_its_plan_fields() {
        let row: HostPlanRow = serde_json::from_value(serde_json::json!({
            "repo": "acme/one", "issue": 7, "position": 3, "plan_state": "next"
        }))
        .unwrap();
        assert_eq!(row.issue, 7);
        assert_eq!(row.plan.position, Some(3));
        assert_eq!(row.plan.plan_state, PlanState::Next);
        let back = serde_json::to_value(&row).unwrap();
        assert_eq!(back["plan_state"], "next");
        assert!(back.get("plan").is_none(), "flattened, not nested: {back}");
    }

    #[test]
    fn advancement_ranks_running_first_and_unknown_last() {
        let mut states = [
            PlanState::Blocked,
            PlanState::Unknown,
            PlanState::Running,
            PlanState::Queued,
            PlanState::Next,
        ];
        states.sort_by_key(|s| s.advancement());
        assert_eq!(
            states,
            [
                PlanState::Running,
                PlanState::Next,
                PlanState::Queued,
                PlanState::Blocked,
                PlanState::Unknown
            ]
        );
    }

    #[test]
    fn an_empty_payload_defaults_every_field() {
        let plan: HostPlan = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(plan, HostPlan::default());
        let fleet: FleetPlan = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(fleet, FleetPlan::default());
    }
}
