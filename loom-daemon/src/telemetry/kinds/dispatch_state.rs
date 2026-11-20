//! `eta.dispatch_state` (#12918): one classified open item's dispatch gate, as
//! the step dispatcher saw it on its last tick.
//!
//! # Why this exists
//!
//! `eta.dispatch_eligibility` (#12790) journals the *work finder's* queue
//! disposition for a ready issue. The step dispatcher decides differently: it
//! classifies every open item (issue or PR) into a step or a pass, then weighs
//! it into the draw pool or out of it with an [`Exclusion`]. An ETA consumer
//! that wants to know **when a gated item can next be drawn** needs that
//! decision per item, with the gate's release time when one exists: a
//! cooling-down item's `gate_until`, and a Champion re-evaluation ladder's
//! `rung`/`next`. None of that was exported.
//!
//! # How it is produced
//!
//! The step driver's tick already holds every one of these facts (the
//! [`Weighing`], the merged [`StepCooldowns`], the bodies' ladder markers), so
//! `observability::dispatch_state` journals them from there with **no forge
//! call**. OTLP only; one log per item per emission; the body is the record's
//! JSON and the signals readers filter on ride as `loom.eta.dispatch_state.*`
//! attributes. No row cap: every classified item is a row.
//!
//! An absent `gate_until`, `reeval_rung` or `reeval_next` means *no such gate
//! is known*, never "now" or 0.
//!
//! [`Exclusion`]: crate::step_dispatch::item_draw::Exclusion
//! [`Weighing`]: crate::step_dispatch::item_draw::Weighing
//! [`StepCooldowns`]: crate::step_dispatch::outcome_ledger::StepCooldowns

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::RepoVisibility;

/// Payload version carried in the record's own `schema_version` field.
pub const DISPATCH_STATE_SCHEMA_VERSION: u32 = 1;

/// The `loom.*` attribute keys the OTLP mapping promotes out of the body.
/// Every key MUST also be on the gateway's `transform/privacy` log `keep_keys`
/// allowlist; the mapping's
/// `attributes_are_declared_and_the_collector_keeps_them` test enforces both.
pub const DISPATCH_STATE_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.eta.dispatch_state.artifact",
    "loom.eta.dispatch_state.step",
    "loom.eta.dispatch_state.in_pool",
    "loom.eta.dispatch_state.exclusion",
    "loom.eta.dispatch_state.gate_until",
    "loom.eta.dispatch_state.reeval_rung",
    "loom.eta.dispatch_state.reeval_next",
];

/// Which forge artifact the item's number names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Artifact {
    Issue,
    Pr,
}

impl Artifact {
    /// The snake_case wire name (identical to the serde form).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::Pr => "pr",
        }
    }
}

/// Why this emission was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateTransition {
    /// First sight, or one of (`in_pool`, `exclusion`, `gate_until`, rung)
    /// changed since the last emission.
    Changed,
    /// Unchanged, re-sent by the periodic full reconciliation.
    Refresh,
}

impl StateTransition {
    /// The snake_case wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Changed => "changed",
            Self::Refresh => "refresh",
        }
    }
}

/// One classified item's dispatch gate as of one step-dispatch tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchStateRecord {
    /// [`DISPATCH_STATE_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Forge `owner/repo`. Never a local path.
    pub repo: String,
    /// The host whose dispatcher classified the item.
    pub host: String,
    /// The item's number: the step's target (the PR for a PR step), else the
    /// issue, else the PR.
    pub number: u32,
    pub artifact: Artifact,
    /// The step the classifier chose (`Step::as_str`); absent for a pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// Whether the item was in the draw pool this tick.
    pub in_pool: bool,
    /// [`Exclusion::as_str`] when it was out of the pool.
    ///
    /// [`Exclusion::as_str`]: crate::step_dispatch::item_draw::Exclusion::as_str
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusion: Option<String>,
    /// When the gate lifts, where it has a per-item end (a `cooling_down`
    /// item's window, including a not-yet-due re-evaluation). Absent, never 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_until: Option<DateTime<Utc>>,
    /// The Champion re-evaluation ladder rung from the item's body marker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reeval_rung: Option<u32>,
    /// The marker's `next`: not re-evaluated before this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reeval_next: Option<DateTime<Utc>>,
    /// The item weight `iw` (priority × age term) when pooled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iw: Option<u64>,
    /// The item's weight in the host-wide draw, when pooled: `iw` scaled by
    /// its repo's terms, as the draw's first pick sees it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<u64>,
    /// When this daemon first observed the item `claimed` (a role session is
    /// treating it), kept while it stays claimed. A **lower bound** on the
    /// claim start, never the exact start: a restart or a claim taken between
    /// ticks is first observed late. Absent when not claimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_since: Option<DateTime<Utc>>,
    /// Qualifying Judge rejections of the Doctor PR's GLM attempts, from the
    /// sweep checkpoint's `doctor_escalation` state for this PR. Doctor PR
    /// rows only; absent (unknown, not `0`) when no state was read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doctor_rejections: Option<u32>,
    /// The GLM-to-Claude escalation latch, same source and omission contract
    /// as `doctor_rejections`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doctor_escalated: Option<bool>,
    pub transition: StateTransition,
    /// The emission instant.
    pub sampled_at: DateTime<Utc>,
    /// The repo's visibility, for the redaction layer.
    #[serde(default)]
    pub visibility: RepoVisibility,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn absent_gate_fields_are_missing_from_the_wire_not_zero() {
        let at = "2026-10-20T12:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let record = DispatchStateRecord {
            schema_version: DISPATCH_STATE_SCHEMA_VERSION,
            repo: "rjwalters/loom".into(),
            host: "host-a".into(),
            number: 7,
            artifact: Artifact::Pr,
            step: Some("champion_merge".into()),
            in_pool: false,
            exclusion: Some("cooling_down".into()),
            gate_until: Some(at),
            reeval_rung: Some(2),
            reeval_next: Some(at),
            iw: None,
            weight: None,
            claimed_since: None,
            doctor_rejections: None,
            doctor_escalated: None,
            transition: StateTransition::Changed,
            sampled_at: at,
            visibility: RepoVisibility::Public,
        };
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(value["exclusion"], "cooling_down");
        assert_eq!(value["artifact"], "pr");
        assert!(value.get("iw").is_none());
        assert!(value.get("weight").is_none());
        let back: DispatchStateRecord = serde_json::from_value(value).unwrap();
        assert_eq!(back, record);
        let bare = DispatchStateRecord {
            gate_until: None,
            reeval_rung: None,
            reeval_next: None,
            ..record
        };
        let value = serde_json::to_value(&bare).unwrap();
        assert!(value.get("gate_until").is_none(), "{value}");
        assert!(value.get("reeval_next").is_none(), "{value}");
    }

    #[test]
    fn wire_names() {
        assert_eq!(Artifact::Issue.as_str(), "issue");
        assert_eq!(StateTransition::Refresh.as_str(), "refresh");
        assert_eq!(serde_json::to_value(StateTransition::Changed).unwrap(), "changed");
    }
}
