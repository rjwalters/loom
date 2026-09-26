//! Wire types for the work finder's per-issue ready queue (Issue #8852).
//!
//! [`super::WorkFinderTickSummary`] carries aggregate counters ("3
//! backoff-skip, 2 deferred-capacity"); these rows say *which* issue got
//! *which* outcome, in the order the work finder actually ranks them. The
//! order is the daemon's own dispatch comparator,
//! `crate::work_finder::candidate_cmp`: workspace priority, then `loom:urgent`,
//! then oldest `createdAt`, then issue number. `tier:*` labels are carried
//! as information only; they do not affect dispatch order.

use serde::{Deserialize, Serialize};

/// What the work finder did with one ready issue on its most recent tick.
///
/// Each variant is recorded next to the `TickReport` counter bump it
/// matches (except `WorkspaceHalted`, which has only the tick-wide `halted`
/// flag), so the rows and the aggregate counts agree. Unknown values
/// (a newer daemon's variant read by an older client) parse as
/// [`Self::Unknown`] rather than failing the whole status payload.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum QueueDisposition {
    /// A new sweep was dispatched for it this tick.
    Dispatched,
    /// A live sweep already exists for it (or a live claim guard refused it).
    InFlight,
    /// Waiting: the shared concurrency cap was full.
    DeferredCapacity,
    /// Waiting: the per-tick admission ramp cap was reached.
    DeferredRampCap,
    /// Waiting: the saturation admission brake held new admissions.
    DeferredSaturation,
    /// Waiting: outside this host's preferred repo slice while the slice still
    /// had work (repo sharding, #6243).
    DeferredOutOfSlice,
    /// Blocked: the work finder is holding dispatch for its whole repo. The
    /// hold has several possible causes (verified-red `main`, a main-health
    /// gate still running, a pre-flight advisory hold, an unusable token pool,
    /// a scheduled drain, the host-distress breaker); the row does not say
    /// which.
    WorkspaceHalted,
    /// Blocked: its workspace is missing `.claude/commands/loom/sweep.md`.
    WorkspaceCommandsMissing,
    /// Not for this host: a host-affinity constraint names another host.
    HostConstraint,
    /// Blocked: it carries a skip/park label (or lacks a required capability).
    Parked,
    /// Blocked: a hard-exclusion rule applies (e.g. the `external` label).
    HardExclusion,
    /// Waiting: its own `loom:recheck-interval` marker has not elapsed.
    RecheckInterval,
    /// Blocked: quarantined for repeated insta-crashes.
    Quarantined,
    /// Waiting: inside a dispatch-backoff window after a failed dispatch.
    DispatchBackoff,
    /// Waiting: inside a backoff window armed by the open-PR guard.
    OpenPrBackoff,
    /// Waiting: a no-op release cooldown has not elapsed.
    NoopCooldown,
    /// Blocked: a previous sweep declined it and the cooldown has not elapsed.
    Declined,
    /// Waiting: a previous dispatch produced no PR; its retry window is open.
    PrlessRetry,
    /// Held elsewhere: a peer host advertised a live soft claim.
    PeerClaim,
    /// Blocked: it already has an open linked PR.
    OpenPr,
    /// The dispatch attempt failed.
    DispatchError,
    /// Blocked on the forge: the issue carries `loom:blocked` without
    /// `loom:issue`, so the work finder never lists it (Issue #8957). Only in
    /// `queue.snapshot`, added by the collector from its own cached forge
    /// read; never a tick outcome, so it is not in [`Self::ALL`].
    LabelledBlocked,
    /// A disposition this client does not know (forward compatibility).
    #[serde(other)]
    Unknown,
}

impl QueueDisposition {
    /// Every disposition a work-finder tick can assign, in declaration order
    /// ([`Self::LabelledBlocked`] and [`Self::Unknown`] excluded). The queue-depth metrics (Issue #8852, phase 2) emit one
    /// point per entry every tick, zeros included, so an empty queue reads as
    /// `0` rather than as a missing series.
    pub const ALL: [Self; 21] = [
        Self::Dispatched,
        Self::InFlight,
        Self::DeferredCapacity,
        Self::DeferredRampCap,
        Self::DeferredSaturation,
        Self::DeferredOutOfSlice,
        Self::WorkspaceHalted,
        Self::WorkspaceCommandsMissing,
        Self::HostConstraint,
        Self::Parked,
        Self::HardExclusion,
        Self::RecheckInterval,
        Self::Quarantined,
        Self::DispatchBackoff,
        Self::OpenPrBackoff,
        Self::NoopCooldown,
        Self::Declined,
        Self::PrlessRetry,
        Self::PeerClaim,
        Self::OpenPr,
        Self::DispatchError,
    ];

