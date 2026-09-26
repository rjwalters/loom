//! Forge-side `loom:blocked` rows for `queue.snapshot` (Issue #8957).
//!
//! The work finder lists only `loom:issue` items, so an issue that carries
//! `loom:blocked` **without** `loom:issue` never reaches the ready queue and
//! the dashboard had no row for it. This module adds those issues to the
//! snapshot from the daemon side:
//!
//! - **Cost.** One ETag-cached REST listing per managed repo
//!   ([`crate::forge_listing::list_issues_cached`]; an unchanged listing is a
//!   free `304`), made only when a snapshot is actually emitted, so at most
//!   once per repo per `host.health` interval.
//! - **Anti-leak.** Rows go through the same slug and `derive_visibility`
//!   resolution as ready rows and carry their own `visibility`. The reason is
//!   the fixed [`QueueDisposition::reason`] text. `detail` is at most the
//!   allowlisted hold labels in [`HOLD_LABELS`]; comment text (where a
//!   blocking reason is usually written) is never read, let alone exported.
//! - **Order.** The rows are not in dispatch order, so their `rank` is `0`.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::forge_listing::RestIssue;
use crate::telemetry::queue_snapshot::{QueueRepoRef, QueueSnapshotRow, MAX_ROWS};
use crate::telemetry::QueueSnapshotRecord;
use crate::types::QueueDisposition;
use crate::workspace_pool::WorkspacePool;

/// The label this module lists.
pub const BLOCKED_LABEL: &str = "loom:blocked";

/// Co-labels that explain a hold and may be exported as `detail`. Label
/// names only, never free text.
pub const HOLD_LABELS: &[&str] = &[
    "loom:operator",
    "loom:operator-only",
    "loom:operator-mechanical",
    "loom:needs-capability",
];

/// `rank` for rows outside the dispatch order.
pub const UNRANKED: usize = 0;

/// The snapshot rows for one repo's open `loom:blocked` listing: issues only
/// (REST listings include PRs), minus those that also carry `loom:issue`
/// (the work finder already lists those). Pure.
#[must_use]
pub fn blocked_rows(repo: &QueueRepoRef, listing: &[RestIssue]) -> Vec<QueueSnapshotRow> {
    listing
        .iter()
        .filter(|item| !item.is_pull_request)
        .filter(|item| item.labels.iter().any(|l| l == BLOCKED_LABEL))
        .filter(|item| !item.labels.iter().any(|l| l == "loom:issue"))
        .map(|item| {
            let disposition = QueueDisposition::LabelledBlocked;
            let holds: Vec<&str> = HOLD_LABELS
                .iter()
                .copied()
                .filter(|hold| item.labels.iter().any(|l| l == hold))
                .collect();
            QueueSnapshotRow {
                rank: UNRANKED,
                repo: repo.repo.clone(),
                visibility: repo.visibility,
                issue: item.number,
                workspace_priority: crate::workspace_registry::DEFAULT_WORKSPACE_PRIORITY,
                urgent: item
                    .labels
                    .iter()
                    .any(|l| l == crate::work_finder::URGENT_LABEL),
                created_at: item.created_at.clone(),
                tier: item.labels.iter().find(|l| l.starts_with("tier:")).cloned(),
                disposition,
                state: disposition.state().to_string(),
                reason: disposition.reason().to_string(),
                detail: (!holds.is_empty()).then(|| holds.join(", ")),
            }
        })
        .collect()
}

/// Append `rows` to `record` after its ranked rows. Each counts in
/// `counts.blocked`; rows already present (same repo and issue) are skipped,
/// and rows past [`MAX_ROWS`] are counted in `rows_truncated`.
pub fn append(record: &mut QueueSnapshotRecord, rows: Vec<QueueSnapshotRow>) {
    let mut seen: BTreeSet<(String, u32)> = record
        .rows
        .iter()
        .map(|r| (r.repo.clone(), r.issue))
        .collect();
    for row in rows {
        if !seen.insert((row.repo.clone(), row.issue)) {
            continue;
        }
        record.counts.blocked += 1;
        if record.rows.len() >= MAX_ROWS {
            record.rows_truncated += 1;
        } else {
            record.rows.push(row);
        }
    }
}

/// List `loom:blocked` for every managed repo and return the rows. A repo
/// whose slug cannot be resolved, or whose listing fails, contributes nothing
/// (logged at debug; the ready rows are unaffected).
pub(super) async fn collect(
    workspace_pool: &WorkspacePool,
    slug_cache: &mut HashMap<String, String>,
) -> Vec<QueueSnapshotRow> {
    let mut rows = Vec::new();
    let mut listed: BTreeSet<String> = BTreeSet::new();
    for root in super::collector::provisioned_roots(workspace_pool) {
        let root_str = root.to_string_lossy().to_string();
        let Some(slug) = super::collector::resolve_repo_slug_cached(slug_cache, &root_str).await
        else {
            continue;
        };
        if !listed.insert(slug.clone()) {
            continue;
        }
        let Some(listing) = list_open(root, BLOCKED_LABEL).await else {
            continue;
        };
        let repo = QueueRepoRef {
            visibility: super::collector::resolve_visibility(&slug).await,
            repo: slug,
        };
        rows.extend(blocked_rows(&repo, &listing));
    }
    rows
}

/// One ETag-cached listing of open items carrying `label` in the repo at
/// `root`, off the async runtime. `None` on failure.
pub(super) async fn list_open(root: PathBuf, label: &'static str) -> Option<Vec<RestIssue>> {
    let shown = root.display().to_string();
    let result = tokio::task::spawn_blocking(move || {
        crate::forge_listing::list_issues_cached(Path::new("gh"), Some(&root), None, label, "open")
    })
    .await;
    match result {
        Ok(Ok(listing)) => Some(listing),
        Ok(Err(error)) => {
            log::debug!("observability: listing {label} in {shown} failed: {error}");
            None
        }
        Err(join_error) => {
            log::debug!("observability: listing {label} in {shown} panicked: {join_error}");
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "queue_blocked_tests.rs"]
mod tests;
