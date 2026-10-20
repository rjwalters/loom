//! `eta.dispatch_eligibility` (#12790): the dispatcher's own
//! schedulability decision for one ready item, over time.
//!
//! # Why this exists
//!
//! ETA's worst-fit phase is `ready_wait` — the time an issue sits in
//! `loom:issue` before a Builder claims it. It is a **queueing delay**, but the
//! existing dwell facts (`eta.stage_outcome`) cannot see the one thing that
//! drives it: *why the scheduler is or is not picking the item right now*. A
//! `loom:issue` ("ready") label does not mean the dispatcher can actually
//! select it. This fact splits a ready item's dwell at the **eligibility
//! boundary** — schedulable-waiting (congestion) vs ineligible (held) — which
//! have completely different hazards, and records how many *eligible*
//! competitors are ahead of it under the real dispatch policy.
//!
//! # How it is produced
//!
//! The producer is `observability::dispatch_eligibility`, which reads the work
//! finder's last tick summary (`crate::work_finder::last_tick_summary`) on the
//! collector's snapshot cadence — never from the tick loop, and it makes **no
//! forge call** (it reads only the synchronous `cached_repo_ref` the collector
//! already warmed this pass; an unresolved row is dropped). This is the same
//! write-chokepoint-emission rule `queue.snapshot` and `ops::disposition`
//! follow. The dispatcher already computed the per-item `QueueDisposition`;
//! this fact only journals it.
//!
//! **OTLP only.** One log per ready item per emission (a class transition or a
//! refresh). The body is the record's JSON so ClickHouse can `JSONExtract` the
//! eligibility fields; `loom.repo`, `loom.issue` and `loom.repo.visibility`
//! ride as attributes (all already on the gateway `keep_keys`), so a reader can
//! filter and join with the dwell facts without a collector change. The wire
//! tag keeps the `eta.` prefix so the ETA readers' `loom.kind LIKE 'eta%'`
//! queries catch it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::RepoVisibility;

/// Payload version carried in the record's own `schema_version` field
/// (distinct from the envelope's gate, which is `NEW_KIND_SCHEMA_VERSION`).
pub const DISPATCH_ELIGIBILITY_SCHEMA_VERSION: u32 = 1;

/// The `loom.*` attribute keys the OTLP mapping promotes out of the body so
/// SigNoz can `GROUP BY`/filter on them without a `JSONExtract` (#12867). The
/// body still carries the full record for the richer fields, but the three
/// signals that answer "why isn't this eligible-looking item being picked" —
/// the eligibility **verdict**, the dispatcher's **reason** token (the
/// blocking reason), and the item's **rank** — are the ones readers query, so
/// they ride as attributes. Every key here MUST also be on the gateway's
/// `transform/privacy` log `keep_keys` allowlist, or the collector strips it
/// before it reaches the store; the mapping's
/// `attributes_are_declared_and_the_collector_keeps_them` test enforces both.
pub const DISPATCH_ELIGIBILITY_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.eta.dispatch_eligibility.eligibility",
    "loom.eta.dispatch_eligibility.reason",
    "loom.eta.dispatch_eligibility.rank",
];

/// Whether the dispatcher can currently select the item, split at the
/// eligibility boundary (#12790).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Eligibility {
    /// Schedulable: the item is held only by a capacity-style limit (the
    /// concurrency cap, the ramp, saturation, the build back-off, the repo cap,
    /// the repo slice or a file overlap). It will dispatch once the resource
    /// pool frees — this is congestion, not ineligibility.
    Eligible,
    /// Ineligible: something specific to the item, its repo or its history
    /// holds it (an open PR, a park/hold label, a hard exclusion, a quarantine,
    /// a cooldown/back-off, a halted repo, a peer claim, a host constraint).
    /// No amount of waiting on the pool resolves it.
    Blocked,
}

impl Eligibility {
    /// The snake_case wire name (identical to the serde form).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Eligible => "eligible",
            Self::Blocked => "blocked",
        }
    }

    /// Classify from a [`crate::types::QueueDisposition::state`] token: the
    /// coarse `"blocked"` state is [`Self::Blocked`]; `"ready"` (and anything
    /// else) is [`Self::Eligible`]. The producer only emits a fact for a ready
    /// item that is waiting (`ready` / `blocked`), never for a `running`
    /// (dispatched / in-flight) row.
    #[must_use]
    pub fn from_state(state: &str) -> Self {
        if state == "blocked" {
            Self::Blocked
        } else {
            Self::Eligible
        }
    }
}

/// How this emission relates to the previous one the producer made for the same
/// `(repo, issue)` item — the wire value of `transition`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EligibilityTransition {
    /// First sight of the item, or its eligibility class flipped since the last
    /// emission. `block_start` is re-anchored to this tick.
    Changed,
    /// Same eligibility class as last emitted, re-sent because the refresh
    /// window elapsed. `block_start` is unchanged — the stream stays alive and
    /// the class-start is re-stated without a flip.
    Refresh,
}

impl EligibilityTransition {
    /// The snake_case wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Changed => "changed",
            Self::Refresh => "refresh",
        }
    }
}

