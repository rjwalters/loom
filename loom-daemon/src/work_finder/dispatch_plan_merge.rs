//! `merge_plans` (Issue #9310): fold several hosts' dispatch plans into one
//! fleet plan.
//!
//! Phase A (#9288, `dispatch_plan.rs`) publishes *one host's* plan. Its
//! `position` is an index into that host's own shaped pass-2 order, so two
//! hosts' positions are not comparable — a fleet view that sorted on them
//! would interleave by an accident of how much work each host happens to
//! see. This module is the one place the fleet rule lives; the dashboard's
//! `dashboard/web/src/workQueue.ts` is a port of it, and both are pinned to
//! the same JSON fixture (`dashboard/test/fixtures/dispatch-plan-merge.json`)
//! so the two implementations cannot drift silently.
//!
//! # The rule
//!
//! **Fold** by `repo#issue`. Among the hosts that list the same issue, the
//! **primary** — the observation that classifies the item for the fleet — is
//! the one that wins, in order:
//!
//! 1. most advanced `plan_state` (`running` > `next` > `queued` > `blocked`,
//!    and an `unknown` state last);
//! 2. the host that **owns** the row's shard (its `PlanShard::host_shard`
//!    equals the row's `owning_shard`) — the host whose job it actually is;
//! 3. lower `position` (a row with none sorts last);
//! 4. lower host id.
//!
//! **Order** is `plan_state` rank first, then a round-robin interleave of the
//! hosts' own orders: within one state band each host's items are taken in
//! its own `position` order, and round *k* of every host precedes round
//! *k+1*, hosts within a round by host id. That gives every host equal
//! standing at the top of the fleet list without ever comparing two hosts'
//! positions as if they were one scale.
//!
//! # What is deliberately not compared
//!
//! `workspace_priority` is **per-host and unsynced** — it comes from each
//! host's own `.loom/config.json`, so host A's `priority: 100` and host B's
//! `priority: 100` are different repos' operators making unrelated
//! statements. Comparing them across hosts would let one host's local
//! numbering reorder another's queue. It is not a merge input, here or in the
//! dashboard port. Within a host it already shaped `position`, which is what
//! the interleave consumes.
//!
//! Wall-clock inputs (`created_at`, observation age) are not merge inputs
//! either: the hosts already applied them inside their own comparators.

use std::collections::HashMap;

use crate::types::{FleetPlan, FleetPlanItem, FleetPlanObservation, HostPlan, RowPlan};

/// Fold `hosts` into one fleet plan. Pure, total, and deterministic: the
/// output depends only on the input slice, and never on its order (host id
/// is the last tie-break everywhere).
///
/// A single-host slice is a pass-through: every row becomes one item with no
/// `others`, in that host's own plan order.
#[must_use]
pub fn merge_plans(hosts: &[HostPlan]) -> FleetPlan {
    let mut host_ids: Vec<String> = hosts.iter().map(|h| h.host_id.clone()).collect();
    host_ids.sort();
    host_ids.dedup();
    FleetPlan {
        hosts: host_ids,
        items: order_fleet(fold_by_issue(hosts)),
    }
}

/// One host's view of one row, plus the shard posture that host ranked under
/// (which [`Candidate::owns`] needs and [`FleetPlanObservation`] does not
/// carry — it is a property of the host, not of the row).
#[derive(Debug, Clone)]
struct Candidate {
    host_id: String,
    host_shard: Option<u32>,
    plan: RowPlan,
}

impl Candidate {
    /// Whether this host is the one the row's workspace shards to. Unsharded
    /// hosts (`host_shard: None`) never own anything, so an unsharded fleet
    /// falls straight through to the `position` tie-break.
    fn owns(&self) -> bool {
        self.host_shard.is_some() && self.host_shard == self.plan.owning_shard
    }

    /// A row with no position sorts behind every row that has one.
    fn position(&self) -> u32 {
        self.plan.position.unwrap_or(u32::MAX)
    }

    /// The primary rule, as an ordering: least is best.
    fn cmp_primary(&self, other: &Self) -> std::cmp::Ordering {
        self.plan
            .plan_state
            .advancement()
            .cmp(&other.plan.plan_state.advancement())
            // `true` first, so the owning host wins the tie.
            .then_with(|| other.owns().cmp(&self.owns()))
            .then_with(|| self.position().cmp(&other.position()))
            .then_with(|| self.host_id.cmp(&other.host_id))
    }
}

