//! Repository reference types shared by the OTLP queue and eligibility
//! emitters (Issue #8852, trimmed by #12813 when `queue.snapshot` was removed).
//!
//! Anti-leak rules, enforced where records are built:
//! - `repo` is the forge `owner/repo` slug and never a local path. A row whose
//!   slug cannot be resolved is dropped and counted.
//! - Every reference carries its own [`RepoVisibility`]. A missing or unknown
//!   tag decodes to `Private`.
//! - `detail` is kept only for the structured dispositions (the park label and
//!   the open PR number). Free-form dispatch-error text is never exported.

use serde::{Deserialize, Serialize};

use super::RepoVisibility;
use crate::types::QueueDisposition;

/// A repository named by a queue record, with its visibility tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueRepoRef {
    /// Forge `owner/repo`.
    pub repo: String,
    #[serde(default)]
    pub visibility: RepoVisibility,
}

/// `detail` survives only for dispositions whose detail is structured: the
/// park label, the open PR, a `labelled_blocked` row's allowlisted hold
/// labels (#8957), a `deferred_file_overlap` row's shared `## Affected Files`
/// paths (#9781; the daemon formats `file overlap: <paths>`, never forge free
/// text), and a `workspace_halted` row's closed-vocabulary hold
/// cause (#9017 — `main_red`, `gate_pending`, `token_pool`, …, emitted by
/// `work_finder::halt_cause`, never free-form text), and an `already_landed`
/// row's evidence (#11429: `forge_closed` or `journal_landed`).
#[must_use]
pub fn exportable_detail(disposition: QueueDisposition, detail: Option<&str>) -> Option<String> {
    match disposition {
        QueueDisposition::Parked
        | QueueDisposition::OpenPr
        | QueueDisposition::LabelledBlocked
        | QueueDisposition::DeferredFileOverlap
        | QueueDisposition::WorkspaceHalted
        | QueueDisposition::AlreadyLanded => detail.map(str::to_string),
        _ => None,
    }
}
