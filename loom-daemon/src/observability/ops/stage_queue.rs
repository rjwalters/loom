//! Queue-depth states of `loom.forge.stage_items` that no single stage-label
//! listing can answer (Issue #11847): triage depth, issues with no Loom label
//! at all, curated issues not yet promoted, the ready queue split by whether
//! an open PR already closes the issue, held PRs and parked issues.
//!
//! # One walk per repo
//!
//! "No `loom:` label" cannot be asked of the forge by label, and the triage
//! backlog runs past one 100-item page, so these states come from **one**
//! unfiltered walk of the repo's open issues and PRs
//! ([`crate::forge_listing::list_open_issues_cached_all_as`]): ETag-cached per
//! page (an unchanged page is a free `304`), a deferrable read that is shed
//! rather than retried when the readers are out of budget, and the same URLs
//! the intake pass walks, so on a host running both the second walk is mostly
//! `304`s. The open-PR split needs no extra read either: the walk already
//! carries every open PR and its body, and an issue counts as `issue_open_pr`
//! when an open PR's body closes it (`Closes #N`, same-repo only; see
//! [`super::stage_dwell::closing_refs`]).
//!
//! A walk that is shed, fails, or does not finish yields no counts, so the
//! repo emits none of these states for that sample: absent, never a `0` that
//! was not measured. The per-label states (`curated`, `issue`, …) keep their
//! own first-page listings and are unaffected.
//!
//! # States (issues unless noted; states overlap, they are not a partition)
//!
//! | `state` | counts |
//! |---|---|
//! | `triage` | issues carrying `loom:triage` |
//! | `unlabeled` | issues carrying no `loom:` label |
//! | `curated_not_promoted` | `loom:curated` without `loom:issue` or `loom:building` |
//! | `issue_open_pr` | `loom:issue` with an open PR closing it |
//! | `issue_no_pr` | `loom:issue` with no open PR closing it |
//! | `parked` | issues carrying a park label (registry `park` property) |
//! | `pr_held` | **PRs** carrying a hold label (registry `hold` property), e.g. an approved `loom:pr` under `loom:operator` |

use std::collections::{BTreeMap, BTreeSet};

use crate::forge_listing::RestIssue;

pub const TRIAGE: &str = "loom:triage";

/// Every state this module counts, in export order.
pub const QUEUE_STATES: [&str; 7] = [
    "triage",
    "unlabeled",
    "curated_not_promoted",
    "issue_open_pr",
    "issue_no_pr",
    "parked",
    "pr_held",
];

fn has(item: &RestIssue, label: &str) -> bool {
    item.labels.iter().any(|l| l == label)
}

fn has_any(item: &RestIssue, set: &[&str]) -> bool {
    item.labels.iter().any(|l| set.contains(&l.as_str()))
}

/// The queue-state counts of one repo's complete open listing (issues and
/// PRs), every state present, `0` included. Pure.
#[must_use]
pub fn queue_counts(rows: &[RestIssue]) -> BTreeMap<&'static str, usize> {
    use super::stage_dwell::{BUILDING, CURATED, ISSUE};
    let mut counts: BTreeMap<&'static str, usize> =
        QUEUE_STATES.iter().map(|state| (*state, 0)).collect();
    let mut bump = |state: &'static str| *counts.entry(state).or_insert(0) += 1;
    let linked: BTreeSet<u32> = rows
        .iter()
        .filter(|row| row.is_pull_request)
        .flat_map(|row| super::stage_dwell::closing_refs(row.body.as_deref().unwrap_or("")))
        .collect();
    for row in rows {
        if row.is_pull_request {
            if has_any(row, &crate::work_finder::HOLD_NAMES) {
                bump("pr_held");
            }
            continue;
        }
        if has(row, TRIAGE) {
            bump("triage");
        }
        if !row.labels.iter().any(|l| l.starts_with("loom:")) {
            bump("unlabeled");
        }
        if has(row, CURATED) && !has(row, ISSUE) && !has(row, BUILDING) {
            bump("curated_not_promoted");
        }
        if has(row, ISSUE) {
            bump(if linked.contains(&row.number) {
                "issue_open_pr"
            } else {
                "issue_no_pr"
            });
        }
        if has_any(row, &crate::work_finder::PARK_LABELS) {
            bump("parked");
        }
    }
    counts
}

/// One complete walk of the open issues and PRs of the repo at `root`, off
/// the async runtime, reduced to [`queue_counts`]. `None` when the walk was
/// shed, failed or could not finish: nothing measured.
pub(super) async fn walk_counts(root: std::path::PathBuf) -> Option<BTreeMap<&'static str, usize>> {
    let shown = root.display().to_string();
    let result = tokio::task::spawn_blocking(move || {
        let gh = std::path::PathBuf::from(crate::gh_invocation::gh_bin());
        crate::forge_listing::list_open_issues_cached_all_as("stage_dwell", &gh, Some(&root), None)
    })
    .await;
    match result {
        Ok(Ok(rows)) => Some(queue_counts(&rows)),
        Ok(Err(error)) => {
            log::debug!("observability: open-items walk in {shown} failed: {error}");
            None
        }
        Err(join_error) => {
            log::debug!("observability: open-items walk in {shown} panicked: {join_error}");
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "stage_queue_tests.rs"]
mod tests;
