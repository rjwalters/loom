//! Wire types for the starred-issue liveness contract (#9244 slice C, #9301).
//!
//! A starred (`loom:operator-priority`) issue is always either being worked
//! on or escalated to the operator with one concrete ask. The daemon computes
//! a [`LandingStage`] for every starred issue (and every blocker that inherits
//! a star) and publishes the rows here: on `DaemonStatusReport` (read by
//! `loom-daemon status`, `loom-daemon queue` and the `operator_attention`
//! health section) and on `queue.snapshot` (read by loom-ui).
//!
//! The computation lives in `crate::star_liveness`; this file is only the
//! shape both ends agree on. Every field a newer daemon adds must be
//! `#[serde(default)]` so an older client keeps parsing.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Where a starred issue is on its way to landing.
///
/// Every stage except [`Self::NeedsOperator`] has an agent owner. A stage an
/// agent owns can still escalate through the no-progress watchdog, which
/// moves it to `NeedsOperator`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LandingStage {
    /// Waiting for (or in) curation: `loom:triage`, `loom:curated`,
    /// `loom:curating`, or no workflow label at all.
    Curating,
    /// `loom:issue`, not yet claimed.
    Ready,
    /// A Builder holds it (`loom:building` or a live sweep), no PR yet.
    Building,
    /// Its PR waits on Judge.
    InReview,
    /// Its PR waits on Doctor (changes requested, conflict, CI failure).
    ChangesRequested,
    /// Its PR is approved (`loom:pr`) and waits on Champion.
    Mergeable,
    /// Its PR is approved and a live sweep on this host is driving the merge.
    Merging,
    /// An open issue blocks it; the blocker inherits its star.
    BlockedBy,
    /// Only the operator can move it. [`StarLandingRow::ask`] says what to do.
    /// (A watchdog escalation keeps the agent-owned stage and sets `ask`.)
    NeedsOperator,
    /// Nothing is wrong with the issue; this host has no free slot for it.
    NoCapacity,
    /// A stage this client does not know (forward compatibility).
    #[serde(other)]
    Unknown,
}

impl LandingStage {
    /// The kebab-case wire name (identical to the serde form).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Curating => "curating",
            Self::Ready => "ready",
            Self::Building => "building",
            Self::InReview => "in-review",
            Self::ChangesRequested => "changes-requested",
            Self::Mergeable => "mergeable",
            Self::Merging => "merging",
            Self::BlockedBy => "blocked-by",
            Self::NeedsOperator => "needs-operator",
            Self::NoCapacity => "no-capacity",
            Self::Unknown => "unknown",
        }
    }
}

/// What kind of help the operator is asked for. The kind is part of the
/// escalation's dedupe key, so its wire names are stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AskKind {
    /// `loom:operator-only` (or one of its sub-kinds) on the issue or its PR.
    OperatorOnly,
    /// `loom:operator-decision` on the issue or its PR.
    OperatorDecision,
    /// Champion's merge-risk or critical-file hold (`loom:operator` on the PR).
    MergeRiskHold,
    /// The forge refused the merge of an approved PR (405, ruleset, merge
    /// method), the #9268 shape.
    MergeRefused,
    /// The token pool is exhausted on this host and no other host has claimed
    /// the issue.
    PoolsExhausted,
    /// No host this daemon knows of manages the repo (a loom-ui star for a
    /// repo outside this host's workspace registry).
    UnmanagedRepo,
    /// `loom:blocked` with no open blocking issue named anywhere.
    BlockedUnnamed,
    /// No forward progress for the watchdog window.
    NoProgress,
    /// A kind this client does not know (forward compatibility).
    #[serde(other)]
    Unknown,
}

impl AskKind {
    /// The kebab-case wire name (identical to the serde form).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OperatorOnly => "operator-only",
            Self::OperatorDecision => "operator-decision",
            Self::MergeRiskHold => "merge-risk-hold",
            Self::MergeRefused => "merge-refused",
            Self::PoolsExhausted => "pools-exhausted",
            Self::UnmanagedRepo => "unmanaged-repo",
            Self::BlockedUnnamed => "blocked-unnamed",
            Self::NoProgress => "no-progress",
            Self::Unknown => "unknown",
        }
    }
}

