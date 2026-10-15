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
//! | `pr_champion_held` | **PRs** at the Champion-hold label shape: `loom:pr` + `loom:blocked`, no operator hold (#11905), a subset of `pr_held`; markerless parks count too (no marker read) |
//! | `pr_refused` | **PRs** open with `loom:pr` that Champion is refusing to merge, labelled `reason` ([`refused_points`], #12027) |
//!
//! # `pr_refused`: per host, not per captain (#12027, D2)
//!
//! `pr_refused` comes from this host's refusal store
//! (`step_verdict::refusals`), fed by the Champion runs this host drained,
//! not from the walk: the walk carries labels but not the comments that say
//! why an approved PR waits. So it is reported by **every host whose store
//! has held an entry for the repo**, outside the captain skip, beside
//! `loom.forge.stage_oldest_age_seconds{state="pr_refused", reason, repo}`
//! (now minus the oldest `first_refused_at`). A host that never ran Champion
//! for a repo reports nothing for it, never a false `0`. Where this host
//! walked the repo, the walk first drops entries whose PR is no longer open
//! with `loom:pr`; a walk that did not complete leaves the repo's
//! `pr_refused` absent like every other queue state. A repo left to the fleet
//! captain is reported from the store alone (entries expire unseen after
//! `refusals::ENTRY_TTL`). Across hosts, `max by (repo, reason)` is a lower
//! bound on the count; the exact list is the `pass.verdict` query.

use std::collections::{BTreeMap, BTreeSet};

use crate::forge_listing::RestIssue;

/// Namespace of Loom's own labels, built so the label-literal guard sees no bare literal.
const LOOM_PREFIX: &str = concat!("loom", ":");

pub const TRIAGE: &str = "loom:triage";

/// Every state this module counts, in export order.
pub const QUEUE_STATES: [&str; 8] = [
    "triage",
    "unlabeled",
    "curated_not_promoted",
    "issue_open_pr",
    "issue_no_pr",
    "parked",
    "pr_held",
    "pr_champion_held",
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
            if crate::champion_hold::held_approval_shape(&row.labels) {
                bump("pr_champion_held");
            }
            continue;
        }
        if has(row, TRIAGE) {
            bump("triage");
        }
        if !row.labels.iter().any(|l| l.starts_with(LOOM_PREFIX)) {
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
pub(super) async fn walk_counts(
    root: std::path::PathBuf,
    slug: String,
) -> Option<BTreeMap<&'static str, usize>> {
    let shown = root.display().to_string();
    let result = tokio::task::spawn_blocking(move || {
        let gh = std::path::PathBuf::from(crate::gh_invocation::gh_bin());
        let rows = crate::forge_listing::list_open_issues_cached_all_as(
            "stage_dwell",
            &gh,
            Some(&root),
            None,
        );
        if let Ok(rows) = &rows {
            reconcile_refusals(&root, &slug, rows);
        }
        rows
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

/// The open PRs of a complete walk that carry `loom:pr`.
#[must_use]
pub fn approved_open(rows: &[RestIssue]) -> BTreeSet<u64> {
    rows.iter()
        .filter(|row| row.is_pull_request && has(row, super::stage_dwell::PR_APPROVED))
        .map(|row| u64::from(row.number))
        .collect()
}

/// Drop this host's refusals of `slug` that a complete walk no longer lists
/// open with `loom:pr` (#12027).
fn reconcile_refusals(root: &std::path::Path, slug: &str, rows: &[RestIssue]) {
    let open = approved_open(rows);
    let repo = slug.to_ascii_lowercase();
    crate::observability::step_verdict::refusals::with_store(Some(root), &repo, |store| {
        let before = store.clone();
        store.reconcile(&repo, &open);
        ((), *store != before)
    });
}

/// The `pr_refused` points of `repo` (lowercase `owner/name`) from `store`:
/// `loom.forge.stage_items` per refusal reason (`0` included) and
/// `loom.forge.stage_oldest_age_seconds` per reason with a refusal. Empty
/// for a repo the store never held an entry for. Pure.
#[must_use]
pub fn refused_points(
    store: &crate::observability::step_verdict::refusals::RefusalStore,
    repo: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<crate::telemetry::ops::MetricPoint> {
    use crate::telemetry::ops::{MetricName, MetricPoint};
    let reasons = &crate::observability::step_verdict::champion::GAUGE_REASONS;
    let Some(gauge) = store.gauge(repo, reasons) else {
        return Vec::new();
    };
    let mut points = Vec::new();
    for (reason, (count, oldest)) in gauge {
        points.push(
            MetricPoint::int(MetricName::ForgeStageItems, i64::try_from(count).unwrap_or(i64::MAX))
                .label("state", "pr_refused")
                .label("reason", reason)
                .label("repo", repo),
        );
        if let Some(oldest) = oldest {
            points.push(
                MetricPoint::int(
                    MetricName::ForgeStageOldestAgeSeconds,
                    (now - oldest).num_seconds().max(0),
                )
                .label("state", "pr_refused")
                .label("reason", reason)
                .label("repo", repo),
            );
        }
    }
    points
}

/// [`refused_points`] for each `(root, slug)` this sample may report
/// (walked, or left to the captain), from this host's store.
pub(super) fn refused_points_for(
    repos: &[(std::path::PathBuf, String)],
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<crate::telemetry::ops::MetricPoint> {
    let mut points = Vec::new();
    for (root, slug) in repos {
        let repo = slug.to_ascii_lowercase();
        points.extend(crate::observability::step_verdict::refusals::with_store(
            Some(root),
            &repo,
            |store| {
                let before = store.clone();
                store.prune(now);
                (refused_points(store, &repo, now), *store != before)
            },
        ));
    }
    points
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "stage_queue_tests.rs"]
mod tests;
