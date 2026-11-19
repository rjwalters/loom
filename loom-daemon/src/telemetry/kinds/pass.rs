//! `pass.verdict` (#10752): one artifact's verdict in a pass over forge
//! artifacts: repo#n, the verdict and its reason, the blockers with the state
//! the pass read, and the labels it changed. `role` is empty for a daemon
//! pass. A role's verdicts (Guide's unblock outcomes, Champion's promotion
//! verdicts) fit the same record with `role` set and their own `mechanism`,
//! `verdict` and `reason` vocabulary. `pass.summary` (the per-pass rollup) was
//! retired in #12094: nothing emitted it after the `loom:blocked` release pass
//! went (#12108).
//!
//! `mechanism` is the pass's `forge_call_stats` caller.
//!
//! **OTLP only**, like the other post-#8921 log kinds. The body is the
//! record's JSON; the scalars ride as `loom.pass.*` attributes plus the shared
//! `loom.repo` / `loom.role` ([`PASS_LOG_ATTRIBUTE_KEYS`], which the
//! collector's log `keep_keys` must list; contract-tested).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::provenance::Provenance;

/// Every `loom.pass.*` log attribute key the kind exports. The
/// collector's `transform/privacy` log `keep_keys` must list each one
/// (`defaults/observability/collector/config.yaml`, contract-tested).
/// `loom.repo` and `loom.role` are shared keys already on that list.
pub const PASS_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.pass.id",
    "loom.pass.mechanism",
    "loom.pass.mode",
    "loom.pass.version",
    "loom.pass.revision",
    "loom.pass.number",
    "loom.pass.artifact",
    "loom.pass.verdict",
    "loom.pass.reason",
    "loom.pass.blockers",
    "loom.pass.labels_added",
    "loom.pass.labels_removed",
    "loom.pass.applied",
    // #12027: a role step's verdict ([`StepFacts`]).
    "loom.pass.head_sha",
    "loom.pass.step",
    "loom.pass.class",
    "loom.pass.ledger_class",
    "loom.pass.ledger_reason",
    "loom.pass.fact_id",
    "loom.pass.first_refused_at",
    "loom.pass.criterion",
    "loom.pass.reason_set",
    "loom.pass.proposal_label",
    "loom.pass.body_hash",
    // #12715: the step's raw reason token.
    "loom.pass.reason_detail",
    "loom.pass.unevaluated_reason",
    "loom.pass.budget.bucket",
    "loom.pass.budget.remaining",
    "loom.pass.budget.floor",
];

/// `repo` value when no forge slug is known. Never a local path (#9442).
pub const REPO_UNRESOLVED: &str = "repo_unresolved";

/// Whether the pass wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassMode {
    /// Writes were made.
    On,
    /// Planned and reported only; nothing written.
    DryRun,
}

impl PassMode {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::DryRun => "dry_run",
        }
    }
}

/// The budget reading that stopped one artifact (#12036).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassBudgetReading {
    /// `core` or `graphql`.
    pub bucket: String,
    pub remaining: u64,
    pub floor: u64,
}

/// One blocker of an artifact, with the state the pass read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockerState {
    /// `#12`, or `owner/repo#12` for a cross-repo blocker.
    #[serde(rename = "ref")]
    pub reference: String,
    /// `open`, `closed`, `merged`, `closed_unmerged` (a PR closed without
    /// merging), `unread` (the read failed) or `not_read` (this pass did not
    /// read it).
    pub state: String,
}

/// One artifact's verdict in one pass (or one role run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassVerdictRecord {
    /// The `pass_id` of the pass that decided it.
    pub pass_id: String,
    /// The pass: its `forge_call_stats` caller.
    pub mechanism: String,
    /// The role that decided it; `None` for a daemon pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// `owner/repo`, or [`REPO_UNRESOLVED`].
    pub repo: String,
    pub number: u64,
    /// `issue` or `pr`.
    pub artifact: String,
    /// The mechanism's verdict (`released`, `reparked`, `still_blocked`,
    /// `skipped`, `unevaluated`, `failed` for the release pass).
    pub verdict: String,
    /// The closed-set reason (a skip reason), when the verdict has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Free-text detail (why an artifact went unevaluated or a write
    /// failed), sanitized and truncated. Body only, never an attribute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Why an artifact went unevaluated (`budget_floor_core`,
    /// `budget_floor_graphql`, `read_failed`, `other`) (#12036).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unevaluated_reason: Option<String>,
    /// The budget reading behind a floor stop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<PassBudgetReading>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<BlockerState>,
    /// Labels the verdict adds (planned, under dry-run).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels_added: Vec<String>,
    /// Labels the verdict removes (planned, under dry-run).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels_removed: Vec<String>,
    pub mode: PassMode,
    /// Every write of the verdict landed.
    pub applied: bool,
    /// When it was decided; the record's time.
    pub at: DateTime<Utc>,
    /// A role step's facts (#12027); all absent for a daemon pass, so a
    /// `stale_blocked_release` record serializes exactly as before.
    #[serde(flatten, default)]
    pub step: StepFacts,
}

/// What a role step's verdict adds to [`PassVerdictRecord`] (#12027,
/// `observability::step_verdict`). Every field is optional and skipped when
/// empty. Only what was knowable at `evaluated_at` is ever filled.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepFacts {
    /// The step's sweep id (#12715), so a verdict joins to its session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sweep_id: Option<String>,
    /// The head the role evaluated, when the run recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    /// `champion_pr`, `champion_issue`, …: the role plus its mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// `verdict | noop | refused | failed`, the projection's class (the
    /// record's `verdict` carries the same value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    /// The closed reason set the projection's `reason` comes from
    /// (`champion_merge_v2`, `champion_promotion_v1`, `champion_triage_v1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_set: Option<String>,
    /// The outcome ledger's own classification of the step
    /// (`step_dispatch::outcome_ledger::classify`), computed record-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger_reason: Option<String>,
    /// The item's labels as the run read them, before it acted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_labels: Vec<String>,
    /// When the decision was knowable (the step's end, or the journal
    /// entry's time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluated_at: Option<DateTime<Utc>>,
    /// When this refusal identity was first seen on this head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_refused_at: Option<DateTime<Utc>>,
    /// The raw marker tokens (`champion:reject:<criterion_key>:<reason_key>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criterion_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_key: Option<String>,
    /// The proposal label the issue carried (`loom:curated`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposal_label: Option<String>,
    /// The `proposal-verdict:body-<hash>` the verdict named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_hash: Option<String>,
    /// Derived, never random, so a duplicate from another host dedups on
    /// read: `derived_hex(["loom.pass.verdict", mechanism, repo, number,
    /// head_sha, criterion_key, reason_key, evaluated_at hour], 32)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fact_id: Option<String>,
    /// The deciding build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loom: Option<Provenance>,
}

#[cfg(test)]
#[path = "pass_tests.rs"]
mod tests;
