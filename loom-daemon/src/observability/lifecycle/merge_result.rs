//! The terminal result of a sweep's merge phase, recorded at the
//! `merge-pr.sh` chokepoint (#12029).
//!
//! `sweep-checkpoint begin N merge --pr-number P` opens the merge attempt, but
//! the merge phase never writes a `merge-done` checkpoint (it deletes the
//! checkpoint), so the attempt used to stay open until `finish_execution`
//! closed it as `exit_unobserved`. `merge-pr.sh` now calls
//! `loom-daemon merge-pr record-result` once on each terminal path; this closes
//! the open merge attempt (and its phase) with the PR, head and result.
//!
//! Telemetry only and best effort: nothing is closed when no merge attempt is
//! open (Champion's and attended merges), the checkpoint file is never read or
//! written, and no forge is consulted.

use chrono::Utc;

use super::{
    attributes, checkpoint_workspace, inherited_context, insert_worked, SpanName, SpanStatus,
};
use std::path::Path;

/// `loom.result` values of a merge attempt.
pub const RESULTS: [&str; 3] = ["merged", "refused", "failed"];

/// What `merge-pr.sh` observed at one terminal path.
#[derive(Debug, Clone, Copy)]
pub struct MergeResult<'a> {
    pub pr: u32,
    pub head_sha: Option<&'a str>,
    /// One of [`RESULTS`].
    pub result: &'a str,
    /// A `merge_pr::response::MergeResponseKind` token, when the failure had one.
    pub reason: Option<&'a str>,
    pub exit_code: Option<i32>,
}

/// Name the PR on a begun attempt (`begin … --pr-number`), when given.
pub(super) fn stamp_pr(metadata: &mut crate::telemetry::trace::TraceAttributes, pr: Option<u32>) {
    if let Some(pr) = pr {
        metadata.insert("loom.pr_number".into(), pr.to_string());
    }
}

/// Close the open merge attempt of the inherited trace. Returns whether one
/// was closed.
pub fn record(root: &Path, merge: &MergeResult<'_>) -> bool {
    if !RESULTS.contains(&merge.result) {
        return false;
    }
    let primary = checkpoint_workspace(root);
    if !crate::observability::tracing::enabled(primary.as_path()) {
        return false;
    }
    let Some((journal, root_context, _)) = inherited_context(primary.as_path()) else {
        return false;
    };
    let Ok(active) = journal.active() else {
        return false;
    };
    let pr = merge.pr.to_string();
    // The newest open merge attempt of this execution that names this PR (or,
    // begun without one, none): another PR's attempt is never closed.
    let Some(attempt) = active
        .iter()
        .filter(|a| {
            a.record.name == SpanName::RoleAttempt
                && a.record
                    .attributes
                    .get("loom.role")
                    .is_some_and(|r| r == "merge")
                && a.record
                    .attributes
                    .get("loom.pr_number")
                    .is_none_or(|p| *p == pr)
                && a.record.context.trace_id == root_context.trace_id
        })
        .max_by_key(|a| a.record.started_at)
    else {
        return false;
    };
    let mut close = attributes(&[("loom.result", merge.result), ("loom.pr_number", &pr)]);
    if let Some(head) = merge.head_sha.filter(|h| !h.is_empty()) {
        close.insert("loom.head_sha".into(), head.to_ascii_lowercase());
    }
    if let Some(reason) = merge.reason.filter(|r| !r.is_empty()) {
        close.insert("loom.merge.reason".into(), reason.to_string());
    }
    if let Some(code) = merge.exit_code {
        close.insert("loom.exit_code".into(), code.to_string());
    }
    insert_worked(&mut close, Some(true));
    // A held or re-queued merge is not a failure (`refused` is Unset).
    let status = match merge.result {
        "merged" => SpanStatus::Ok,
        "failed" => SpanStatus::Error,
        _ => SpanStatus::Unset,
    };
    let at = Utc::now();
    let _ = journal.finish(attempt, at, status, close.clone());
    if let Some(phase) = active.iter().find(|p| {
        p.record.name == SpanName::Phase
            && attempt.record.parent_span_id.as_ref() == Some(&p.record.context.span_id)
    }) {
        let _ = journal.finish(phase, at, status, close);
    }
    true
}

#[cfg(test)]
#[path = "merge_result/tests.rs"]
mod tests;
