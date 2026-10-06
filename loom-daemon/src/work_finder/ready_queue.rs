//! Per-issue ready-queue recording for the multi-workspace tick (Issue #8852).
//!
//! `tick_multi_with_sharding` already decides, for every ready issue it lists,
//! whether to skip, defer, or dispatch it, and bumps one [`super::TickReport`]
//! counter for each outcome. This module records the same outcome **per
//! issue**, next to the counter bump, so the operator can see which issue is
//! next and what is holding each one.
//!
//! Rows are ranked with [`super::candidate_cmp`], the comparator the tick
//! itself sorts dispatch candidates with, so the queue order is the real
//! dispatch order and is never re-derived here. Issues dropped before the sort
//! (skip labels, backoff, peer claims, …) are ranked by the same comparator,
//! which shows where they would sit once unblocked.

use std::path::PathBuf;

use chrono::{DateTime, Utc};

use crate::types::{PlanKey, QueueDisposition, ReadyQueueRow};

use super::{candidate_cmp, PriorityCandidate, WorkItem};

/// One recorded row before it is ranked and given a repo name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickQueueRow {
    /// The ordering keys, exactly as dispatch uses them.
    pub key: PriorityCandidate,
    /// The issue's `tier:*` label, if any (information only).
    pub tier: Option<String>,
    /// What the tick did with it. `None` only while a candidate is waiting
    /// for pass 2; [`finish`] treats a still-`None` row as a capacity deferral.
    pub disposition: Option<QueueDisposition>,
    /// Specifics (park label, open PR number, error text).
    pub detail: Option<String>,
    /// The issue's `updatedAt` from the listing, which seeds its queue-dwell
    /// clock (#8856, `observability::ops::dwell`).
    pub updated_at: Option<String>,
    /// When a time-boxed hold clears (Issue #9311): `RecheckInterval`,
    /// `DispatchBackoff`/`OpenPrBackoff`, `NoopCooldown`, `Declined`, or
    /// `PrlessRetry`. `None` for every other disposition, including a
    /// candidate still awaiting pass 2 — `dispatch_plan::annotate_rows` copies
    /// this straight into `RowPlan::held_until`.
    pub held_until: Option<DateTime<Utc>>,
    /// The issue's resolved story-point size (#9432), from its single
    /// `points:*` label (Issue #9674). Resolved once here from the listing's
    /// label projection — zero extra forge reads — so the disposition
    /// exporter can sum the frozen backlog's weight without reading the
    /// forge.
    pub story_points: Option<u32>,
}

/// The quiet resolution of `item`'s story points: [`PointsLabels::value`] —
/// `None` for absent, out-of-vocabulary, and stacked labels, with **no log**
/// (unlike `resolve_story_points`). The listing re-reads every ready issue
/// every tick, so a curation defect here would otherwise warn per tick,
/// duplicating the loud warn the dispatch path already emits for the same
/// issue.
fn points_of(item: &WorkItem) -> Option<u32> {
    crate::story_points::classify_points_labels(&item.labels).value()
}

fn tier_of(item: &WorkItem) -> Option<String> {
    item.labels.iter().find(|l| l.starts_with("tier:")).cloned()
}

/// The ordering keys for `item` in workspace `idx`. `repo_red` is whether
/// that repo's `main` is verified red this tick (#9244): the red-main-fix key
/// is set only then, so a marker on a green repo gets no boost. Every
/// candidate key — the view's and dispatch's — is built here, so the two
/// cannot drift.
#[must_use]
pub fn key_of(
    idx: usize,
    workspace_priority: u32,
    item: &WorkItem,
    repo_red: bool,
) -> PriorityCandidate {
    PriorityCandidate {
        workspace_idx: idx,
        workspace_priority,
        operator_level: item.operator_level(),
        operator_priority: item.is_operator_priority(),
        operator_priority_at: item.operator_priority_at.clone(),
        main_red_fix: repo_red && item.is_main_red_fix(),
        created_at: item.created_at.clone(),
        number: item.number,
        complexity: None,
    }
}

/// Stable-sort one workspace's ready items by the #9244 lane keys alone
/// (starred, starred-at, red-main fix): starred and fix issues move to the
/// front, and every other item keeps its listing order. The single-workspace
/// tick's ordering; the multi-workspace tick sorts by the full
/// [`candidate_cmp`].
pub fn sort_lanes(items: &mut [WorkItem], repo_red: bool) {
    items.sort_by(|a, b| {
        super::ordering::lane_cmp(&key_of(0, 0, a, repo_red), &key_of(0, 0, b, repo_red))
    });
}

/// The dispatch-order seam (Issue #9288), re-exported beside [`key_of`]:
/// [`candidate_cmp`] is the lexicographic compare of [`candidate_keys`].
pub use super::ordering::{
    candidate_keys, keyed_cmp, CandidateKey, KeyValue, ETA_IGNORED_KEYS, ETA_POSITION_KEYS,
};

/// The comparator's key names, in order — the plan's `ordering`.
#[must_use]
pub fn ordering_names() -> Vec<String> {
    candidate_keys(&PriorityCandidate::default())
        .iter()
        .map(|k| k.name.to_string())
        .collect()
}

/// `c`'s keys as wire [`PlanKey`]s.
#[must_use]
pub fn plan_keys(c: &PriorityCandidate) -> Vec<PlanKey> {
    candidate_keys(c)
        .iter()
        .map(|k| PlanKey {
            name: k.name.to_string(),
            value: k.value.to_json(),
        })
        .collect()
}

