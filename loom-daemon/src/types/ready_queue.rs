//! Wire types for the work finder's per-issue ready queue (Issue #8852).
//!
//! [`super::WorkFinderTickSummary`] carries aggregate counters ("3
//! backoff-skip, 2 deferred-capacity"); these rows say *which* issue got
//! *which* outcome, in the order the work finder actually ranks them. The
//! order is the daemon's own dispatch comparator,
//! `crate::work_finder::candidate_cmp` (#9244): `loom:operator-priority`
//! (starred) first, starred issues by starred-at, then red-main fixes (only
//! while that repo's `main` is verified red), then workspace priority, then
//! oldest `createdAt`, then issue number. `loom:urgent` and `tier:*` labels
//! do not affect dispatch order.

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
    /// Waiting: the build back-off (#9410) held new issue builds because the
    /// host's review + merge debt is high. Starred and red-main-fix issues
    /// bypass it, so this row is always an unstarred issue.
    DeferredBuildBackoff,
    /// Waiting: outside this host's preferred repo slice while the slice still
    /// had work (repo sharding, #6243).
    DeferredOutOfSlice,
    /// Waiting: its own repo already held `maxConcurrentPerRepo` of the shared
    /// concurrency budget's slots (#9090). The machine-level cap was NOT full;
    /// the slot went to another repo's candidate in the same tick.
    DeferredRepoCap,
    /// Blocked: the work finder is holding dispatch for its whole repo. The
    /// hold has several possible causes (verified-red `main`, a main-health
    /// gate still running, a pre-flight advisory hold, an unusable token pool,
    /// a scheduled drain, the host-distress breaker); the row's `detail`
    /// names which one, as a closed-vocabulary token from
    /// `work_finder::halt_cause` (#9017) — absent on rows recorded by a
    /// cause-less legacy caller.
    WorkspaceHalted,
    /// Blocked: its workspace is missing `.claude/commands/loom/sweep.md`.
    WorkspaceCommandsMissing,
    /// Not for this host: a host-affinity constraint names another host.
    HostConstraint,
    /// Not for this host: it carries `loom:heavy` and this host is
    /// classified `local-dev`, with no override set (Issue #9034).
    HostClassRefused,
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
    pub const ALL: [Self; 24] = [
        Self::Dispatched,
        Self::InFlight,
        Self::DeferredCapacity,
        Self::DeferredRampCap,
        Self::DeferredSaturation,
        Self::DeferredBuildBackoff,
        Self::DeferredOutOfSlice,
        Self::DeferredRepoCap,
        Self::WorkspaceHalted,
        Self::WorkspaceCommandsMissing,
        Self::HostConstraint,
        Self::HostClassRefused,
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
            Self::DeferredBuildBackoff => "deferred_build_backoff",
            Self::DeferredOutOfSlice => "deferred_out_of_slice",
            Self::DeferredRepoCap => "deferred_repo_cap",
            Self::WorkspaceHalted => "workspace_halted",
            Self::WorkspaceCommandsMissing => "workspace_commands_missing",
            Self::HostConstraint => "host_constraint",
            Self::HostClassRefused => "host_class_refused",
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
            | Self::DeferredBuildBackoff
            | Self::DeferredOutOfSlice
            | Self::DeferredRepoCap => "ready",
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
            Self::DeferredBuildBackoff => "waiting: build back-off (review/merge debt high)",
            Self::DeferredOutOfSlice => "waiting: outside this host's repo slice",
            Self::DeferredRepoCap => "waiting: this repo is at its per-repo cap",
            Self::WorkspaceHalted => {
                "blocked: repo dispatch held (red main, gate, token pool, drain or breaker)"
            }
            Self::WorkspaceCommandsMissing => "blocked: workspace missing sweep command",
            Self::HostConstraint => "not for this host (host affinity)",
            Self::HostClassRefused => "blocked: heavy sweep refused on local-dev host_class",
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
    /// Deprecated (#9244): always `false`. `loom:urgent` no longer affects
    /// dispatch order; the field stays on the wire for one release.
    pub urgent: bool,
    /// Whether the issue is starred (`loom:operator-priority`, #9244).
    #[serde(default)]
    pub operator_priority: bool,
    /// When it was starred, when known (#9244). Absent for an unstarred
    /// issue, or a starred one ordered by its `createdAt` fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_priority_at: Option<String>,
    /// Whether it is a red-main fix boosted this tick: it carries
    /// `<!-- loom:main-red-fix -->` and its repo's `main` is verified red.
    #[serde(default)]
    pub main_red_fix: bool,
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
    /// The dispatch-plan fields (Issue #9288): `position`, `plan_state`,
    /// `keys`, `gate`, `in_slice`, `hot`, `owning_shard`, `repo_cap`.
    /// Flattened, so they sit beside `rank` on the wire; all default when
    /// absent.
    #[serde(flatten, default)]
    pub plan: super::RowPlan,
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
        // A pre-#9288 payload has no plan fields: they default.
        assert_eq!(row.plan, crate::types::RowPlan::default());
    }

    /// Issue #9288: the plan fields are flattened beside `rank`, and a tick
    /// summary without a `plan` block still parses.
    #[test]
    fn plan_fields_flatten_and_old_summaries_parse() {
        let row: ReadyQueueRow = serde_json::from_value(serde_json::json!({
            "rank": 4, "repo": "/r", "issue": 7, "workspace_priority": 100,
            "urgent": false, "disposition": "deferred_capacity",
            "position": 2, "plan_state": "next", "gate": "capacity",
            "keys": [{"name": "number", "value": 7}]
        }))
        .unwrap();
        assert_eq!(row.plan.position, Some(2));
        assert_eq!(row.plan.plan_state, crate::types::PlanState::Next);
        let back = serde_json::to_value(&row).unwrap();
        assert_eq!(back["position"], 2);
        assert_eq!(back["plan_state"], "next");
        assert!(back.get("plan").is_none(), "flattened, not nested: {back}");

        let summary: crate::types::WorkFinderTickSummary =
            serde_json::from_value(serde_json::json!({
                "at": "2026-09-28T00:00:00Z", "max_concurrent": 2, "seen": 0,
                "dispatched": 0, "skipped_labeled": 0, "skipped_in_flight": 0,
                "skipped_quarantined": 0, "skipped_pr_open": 0,
                "skipped_peer_claim": 0, "skipped_backoff": 0,
                "deferred_capacity": 0, "deferred_ramp_cap": 0, "errors": 0,
                "halted": false
            }))
            .unwrap();
        assert!(summary.plan.is_none());
    }

    #[test]
    fn dispositions_serialize_snake_case_and_classify() {
        let json = serde_json::to_string(&QueueDisposition::DeferredRampCap).unwrap();
        assert_eq!(json, "\"deferred_ramp_cap\"");
        assert_eq!(QueueDisposition::Dispatched.state(), "running");
        assert_eq!(QueueDisposition::DeferredCapacity.state(), "ready");
        assert_eq!(QueueDisposition::OpenPr.state(), "blocked");
        // #9034: a host-class refusal is `blocked`, not `ready` — it never
        // self-resolves by waiting, the way a capacity/ramp/saturation defer
        // does.
        assert_eq!(QueueDisposition::HostClassRefused.state(), "blocked");
        assert_eq!(QueueDisposition::HostClassRefused.as_str(), "host_class_refused");
    }

    /// Issue #9410: the build back-off disposition round-trips, is a
    /// capacity-style `ready` wait, and a pre-#9410 tick summary still parses.
    #[test]
    fn build_backoff_disposition_round_trips_and_old_summaries_parse() {
        let d = QueueDisposition::DeferredBuildBackoff;
        assert_eq!(QueueDisposition::ALL.len(), 24);
        assert!(QueueDisposition::ALL.contains(&d));
        let json = serde_json::to_value(d).unwrap();
        assert_eq!(json, serde_json::json!("deferred_build_backoff"));
        assert_eq!(serde_json::from_value::<QueueDisposition>(json).unwrap(), d);
        assert_eq!(d.state(), "ready");
        assert_eq!(d.reason(), "waiting: build back-off (review/merge debt high)");
        let gate = serde_json::to_value(crate::types::PlanGate::BuildBackoff).unwrap();
        assert_eq!(gate, serde_json::json!("build_backoff"));

        let summary: crate::types::WorkFinderTickSummary =
            serde_json::from_value(serde_json::json!({
                "at": "2026-09-28T00:00:00Z", "max_concurrent": 2, "seen": 0,
                "dispatched": 0, "skipped_labeled": 0, "skipped_in_flight": 0,
                "skipped_quarantined": 0, "skipped_pr_open": 0,
                "skipped_peer_claim": 0, "skipped_backoff": 0,
                "deferred_capacity": 0, "deferred_ramp_cap": 0, "errors": 0,
                "halted": false, "saturation_held": true
            }))
            .unwrap();
        assert_eq!((summary.deferred_build_backoff, summary.build_backoff_held), (0, false));
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
