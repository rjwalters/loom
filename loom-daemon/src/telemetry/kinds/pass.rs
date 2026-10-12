//! `pass.summary` and `pass.verdict` (#10752): what a pass over forge
//! artifacts did, once per pass and once per artifact it decided.
//!
//! Before these kinds, the `loom:blocked` release pass (#10556) left one local
//! `log::info!` line per pass and nothing in SigNoz, so "how often did it run,
//! what did it release, what did it skip and why" had no answer. The two
//! records are shaped for any pass that decides per artifact, not only that
//! one:
//!
//! - **`pass.summary`**, one per pass per workspace: the `mechanism`, repo,
//!   host, deciding build, mode, outcome, counts by verdict and by skip reason,
//!   whether the write cap was hit, the duration and the GitHub calls the pass
//!   made (its [`crate::gh_invocation::caller_scope`]).
//! - **`pass.verdict`**, one per artifact: repo#n, the verdict and its reason,
//!   the blockers with the state the pass read, and the labels it changed.
//!   `role` is empty for a daemon pass. A role's verdicts (Guide's unblock
//!   outcomes, Champion's promotion verdicts) fit the same record with `role`
//!   set and their own `mechanism`, `verdict` and `reason` vocabulary.
//!
//! `mechanism` is the name the pass's GitHub calls carry as `github.caller`
//! on their `invoke github` spans (and its `forge_call_stats` caller), so a
//! pass, its verdicts and its forge cost join on one string.
//!
//! **OTLP only**, like the other post-#8921 log kinds. The body is the
//! record's JSON; the scalars ride as `loom.pass.*` attributes plus the shared
//! `loom.repo` / `loom.role` ([`PASS_LOG_ATTRIBUTE_KEYS`], which the
//! collector's log `keep_keys` must list; contract-tested).

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::provenance::Provenance;

/// Every `loom.pass.*` log attribute key the two kinds export. The
/// collector's `transform/privacy` log `keep_keys` must list each one
/// (`defaults/observability/collector/config.yaml`, contract-tested).
/// `loom.repo` and `loom.role` are shared keys already on that list.
pub const PASS_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.pass.id",
    "loom.pass.mechanism",
    "loom.pass.mode",
    "loom.pass.outcome",
    "loom.pass.examined",
    "loom.pass.verdicts",
    "loom.pass.skip_reasons",
    "loom.pass.skipped",
    "loom.pass.write_cap_hit",
    "loom.pass.duration_ms",
    "loom.pass.github_calls",
    "loom.pass.github_writes",
    "loom.pass.github_not_modified",
    "loom.pass.verdicts_emitted",
    "loom.pass.verdicts_unchanged",
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

/// How the pass as a whole ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassOutcome {
    /// It listed the artifacts and decided each one.
    Completed,
    /// The repository is archived; nothing was evaluated.
    Archived,
    /// Something refused the pass before any artifact was decided (the
    /// listing failed, the rate-limit breaker, the forge-write scope);
    /// `refusal` says what.
    Refused,
}

impl PassOutcome {
    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Archived => "archived",
            Self::Refused => "refused",
        }
    }
}

/// The GitHub executions made inside the pass's caller scope.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubSpend {
    /// `gh` executions (each one an `invoke github` span).
    pub calls: u64,
    /// Of those, writes.
    pub writes: u64,
    /// Of those, free `304 Not Modified` answers.
    pub not_modified: u64,
}

/// One pass over one workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassSummaryRecord {
    /// Derived, never random: `derived_hex(["loom.pass", mechanism, host,
    /// repo, started_at])`. Every `pass.verdict` of the pass carries it.
    pub pass_id: String,
    /// The pass: its `forge_call_stats` caller and its spans' `github.caller`
    /// (`stale_blocked_release`).
    pub mechanism: String,
    /// `owner/repo`, or [`REPO_UNRESOLVED`].
    pub repo: String,
    /// The host that ran it.
    pub host: String,
    pub mode: PassMode,
    pub outcome: PassOutcome,
    /// Why the pass was refused, truncated (only for [`PassOutcome::Refused`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    pub started_at: DateTime<Utc>,
    /// When it finished; the record's time.
    pub ended_at: DateTime<Utc>,
    pub duration_ms: u64,
    /// Artifacts listed.
    pub examined: u64,
    /// Artifacts per verdict, every verdict of the mechanism's vocabulary
    /// present (zeros included).
    pub verdicts: BTreeMap<String, u64>,
    /// Skipped artifacts per reason.
    pub skipped: BTreeMap<String, u64>,
    /// The per-pass write cap left work for the next pass.
    pub write_cap_hit: bool,
    pub github: GithubSpend,
    /// `pass.verdict` records emitted for this pass.
    pub verdicts_emitted: u64,
    /// Artifacts whose verdict was unchanged since one already emitted
    /// within the heartbeat, so no `pass.verdict` was emitted this pass.
    pub verdicts_unchanged: u64,
    /// The deciding build.
    pub loom: Provenance,
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
    /// The [`PassSummaryRecord::pass_id`] of the pass that decided it.
    pub pass_id: String,
    /// See [`PassSummaryRecord::mechanism`].
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
}

#[cfg(test)]
#[path = "pass_tests.rs"]
mod tests;