    /// The snake_case wire name (identical to the serde form; pinned by a
    /// test). Used as the `reason` metric label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dispatched => "dispatched",
            Self::InFlight => "in_flight",
            Self::DeferredCapacity => "deferred_capacity",
            Self::DeferredRampCap => "deferred_ramp_cap",
            Self::DeferredSaturation => "deferred_saturation",
            Self::DeferredOutOfSlice => "deferred_out_of_slice",
            Self::WorkspaceHalted => "workspace_halted",
            Self::WorkspaceCommandsMissing => "workspace_commands_missing",
            Self::HostConstraint => "host_constraint",
            Self::Parked => "parked",
            Self::HardExclusion => "hard_exclusion",
            Self::RecheckInterval => "recheck_interval",
            Self::Quarantined => "quarantined",
            Self::DispatchBackoff => "dispatch_backoff",
            Self::OpenPrBackoff => "open_pr_backoff",
            Self::NoopCooldown => "noop_cooldown",
            Self::Declined => "declined",
            Self::PrlessRetry => "prless_retry",
            Self::PeerClaim => "peer_claim",
            Self::OpenPr => "open_pr",
            Self::DispatchError => "dispatch_error",
            Self::LabelledBlocked => "labelled_blocked",
            Self::Unknown => "unknown",
        }
    }

    /// Coarse state for dashboards: `running`, `ready` (waiting only on
    /// capacity-style limits), or `blocked` (something specific to the issue,
    /// its repo, or its history is holding it).
    #[must_use]
    pub fn state(self) -> &'static str {
        match self {
            Self::Dispatched | Self::InFlight => "running",
            Self::DeferredCapacity
            | Self::DeferredRampCap
            | Self::DeferredSaturation
            | Self::DeferredOutOfSlice => "ready",
            _ => "blocked",
        }
    }

    /// The human-readable reason shown next to the issue.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::Dispatched => "dispatched this tick",
            Self::InFlight => "sweep already running",
            Self::DeferredCapacity => "waiting: concurrency cap full",
            Self::DeferredRampCap => "waiting: per-tick admission cap reached",
            Self::DeferredSaturation => "waiting: host saturated (admission brake)",
            Self::DeferredOutOfSlice => "waiting: outside this host's repo slice",
            Self::WorkspaceHalted => {
                "blocked: repo dispatch held (red main, gate, token pool, drain or breaker)"
            }
            Self::WorkspaceCommandsMissing => "blocked: workspace missing sweep command",
            Self::HostConstraint => "not for this host (host affinity)",
            Self::Parked => "blocked: skip/park label",
            Self::HardExclusion => "blocked: hard-exclusion rule",
            Self::RecheckInterval => "waiting: issue's recheck interval",
            Self::Quarantined => "blocked: quarantined after repeated crashes",
            Self::DispatchBackoff => "waiting: dispatch backoff after a failure",
            Self::OpenPrBackoff => "waiting: open-PR guard backoff",
            Self::NoopCooldown => "waiting: no-op release cooldown",
            Self::Declined => "blocked: declined, cooldown active",
            Self::PrlessRetry => "waiting: last run produced no PR",
            Self::PeerClaim => "held by a peer host",
            Self::OpenPr => "blocked: open linked PR",
            Self::DispatchError => "dispatch failed",
            Self::LabelledBlocked => "blocked: labelled loom:blocked",
            Self::Unknown => "unknown",
        }
    }
}

/// One ready issue on the most recent work-finder tick, in dispatch order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadyQueueRow {
    /// 1-based position in the work finder's dispatch order.
    pub rank: usize,
    /// The workspace (repo root) the issue belongs to.
    pub repo: String,
    /// The issue number.
    pub issue: u32,
    /// The owning workspace's priority tier (lower dispatches first).
    pub workspace_priority: u32,
    /// Whether the issue carries `loom:urgent`.
    pub urgent: bool,
    /// The issue's `createdAt`, when the listing supplied it.
    #[serde(default)]
    pub created_at: Option<String>,
    /// The issue's `tier:*` label, if any. Informational only: the work
    /// finder does not order by it.
    #[serde(default)]
    pub tier: Option<String>,
    /// What the work finder did with it.
    pub disposition: QueueDisposition,
    /// Specifics, when there are any: the park label, the open PR number, the
    /// dispatch error text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// [`QueueDisposition::state`], serialized so clients (the `serve`
    /// dashboard, the fleet backend) never keep their own copy of the
    /// mapping. Empty on payloads from daemons older than phase 2.
    #[serde(default)]
    pub state: String,
    /// [`QueueDisposition::reason`], serialized for the same reason.
    #[serde(default)]
    pub reason: String,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn unknown_disposition_parses_instead_of_failing() {
        let row: ReadyQueueRow = serde_json::from_value(serde_json::json!({
            "rank": 1, "repo": "/r", "issue": 7, "workspace_priority": 100,
            "urgent": false, "disposition": "some_future_reason"
        }))
        .unwrap();
        assert_eq!(row.disposition, QueueDisposition::Unknown);
        assert_eq!(row.created_at, None);
        // A pre-phase-2 payload has no `state`/`reason`: they default empty.
        assert!(row.state.is_empty() && row.reason.is_empty());
    }

    #[test]
    fn dispositions_serialize_snake_case_and_classify() {
        let json = serde_json::to_string(&QueueDisposition::DeferredRampCap).unwrap();
        assert_eq!(json, "\"deferred_ramp_cap\"");
        assert_eq!(QueueDisposition::Dispatched.state(), "running");
        assert_eq!(QueueDisposition::DeferredCapacity.state(), "ready");
        assert_eq!(QueueDisposition::OpenPr.state(), "blocked");
    }

    #[test]
    fn as_str_matches_serde_for_every_disposition() {
        for d in QueueDisposition::ALL {
            assert_eq!(serde_json::to_value(d).unwrap(), serde_json::json!(d.as_str()));
        }
        assert_eq!(QueueDisposition::Unknown.as_str(), "unknown");
        // Forge-side only (#8957): outside `ALL`, still serde-consistent.
        let labelled = QueueDisposition::LabelledBlocked;
        assert!(!QueueDisposition::ALL.contains(&labelled));
        assert_eq!(serde_json::to_value(labelled).unwrap(), serde_json::json!(labelled.as_str()));
        assert_eq!(labelled.state(), "blocked");
    }
}
