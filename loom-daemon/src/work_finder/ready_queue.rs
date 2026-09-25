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

use crate::types::{QueueDisposition, ReadyQueueRow};

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
}

fn tier_of(item: &WorkItem) -> Option<String> {
    item.labels.iter().find(|l| l.starts_with("tier:")).cloned()
}

/// The ordering keys for `item` in workspace `idx`.
#[must_use]
pub fn key_of(idx: usize, workspace_priority: u32, item: &WorkItem) -> PriorityCandidate {
    PriorityCandidate {
        workspace_idx: idx,
        workspace_priority,
        urgent: item.is_urgent(),
        created_at: item.created_at.clone(),
        number: item.number,
        complexity: None,
    }
}

/// Record a ready issue the tick dropped before the global sort.
pub fn record_skip(
    rows: &mut Vec<TickQueueRow>,
    key: PriorityCandidate,
    item: &WorkItem,
    disposition: QueueDisposition,
    detail: Option<String>,
) {
    rows.push(TickQueueRow {
        key,
        tier: tier_of(item),
        disposition: Some(disposition),
        detail,
    });
}

/// Record a ready issue that entered the dispatch candidate list. Pass 2
/// resolves its disposition with [`resolve`].
pub fn record_candidate(rows: &mut Vec<TickQueueRow>, key: &PriorityCandidate, item: &WorkItem) {
    rows.push(TickQueueRow {
        key: PriorityCandidate {
            complexity: None,
            ..key.clone()
        },
        tier: tier_of(item),
        disposition: None,
        detail: None,
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

/// Rank the recorded rows in dispatch order and name each row's repo.
///
/// `roots[i]` is workspace `i`'s repo root; an index with no root is shown as
/// `workspace #i`.
#[must_use]
pub fn finish(rows: &[TickQueueRow], roots: &[PathBuf]) -> Vec<ReadyQueueRow> {
    let mut sorted: Vec<&TickQueueRow> = rows.iter().collect();
    sorted.sort_by(|a, b| candidate_cmp(&a.key, &b.key));
    sorted
        .into_iter()
        .enumerate()
        .map(|(i, r)| ReadyQueueRow {
            rank: i + 1,
            repo: roots.get(r.key.workspace_idx).map_or_else(
                || format!("workspace #{}", r.key.workspace_idx),
                |p| p.display().to_string(),
            ),
            issue: r.key.number,
            workspace_priority: r.key.workspace_priority,
            urgent: r.key.urgent,
            created_at: r.key.created_at.clone(),
            tier: r.tier.clone(),
            disposition: r.disposition.unwrap_or(QueueDisposition::DeferredCapacity),
            detail: r.detail.clone(),
        })
        .collect()
}

#[cfg(test)]
#[path = "ready_queue_tests.rs"]
mod tests;