/// One concrete ask for the operator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorAsk {
    pub kind: AskKind,
    /// The dedupe key: `<kind>:<specifics>`. The same (issue, key) pair is
    /// escalated once, across ticks and across hosts (the forge comment
    /// carries it in a marker). Stable for a given cause.
    pub key: String,
    /// The ask itself, one or two sentences, naming what to do.
    pub text: String,
}

/// One starred issue (or a blocker that inherited a star).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StarLandingRow {
    /// Forge `owner/repo`.
    pub repo: String,
    pub issue: u32,
    pub stage: LandingStage,
    /// Who acts next: `curator`, `builder`, `judge`, `doctor`, `champion`,
    /// `work-finder`, `operator`, or `blocker #N`.
    pub next_actor: String,
    /// When this issue entered `stage`, as this host first observed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage_since: Option<DateTime<Utc>>,
    /// Seconds in `stage` at report time.
    #[serde(default)]
    pub time_in_stage_secs: u64,
    /// The open PR driving it, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u32>,
    /// `BlockedBy`: the blocking issue, as `#N`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_by: Option<String>,
    /// `NoCapacity`: why there is no slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_capacity: Option<String>,
    /// `NeedsOperator`: the ask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask: Option<OperatorAsk>,
    /// Set when this row is a blocker that inherited a star: the starred
    /// issue it inherited from. Cleared when the blocker no longer blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherited_from: Option<u32>,
    /// The starred-at this row sorts by (its own, or the inheriting star's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_priority_at: Option<String>,
    /// When forward progress was last seen (label change, PR update,
    /// checkpoint write).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_progress_at: Option<DateTime<Utc>>,
}

/// A loom-ui star intent this host dropped instead of applying.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroppedStarIntent {
    pub id: String,
    pub repo: String,
    pub number: u32,
    /// Why: `unmanaged-repo`, `wrong-label`, `missing-requested-by`,
    /// `bad-action`, `malformed`.
    pub reason: String,
}

/// The last liveness pass, as published by the daemon.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StarLivenessReport {
    /// When the pass completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<DateTime<Utc>>,
    /// One row per starred issue and per inheriting blocker, starred first,
    /// in starred-at order.
    #[serde(default)]
    pub rows: Vec<StarLandingRow>,
    /// Escalations this host posted during the pass (the rest were already
    /// on the forge, posted earlier or by another host).
    #[serde(default)]
    pub escalations_posted: usize,
    /// Intents dropped by validation in recent passes (bounded).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped_intents: Vec<DroppedStarIntent>,
    /// Repos whose facts could not be read this pass (their rows are stale or
    /// missing).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_repos: Vec<String>,
}

impl StarLivenessReport {
    /// Rows escalated to the operator: a `needs-operator` stage, or an
    /// agent-owned stage the no-progress watchdog escalated (which keeps its
    /// stage and gains an ask).
    pub fn needs_operator(&self) -> impl Iterator<Item = &StarLandingRow> {
        self.rows.iter().filter(|r| r.ask.is_some())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn wire_names_match_serde() {
        for s in [
            LandingStage::Curating,
            LandingStage::Ready,
            LandingStage::Building,
            LandingStage::InReview,
            LandingStage::ChangesRequested,
            LandingStage::Mergeable,
            LandingStage::Merging,
            LandingStage::BlockedBy,
            LandingStage::NeedsOperator,
            LandingStage::NoCapacity,
        ] {
            assert_eq!(serde_json::to_value(s).unwrap(), serde_json::json!(s.as_str()));
        }
        for k in [
            AskKind::OperatorOnly,
            AskKind::OperatorDecision,
            AskKind::MergeRiskHold,
            AskKind::MergeRefused,
            AskKind::PoolsExhausted,
            AskKind::UnmanagedRepo,
            AskKind::BlockedUnnamed,
            AskKind::NoProgress,
        ] {
            assert_eq!(serde_json::to_value(k).unwrap(), serde_json::json!(k.as_str()));
        }
    }

    #[test]
    fn unknown_values_parse_and_old_payloads_default() {
        let row: StarLandingRow = serde_json::from_value(serde_json::json!({
            "repo": "o/r", "issue": 1, "stage": "some-future-stage", "next_actor": "x"
        }))
        .unwrap();
        assert_eq!(row.stage, LandingStage::Unknown);
        assert_eq!(row.time_in_stage_secs, 0);
        let report: StarLivenessReport = serde_json::from_str("{}").unwrap();
        assert!(report.rows.is_empty());
    }
}