/// One ready item's dispatch-eligibility decision as of one work-finder tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchEligibilityRecord {
    /// [`DISPATCH_ELIGIBILITY_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Forge `owner/repo`. Never a local path (resolved through the cached
    /// repo-ref map, like `queue.snapshot`).
    pub repo: String,
    /// The issue number.
    pub issue: u32,
    /// The host whose dispatcher made the decision.
    pub host: String,
    /// Whether the dispatcher can currently select it.
    pub eligibility: Eligibility,
    /// The dispatcher's own reason token — the
    /// [`crate::types::QueueDisposition::as_str`] value (e.g.
    /// `deferred_capacity`, `deferred_repo_cap`, `open_pr`, `parked`,
    /// `workspace_halted`, `quarantined`, `peer_claim`, `host_constraint`, …).
    pub reason: String,
    /// The coarse [`crate::types::QueueDisposition::state`] token (`ready` or
    /// `blocked` — a `running` row is never emitted here).
    pub state: String,
    /// When the item **entered its current eligibility class** on this host, as
    /// first observed by this process: the work-finder `tick_at` of the first
    /// sample in which it carried this class. An observation floor, not an
    /// exact instant — a class that lasts less than one collector interval can
    /// be missed, and a daemon restart re-anchors it (the same accepted
    /// tradeoff `ops::disposition` documents). Splitting dwell here is the
    /// point of the fact: schedulable-waiting vs ineligible time.
    pub block_start: DateTime<Utc>,
    /// Whether this emission is a class flip ([`EligibilityTransition::Changed`])
    /// or a same-class refresh.
    pub transition: EligibilityTransition,
    /// The item's 1-based bare comparator rank on the host this tick (its place
    /// in the full dispatch order, blocked rows included).
    pub rank: u32,
    /// **Effective eligible work-ahead**: the count of *eligible* items
    /// (schedulable or already running — anything not `blocked`) ranked ahead
    /// of this item across the host's whole concurrency pool. Blocked items are
    /// excluded, so this is the real number of competitors that will take a
    /// slot before this item, not a raw "priority ≥ mine" count.
    pub eligible_work_ahead: u32,
    /// The same count restricted to this item's own repo (the per-repo cap
    /// pool) — the competitors ahead of it under `maxConcurrentPerRepo`.
    pub eligible_work_ahead_repo: u32,
    /// The resource pool / scheduling class the item competes in: the forge
    /// `owner/repo` slug (the per-repo cap and the repo halt state are the
    /// dominant per-item gates, and repo is the dimension readers join on).
    pub resource_pool: String,
    /// The host's effective concurrency cap this tick (the global pool size).
    pub max_concurrent: u32,
    /// The issue's priority level (`crate::priority_pick::Level::as_str`):
    /// `default`, `important` or `very-important` (from its `loom:important` /
    /// `loom:very-important` label). Higher levels are drawn ahead of lower
    /// ones, so this is the priority axis the work-ahead counts are relative
    /// to.
    pub level: String,
    /// The collector sample instant (when this fact was emitted).
    pub sampled_at: DateTime<Utc>,
    /// The work-finder tick this decision reflects.
    pub tick_at: DateTime<Utc>,
    /// The repo's visibility, carried so the redaction layer can tag the row
    /// (the same anti-leak rule as `queue.snapshot`).
    #[serde(default)]
    pub visibility: RepoVisibility,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn eligibility_classifies_from_state_and_round_trips() {
        assert_eq!(Eligibility::from_state("blocked"), Eligibility::Blocked);
        assert_eq!(Eligibility::from_state("ready"), Eligibility::Eligible);
        // Defensive: an unexpected token is treated as schedulable, never as a
        // silent "blocked" that would hide the item from a congestion model.
        assert_eq!(Eligibility::from_state("running"), Eligibility::Eligible);
        assert_eq!(serde_json::to_value(Eligibility::Blocked).unwrap(), "blocked");
        assert_eq!(Eligibility::Eligible.as_str(), "eligible");
    }

    #[test]
    fn transition_wire_names() {
        assert_eq!(serde_json::to_value(EligibilityTransition::Changed).unwrap(), "changed");
        assert_eq!(EligibilityTransition::Refresh.as_str(), "refresh");
    }

    #[test]
    fn record_round_trips_through_serde() {
        let at = "2026-10-18T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let record = DispatchEligibilityRecord {
            schema_version: DISPATCH_ELIGIBILITY_SCHEMA_VERSION,
            repo: "rjwalters/loom".into(),
            issue: 42,
            host: "host-a".into(),
            eligibility: Eligibility::Eligible,
            reason: "deferred_capacity".into(),
            state: "ready".into(),
            block_start: at,
            transition: EligibilityTransition::Changed,
            rank: 3,
            eligible_work_ahead: 2,
            eligible_work_ahead_repo: 2,
            resource_pool: "rjwalters/loom".into(),
            max_concurrent: 2,
            level: "default".into(),
            sampled_at: at,
            tick_at: at,
            visibility: RepoVisibility::Public,
        };
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(value["eligibility"], "eligible");
        assert_eq!(value["eligible_work_ahead"], 2);
        let back: DispatchEligibilityRecord = serde_json::from_value(value).unwrap();
        assert_eq!(back, record);
    }
}
