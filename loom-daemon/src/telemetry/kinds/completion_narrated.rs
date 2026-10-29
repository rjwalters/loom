//! `completion.narrated` (#13287): one public-feed `completion` envelope the
//! safehouse narration sink built, with the trigger that found the merge and
//! whether the token figures made it into the payload.
//!
//! # Why this kind exists
//!
//! `loom.feed.completions{kind,outcome}` counts narrations but carries no repo,
//! issue or token facts, so a regression that silently dropped `tokens` from
//! most completions (2026-10-16 onward) was only visible from the consumer's
//! database. This record answers "how many completions did each host narrate
//! per repo, and how many carried tokens?" from the store.
//!
//! **OTLP only.** The scalars ride as `loom.completion.*` attributes
//! ([`COMPLETION_NARRATED_LOG_ATTRIBUTE_KEYS`], which the collector's log
//! `keep_keys` must list; contract-tested), plus `loom.repo` and `loom.issue`.
//! The body is the record's JSON. **Provenance is required.**

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::provenance::Provenance;

/// Every log attribute key this kind exports besides `loom.kind`,
/// `loom.record_id`, `loom.repo` and `loom.issue` (already kept). The
/// collector's `transform/privacy` log `keep_keys` must list each.
pub const COMPLETION_NARRATED_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.completion.trigger",
    "loom.completion.outcome",
    "loom.completion.pr",
    "loom.completion.tokens_present",
    "loom.completion.tokens_by_model_rows",
];

/// The `completion.narrated` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionNarratedRecord {
    /// The repository's `owner/repo`.
    pub repo: String,
    /// The issue the completion narrates.
    pub issue: u32,
    /// The merged PR.
    pub pr: u32,
    /// What found the merge: `sweep_exit`, `champion_merge_step` or `reconcile`.
    pub trigger: String,
    /// `narrated`, or `seeded` for a fresh host's first reconciliation pass,
    /// which records merges without posting them (#4649).
    pub outcome: String,
    /// Whether the payload carries a flat `tokens` figure.
    pub tokens_present: bool,
    /// How many `tokens_by_model` rows the payload carries.
    pub tokens_by_model_rows: u32,
    /// When the completion was built.
    pub narrated_at: DateTime<Utc>,
    /// The emitting build.
    pub loom: Provenance,
}
