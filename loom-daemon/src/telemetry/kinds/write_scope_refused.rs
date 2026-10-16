//! `write_scope.refused` (#12168): one daemon-pass write the #9548
//! credential-scope gate refused.
//!
//! # Why this kind exists
//!
//! On 2026-10-13 the answer to "are Loom writes being refused for credential
//! scope, against which repos, and since when?" came only from grepping each
//! worker's `daemon.log` for the `refusing the write (#9548)` WARN. The OTLP
//! pipeline carries typed records, not the daemon log, so SigNoz could not
//! count them. [`gate`](crate::write_scope) emits this record at its `Deny`
//! branch: one per refused write, **not** deduplicated like the WARN (the
//! point is a count).
//!
//! # Scope
//!
//! Daemon passes only. Short-lived `loom-daemon forge ...` CLI processes have
//! no OTLP sink, so a refusal there is not recorded.
//!
//! **OTLP only.** The scalars ride as `loom.write_scope.*` attributes
//! ([`WRITE_SCOPE_REFUSED_LOG_ATTRIBUTE_KEYS`], which the collector's log
//! `keep_keys` must list; contract-tested). The body is the record's JSON.
//! The credential field is a non-secret identity, never a token or any
//! fragment of one. **Provenance is required.**

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::provenance::Provenance;

/// Every log attribute key this kind exports besides `loom.kind`,
/// `loom.record_id` and `loom.repo` (already kept). The collector's
/// `transform/privacy` log `keep_keys` must list each.
pub const WRITE_SCOPE_REFUSED_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.write_scope.reason",
    "loom.write_scope.credential",
    "loom.write_scope.call_site",
    "loom.write_scope.version",
    "loom.write_scope.revision",
    "loom.write_scope.provenance_complete",
];

/// Why the gate refused: a closed set, so free text stays out of attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RefusalClass {
    /// The repository is outside the credential's App installation.
    NotInInstallation,
    /// The credential has a role below WRITE on the repository.
    InsufficientPermission,
    /// The permission probe could not answer; refused rather than guessed.
    Unverifiable,
    /// The repository is not the origin of a registered workspace.
    UnmanagedRepo,
    /// `gh` resolves the checkout to a repository other than its origin.
    TargetMismatch,
    /// No target repository: no GitHub remote, or an unparseable `OWNER/REPO`.
    NoTarget,
}

impl RefusalClass {
    /// The wire spelling (also the attribute value).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotInInstallation => "not-in-installation",
            Self::InsufficientPermission => "insufficient-permission",
            Self::Unverifiable => "unverifiable",
            Self::UnmanagedRepo => "unmanaged-repo",
            Self::TargetMismatch => "target-mismatch",
            Self::NoTarget => "no-target",
        }
    }
}

/// The `write_scope.refused` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteScopeRefusedRecord {
    /// The refused `owner/repo`, when the gate knew it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// A non-secret identity for the credential that was refused: its kind
    /// (`gh-process-default`, `gitea`) or its registered per-owner config
    /// directory name (`gh-config:<dir>`). Never a token or a fragment.
    pub credential: String,
    /// The `what` the pass handed the gate (a short pass name).
    pub call_site: String,
    /// Why it was refused.
    pub reason: RefusalClass,
    /// When the gate refused.
    pub refused_at: DateTime<Utc>,
    /// The emitting build.
    pub loom: Provenance,
}
