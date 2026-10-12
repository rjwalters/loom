//! `checkout.external_change` (#11566): a managed checkout's history was moved
//! by something other than Loom.
//!
//! # Why this kind exists
//!
//! On 2026-10-12 at 00:44:39, 00:44:57 and 00:45:16 three `reset` entries
//! appeared in a managed checkout's reflog that no Loom code path wrote. They
//! discarded unpushed commits, and the only evidence was a `git reflog` read by
//! hand hours later. Loom's own git operations carry
//! `GIT_REFLOG_ACTION=loom:<op>` ([`loom_reflog_action`]); the `checkout_ff`
//! pass compares each managed checkout's reflog with what it last saw and emits
//! this record for every history-moving entry that is **not** so tagged.
//!
//! # Facts only
//!
//! The record names what the reflog says and what became unreachable. It does
//! not guess who did it, and it never repairs anything. A reflog the pass cannot
//! compare (first sight, expired, rewritten) is seeded silently: unknown is not
//! a change.
//!
//! **OTLP only.** The scalars ride as `loom.checkout.*` attributes
//! ([`CHECKOUT_EXTERNAL_CHANGE_LOG_ATTRIBUTE_KEYS`], which the collector's log
//! `keep_keys` must list; contract-tested). The body is the record's JSON,
//! which carries the discarded subjects. **Provenance is required.**

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::provenance::Provenance;
use crate::telemetry::RepoVisibility;

/// The namespace of every Loom-initiated git operation's reflog action
/// (`GIT_REFLOG_ACTION=loom:<op>`, see [`loom_reflog_action`]). Kept apart
/// from [`REFLOG_ACTION_SEPARATOR`] so no bare label-shaped `loom:` literal
/// exists here: this is a reflog namespace, not a label (#10013 guard).
pub const LOOM_REFLOG_NAMESPACE: &str = "loom";

/// Separates [`LOOM_REFLOG_NAMESPACE`] from the operation name.
pub const REFLOG_ACTION_SEPARATOR: char = ':';

/// The `GIT_REFLOG_ACTION` value for a Loom git operation `op` (`loom:<op>`).
#[must_use]
pub fn loom_reflog_action(op: &str) -> String {
    format!("{LOOM_REFLOG_NAMESPACE}{REFLOG_ACTION_SEPARATOR}{op}")
}

/// Whether a reflog message was written by a Loom git operation: it starts
/// with `loom:`. Such an entry never produces this record.
#[must_use]
pub fn is_loom_reflog_message(message: &str) -> bool {
    message
        .strip_prefix(LOOM_REFLOG_NAMESPACE)
        .is_some_and(|rest| rest.starts_with(REFLOG_ACTION_SEPARATOR))
}

/// Every log attribute key `checkout.external_change` exports.
pub const CHECKOUT_EXTERNAL_CHANGE_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.checkout.change_id",
    "loom.checkout.kind",
    "loom.checkout.ref",
    "loom.checkout.from_sha",
    "loom.checkout.to_sha",
    "loom.checkout.discarded_count",
    "loom.checkout.hold_active",
    "loom.checkout.version",
    "loom.checkout.revision",
    "loom.checkout.tree_state",
    "loom.checkout.provenance_complete",
];

/// How many discarded-commit subjects a record names.
pub const MAX_DISCARDED_SUBJECTS: usize = 20;

/// Commits that were reachable from the old tip and are now reachable from no
/// ref.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscardedCommits {
    /// How many commits are no longer reachable from any ref.
    pub count: u64,
    /// Their subjects, newest first, at most [`MAX_DISCARDED_SUBJECTS`].
    pub subjects: Vec<String>,
}

/// The `checkout.external_change` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutExternalChangeRecord {
    /// Stable id: a hash of root, ref, from, to and the reflog time. A reflog
    /// entry seen twice is the same id. The hold-clear note cites it.
    pub change_id: String,
    /// `owner/repo` from the checkout's `origin`, when it parses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Private until resolved: a missing tag never decodes to public.
    #[serde(default)]
    pub visibility: RepoVisibility,
    /// The registered root.
    pub root: String,
    /// The host whose checkout it is (and whose reflog was read).
    pub host: String,
    /// `reset`, `checkout` or `rebase`. Not `kind`: that is the envelope's
    /// record-kind tag, and a payload field of the same name would collide.
    pub move_kind: String,
    /// The ref whose log carried the entry (`HEAD` or `refs/heads/<branch>`).
    #[serde(rename = "ref")]
    pub ref_name: String,
    /// The reflog message, as git wrote it.
    pub reflog_line: String,
    /// The commit before the move.
    pub from_sha: String,
    /// The commit after the move.
    pub to_sha: String,
    /// When git wrote the entry.
    pub at: DateTime<Utc>,
    /// When this daemon read it.
    pub observed_at: DateTime<Utc>,
    /// What the move made unreachable. `None`: unknown (the old tip is gone,
    /// or git could not answer), never "nothing".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discarded_commits: Option<DiscardedCommits>,
    /// A dispatch hold stood on this checkout when the pass read the entry.
    /// The hold's clear cites [`Self::change_id`].
    pub hold_active: bool,
    /// The observing build.
    pub loom: Provenance,
}

impl CheckoutExternalChangeRecord {
    /// Whether the record carries valid provenance.
    #[must_use]
    pub fn has_provenance(&self) -> bool {
        self.loom.is_valid()
    }
}