/// Record a ready issue the tick dropped before the global sort, with no
/// time-boxed hold expiry. Mirrors [`record_skip_held`] with `held_until:
/// None` — the common case, every disposition but the five Issue #9311
/// tracks an expiry for.
pub fn record_skip(
    rows: &mut Vec<TickQueueRow>,
    key: PriorityCandidate,
    item: &WorkItem,
    disposition: QueueDisposition,
    detail: Option<String>,
) {
    record_skip_held(rows, key, item, disposition, detail, None);
}

/// [`record_skip`] plus a time-boxed hold's absolute expiry (Issue #9311):
/// `RecheckInterval`, `DispatchBackoff`/`OpenPrBackoff`, `NoopCooldown`,
/// `Declined`, or `PrlessRetry`, when known.
pub fn record_skip_held(
    rows: &mut Vec<TickQueueRow>,
    key: PriorityCandidate,
    item: &WorkItem,
    disposition: QueueDisposition,
    detail: Option<String>,
    held_until: Option<DateTime<Utc>>,
) {
    rows.push(TickQueueRow {
        key,
        tier: tier_of(item),
        disposition: Some(disposition),
        detail,
        updated_at: item.updated_at.clone(),
        held_until,
        story_points: points_of(item),
    });
}

/// Record a ready issue that entered the dispatch candidate list. Pass 2
/// resolves its disposition with [`resolve`]. Never carries a `held_until`:
/// only a pass-1 skip can, and a dispatched/deferred candidate is never a
/// time-boxed hold.
pub fn record_candidate(rows: &mut Vec<TickQueueRow>, key: &PriorityCandidate, item: &WorkItem) {
    rows.push(TickQueueRow {
        key: PriorityCandidate {
            complexity: None,
            ..key.clone()
        },
        tier: tier_of(item),
        disposition: None,
        detail: None,
        updated_at: item.updated_at.clone(),
        held_until: None,
        story_points: points_of(item),
    });
}

/// Set the pass-2 outcome for candidate `cand`.
pub fn resolve(
    rows: &mut [TickQueueRow],
    cand: &PriorityCandidate,
    disposition: QueueDisposition,
    detail: Option<String>,
) {
    if let Some(row) = rows.iter_mut().find(|r| {
        r.disposition.is_none()
            && r.key.workspace_idx == cand.workspace_idx
            && r.key.number == cand.number
    }) {
        row.disposition = Some(disposition);
        row.detail = detail;
    }
}

/// The first label on `item` that parks it, for the `Parked` row's detail.
#[must_use]
pub fn park_label(item: &WorkItem, extra_skip_labels: &[String]) -> Option<String> {
    item.labels
        .iter()
        .find(|l| {
            super::SKIP_LABELS.contains(&l.as_str()) || extra_skip_labels.iter().any(|x| x == *l)
        })
        .cloned()
}

/// Truncate free-form error text so one row stays one line on a dashboard.
#[must_use]
pub fn short_detail(text: &str) -> String {
    const MAX: usize = 160;
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() <= MAX {
        line.to_string()
    } else {
        let cut: String = line.chars().take(MAX).collect();
        format!("{cut}…")
    }
}

/// The display name of workspace `idx`: its repo root, or `workspace #idx`.
fn repo_name(idx: usize, roots: &[PathBuf]) -> String {
    roots
        .get(idx)
        .map_or_else(|| format!("workspace #{idx}"), |p| p.display().to_string())
}

/// Display names for a list of workspace indexes.
#[must_use]
pub fn repo_names(idxs: &[usize], roots: &[PathBuf]) -> Vec<String> {
    idxs.iter().map(|i| repo_name(*i, roots)).collect()
}

/// Rank the recorded rows in dispatch order and name each row's repo.
///
/// `roots[i]` is workspace `i`'s repo root; an index with no root is shown as
/// `workspace #i`.
#[must_use]
pub fn finish(rows: &[TickQueueRow], roots: &[PathBuf]) -> Vec<ReadyQueueRow> {
    ranked(rows)
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            debug_assert!(r.disposition.is_some(), "unresolved queue row #{}", r.key.number);
            let disposition = r.disposition.unwrap_or(QueueDisposition::DeferredCapacity);
            ReadyQueueRow {
                rank: i + 1,
                repo: repo_name(r.key.workspace_idx, roots),
                issue: r.key.number,
                workspace_priority: r.key.workspace_priority,
                // Deprecated by #9244: `loom:urgent` no longer orders anything.
                // Kept on the wire, always false, for one release.
                urgent: false,
                operator_priority: r.key.operator_priority,
                operator_priority_at: r.key.operator_priority_at.clone(),
                main_red_fix: r.key.main_red_fix,
                created_at: r.key.created_at.clone(),
                tier: r.tier.clone(),
                story_points: r.story_points,
                disposition,
                detail: r.detail.clone(),
                state: disposition.state().to_string(),
                reason: disposition.reason().to_string(),
                plan: crate::types::RowPlan::default(),
            }
        })
        .collect()
}

/// `rows` in comparator order — the order [`finish`] ranks them in, shared
/// with `dispatch_plan::annotate` so the two index the same row.
#[must_use]
pub fn ranked(rows: &[TickQueueRow]) -> Vec<&TickQueueRow> {
    let mut sorted: Vec<&TickQueueRow> = rows.iter().collect();
    sorted.sort_by(|a, b| candidate_cmp(&a.key, &b.key));
    sorted
}

#[cfg(test)]
#[path = "ready_queue_tests.rs"]
mod tests;