/// Fold every host's rows by `repo#issue`, picking each item's primary.
/// Returned in first-seen order; [`order_fleet`] does the ordering.
fn fold_by_issue(hosts: &[HostPlan]) -> Vec<FleetPlanItem> {
    let mut seen: Vec<(String, u32)> = Vec::new();
    let mut folded: HashMap<(String, u32), Vec<Candidate>> = HashMap::new();
    for host in hosts {
        for row in &host.rows {
            let key = (row.repo.clone(), row.issue);
            folded
                .entry(key.clone())
                .or_insert_with(|| {
                    seen.push(key);
                    Vec::new()
                })
                .push(Candidate {
                    host_id: host.host_id.clone(),
                    host_shard: host.shard.host_shard,
                    plan: row.plan.clone(),
                });
        }
    }
    seen.into_iter()
        .map(|key| {
            let mut candidates = folded.remove(&key).unwrap_or_default();
            candidates.sort_by(Candidate::cmp_primary);
            let mut observations = candidates.into_iter().map(|c| FleetPlanObservation {
                host_id: c.host_id,
                plan: c.plan,
            });
            let primary = observations.next().unwrap_or_default();
            FleetPlanItem {
                repo: key.0,
                issue: key.1,
                plan_state: primary.plan.plan_state,
                primary,
                others: observations.collect(),
            }
        })
        .collect()
}

/// The fleet sort key: state band, then the round-robin round, then the
/// within-host order the round was assigned from.
#[derive(Debug, Clone)]
struct FleetKey {
    state: u8,
    round: usize,
    host_id: String,
    position: u32,
    repo: String,
    issue: u32,
}

impl FleetKey {
    fn of(item: &FleetPlanItem) -> Self {
        FleetKey {
            state: item.plan_state.advancement(),
            round: 0,
            host_id: item.primary.host_id.clone(),
            position: item.primary.plan.position.unwrap_or(u32::MAX),
            repo: item.repo.clone(),
            issue: item.issue,
        }
    }

    /// One host's own order within a state band — what the rounds are handed
    /// out along. `repo`/`issue` only break a tie between two rows of the
    /// same host with the same position, which a well-formed plan does not
    /// contain; they are here so the result is total regardless.
    fn cmp_within_host(&self, other: &Self) -> std::cmp::Ordering {
        self.state
            .cmp(&other.state)
            .then_with(|| self.host_id.cmp(&other.host_id))
            .then_with(|| self.position.cmp(&other.position))
            .then_with(|| self.repo.cmp(&other.repo))
            .then_with(|| self.issue.cmp(&other.issue))
    }

    fn cmp_fleet(&self, other: &Self) -> std::cmp::Ordering {
        self.state
            .cmp(&other.state)
            .then_with(|| self.round.cmp(&other.round))
            .then_with(|| self.host_id.cmp(&other.host_id))
            .then_with(|| self.position.cmp(&other.position))
            .then_with(|| self.repo.cmp(&other.repo))
            .then_with(|| self.issue.cmp(&other.issue))
    }
}

/// Sort `items` into fleet order: `plan_state` rank, then the round-robin
/// interleave of each host's own plan order.
fn order_fleet(items: Vec<FleetPlanItem>) -> Vec<FleetPlanItem> {
    let mut keyed: Vec<(FleetKey, FleetPlanItem)> = items
        .into_iter()
        .map(|item| (FleetKey::of(&item), item))
        .collect();
    keyed.sort_by(|a, b| a.0.cmp_within_host(&b.0));
    let mut next_round: HashMap<(u8, String), usize> = HashMap::new();
    for (key, _) in &mut keyed {
        let round = next_round
            .entry((key.state, key.host_id.clone()))
            .or_insert(0);
        key.round = *round;
        *round += 1;
    }
    keyed.sort_by(|a, b| a.0.cmp_fleet(&b.0));
    keyed.into_iter().map(|(_, item)| item).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "dispatch_plan_merge_tests.rs"]
mod tests;
