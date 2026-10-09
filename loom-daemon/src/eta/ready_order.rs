//! Where every ready row of a dispatch plan stands for the ETA (#10903).
//!
//! The work finder's plan (#9288) gives a position only to rows **this host**
//! would admit (`next` / `queued`). Every other row is `blocked` with no
//! position, and before #10903 the tracker refused all of them
//! `no_dispatch_plan`. That was right for a host estimating its own work. It
//! is wrong for the single ETA authority (#10498), which estimates for the
//! whole fleet: a row this host cannot dispatch (another host's constraint, a
//! peer's claim, a workspace this host cannot run sweeps in, a host-local
//! halt) is still dispatched by the fleet, so it gets an estimate, with the
//! reason recorded as `not_here` instead of as a refusal.
//!
//! Pure: [`placements`] reads the rows of one tick and nothing else.
//!
//! # Classes
//!
//! - **Running** (`plan_state: running`): the sweep, not the plan, estimates it.
//! - **Waiting** (`next` / `queued` with a position): estimated from its own
//!   plan row exactly as before (#9326). Nothing here changes its inputs.
//! - **Not here**: `host_constraint`, `host_class_refused`, `peer_claim`,
//!   `workspace_commands_missing`, `dispatch_error`, and `workspace_halted`
//!   for a host-local cause (`gate_pending`, `token_pool`,
//!   `preflight_advisory`, `drain`, `breaker`, `write_scope`). A red `main`
//!   (`main_red`, `ci_billing`) halts every host, so it stays refused.
//! - **Time-held**: `recheck_interval`, `dispatch_backoff`, `noop_cooldown` and
//!   `prless_retry` with a `held_until` (#9311). Placed like a not-here row,
//!   and it carries the expiry. Without an expiry it stays refused.
//! - **Held**: a park label (`parked`, `hard_exclusion`, `labelled_blocked`)
//!   is refused `blocked`, the reason a hold label gets everywhere else.
//!   Everything else (`quarantined`, `open_pr`, `open_pr_backoff`, `declined`,
//!   an unknown disposition, a deferral with no position) stays
//!   `no_dispatch_plan`.
//!
//! # Placement
//!
//! A not-here or time-held row is slotted into the planner's order by the
//! work finder's own comparator rank (`rank`, the bare `candidate_cmp`
//! order): `position` is one past the highest planner position among the
//! **waiting** rows ranked before it (the position it would take), and `ahead`
//! is the number of waiting rows at or before that slot, so the two always
//! agree and a placed row is never estimated ahead of a waiting row ranked
//! before it. Only waiting rows count as ahead, because only
//! they compete for this host's slots, which are what `start-v1`'s turnover
//! draws model. That is why waiting rows' inputs are unchanged. Fleet
//! capacity is #10944.

use crate::eta::tracker::{waiting_position, ReadyRow};
use crate::eta::NoEstimateReason;
use crate::types::{PlanState, QueueDisposition as Qd};
use crate::work_finder::halt_cause::HaltCause;
use chrono::{DateTime, Utc};

/// How the ETA reads one ready row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// Dispatched or in flight: not the plan's to estimate.
    Running,
    /// The planner positioned it: estimated from its own plan row.
    Waiting,
    /// This host cannot dispatch it now; the fleet can.
    Placed(Placed),
    /// No estimate, and why.
    Refused(NoEstimateReason),
}

/// A row placed by comparator rank.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    /// One past the highest planner position among the waiting rows ranked
    /// before it (1 when there are none).
    pub position: u32,
    /// Waiting rows ranked before it.
    pub ahead: u32,
    /// Why this host does not dispatch it: the disposition's wire name, plus
    /// the halt cause for `workspace_halted`.
    pub not_here: String,
    /// When its time-boxed hold ends, for a time-held row.
    pub held_until: Option<DateTime<Utc>>,
}

/// What the row's disposition says, before placement.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Class {
    Running,
    Waiting,
    Elsewhere(String, Option<DateTime<Utc>>),
    Refused(NoEstimateReason),
}

/// Halt causes local to the halted host: a peer can still dispatch.
fn host_local(cause: HaltCause) -> bool {
    !matches!(cause, HaltCause::MainRed | HaltCause::CiBilling)
}

fn classify(row: &ReadyRow) -> Class {
    if row.plan.plan_state == PlanState::Running {
        return Class::Running;
    }
    if waiting_position(&row.plan).is_some() {
        return Class::Waiting;
    }
    let d = row.disposition;
    match d {
        Qd::HostConstraint
        | Qd::HostClassRefused
        | Qd::PeerClaim
        | Qd::WorkspaceCommandsMissing
        | Qd::DispatchError => Class::Elsewhere(d.as_str().to_string(), None),
        Qd::WorkspaceHalted => match row.detail.as_deref().and_then(HaltCause::from_wire) {
            Some(cause) if host_local(cause) => {
                Class::Elsewhere(format!("{}:{}", d.as_str(), cause.as_str()), None)
            }
            _ => Class::Refused(NoEstimateReason::NoDispatchPlan),
        },
        Qd::RecheckInterval | Qd::DispatchBackoff | Qd::NoopCooldown | Qd::PrlessRetry => {
            match row.plan.held_until {
                Some(until) => Class::Elsewhere(d.as_str().to_string(), Some(until)),
                None => Class::Refused(NoEstimateReason::NoDispatchPlan),
            }
        }
        Qd::Parked | Qd::HardExclusion | Qd::LabelledBlocked => {
            Class::Refused(NoEstimateReason::Blocked)
        }
        _ => Class::Refused(NoEstimateReason::NoDispatchPlan),
    }
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Each row's [`Placement`], in `rows` order. Pure and deterministic.
#[must_use]
pub fn placements(rows: &[ReadyRow]) -> Vec<Placement> {
    let waiting: Vec<(usize, u32)> = rows
        .iter()
        .filter_map(|r| waiting_position(&r.plan).map(|p| (r.rank, p)))
        .collect();
    rows.iter()
        .map(|row| match classify(row) {
            Class::Running => Placement::Running,
            Class::Waiting => Placement::Waiting,
            Class::Refused(reason) => Placement::Refused(reason),
            Class::Elsewhere(not_here, held_until) => {
                let before = waiting.iter().filter(|(rank, _)| *rank < row.rank);
                let last = before.clone().map(|(_, p)| *p).max().unwrap_or(0);
                Placement::Placed(Placed {
                    position: last.saturating_add(1),
                    ahead: to_u32(waiting.iter().filter(|(_, p)| *p <= last).count()),
                    not_here,
                    held_until,
                })
            }
        })
        .collect()
}
