//! `pick.decision` emission (Issue #10212): per role tick and per work-finder
//! tick, the ranked candidates, what was acted on, and why the rest were
//! skipped. The payload and its caps live in
//! [`crate::telemetry::kinds::pick_decision`].
//!
//! **No forge calls are added.** The work-finder side reads the
//! [`WorkFinderTickSummary`] the tick already published. The role side reads
//! the queue listing the role's own gate probe (`forge_queue_probe` /
//! `forge_merge_probe`) already fetched, stashed in a thread-local by
//! [`record_gate_listing`]: the probe and the end-of-tick emit run on the same
//! blocking thread, and [`emit_role_tick`] drains the stash, so a tick can never
//! see a previous tick's listing.
//!
//! Role candidates are in **forge listing order**: the daemon gates on the
//! queue but does not itself choose among its items — the role agent does,
//! per its prompt. Roles with no gate listing (curator) emit an empty candidate
//! list; the record still marks the tick.
//!
//! Records go to the OTLP exporters' queues through the ops sink; with no OTLP
//! exporter nothing is built.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};

use crate::forge_listing::RestIssue;
use crate::role_runner::RoleTickOutcome;
use crate::telemetry::kinds::pick_decision::{
    PickCandidate, PickDecisionRecord, PickSkipReason, PickSortKey, PickTick, PickVerdict,
    WORK_FINDER_ROLE,
};
use crate::telemetry::{RoleTickResult, TelemetryRecord};
use crate::types::WorkFinderTickSummary;

/// `repo` value when no forge slug is known. Never a local path (#9442).
pub const REPO_UNRESOLVED: &str = "repo_unresolved";

/// A queue-listing row kept for the end-of-tick record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateRow {
    pub number: u32,
    pub labels: Vec<String>,
}

thread_local! {
    /// `(root, label, rows)` listings this thread's tick read, in read order.
    static GATE_LISTINGS: RefCell<Vec<(std::path::PathBuf, String, Vec<GateRow>)>> =
        const { RefCell::new(Vec::new()) };
}

/// Stash the open PR rows of a gate listing the role runner already made.
pub fn record_gate_listing(root: &Path, label: &str, rows: &[RestIssue]) {
    let rows: Vec<GateRow> = rows
        .iter()
        .filter(|r| r.is_pull_request && r.state.eq_ignore_ascii_case("open"))
        .map(|r| GateRow {
            number: r.number,
            labels: r.labels.clone(),
        })
        .collect();
    GATE_LISTINGS.with(|s| {
        s.borrow_mut()
            .push((root.to_path_buf(), label.to_string(), rows))
    });
}

/// Drop anything stashed by an earlier tick on this (reused) blocking thread.
pub fn clear_gate_listings() {
    GATE_LISTINGS.with(|s| s.borrow_mut().clear());
}

fn take_gate_listings(root: &Path) -> Vec<(String, Vec<GateRow>)> {
    GATE_LISTINGS.with(|s| {
        std::mem::take(&mut *s.borrow_mut())
            .into_iter()
            .filter(|(r, _, _)| r == root)
            .map(|(_, label, rows)| (label, rows))
            .collect()
    })
}

/// The forge slug for `root`, from its `origin` remote (a local git call,
/// cached per root), or [`REPO_UNRESOLVED`].
fn repo_label(root: &Path) -> String {
    static CACHE: OnceLock<Mutex<HashMap<std::path::PathBuf, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(root)
    {
        return hit.clone();
    }
    let slug = crate::release_resolve::host::repo_slug(root)
        .unwrap_or_else(|| REPO_UNRESOLVED.to_string());
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(root.to_path_buf(), slug.clone());
    slug
}

fn emit(record: PickDecisionRecord) {
    if let Some(sink) = super::ops::global_ops_sink() {
        sink.emit_record(TelemetryRecord::PickDecision(record));
    }
}

fn exporting() -> bool {
    super::ops::global_ops_sink().is_some()
}

/// The work finder's decision for one completed tick, from its published
/// summary. `resolve` maps a row's repo (a workspace root path or a
/// `workspace #N` placeholder) to the `repo` the record carries.
#[must_use]
pub fn work_finder_record(
    summary: &WorkFinderTickSummary,
    host: &str,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
    resolve: impl Fn(&str) -> String,
) -> PickDecisionRecord {
    let ranked = summary
        .queue
        .iter()
        .map(|row| {
            let value = if row.plan.keys.is_empty() {
                row.rank.to_string()
            } else {
                row.plan
                    .keys
                    .iter()
                    .map(|k| format!("{}={}", k.name, k.value))
                    .collect::<Vec<_>>()
                    .join(",")
            };
            let candidate = PickCandidate {
                rank: u32::try_from(row.rank).unwrap_or(u32::MAX),
                repo: resolve(&row.repo),
                number: row.issue,
                stage: "loom:issue".to_string(),
                sort_key: Some(PickSortKey {
                    name: "candidate_cmp".to_string(),
                    value,
                }),
            };
            let verdict = match PickSkipReason::from_disposition(row.disposition) {
                None => PickVerdict::Acted("dispatched"),
                Some(reason) => PickVerdict::Skipped(reason),
            };
            (candidate, verdict)
        })
        .collect();
    let outcome = if summary.dispatched > 0 {
        "dispatched"
    } else if summary.halted {
        "halted"
    } else if summary.queue.is_empty() {
        "idle"
    } else {
        "none_dispatched"
    };
    PickDecisionRecord::build(
        PickTick {
            role: WORK_FINDER_ROLE.to_string(),
            host: host.to_string(),
            tick_id: format!("{WORK_FINDER_ROLE}-{}", crate::telemetry::trace::instant(started_at)),
            started_at,
            ended_at,
            outcome: outcome.to_string(),
        },
        ranked,
    )
}

/// Emit the work finder's decision for a completed tick (a no-op with no OTLP
/// exporter). Called from the shared end-of-tick seam, so empty ticks count.
pub fn emit_work_finder(
    summary: &WorkFinderTickSummary,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
) {
    if !exporting() {
        return;
    }
    emit(work_finder_record(
        summary,
        &crate::sweep_registry::host_identity(),
        started_at,
        ended_at,
        |repo| {
            let path = Path::new(repo);
            if path.is_absolute() {
                repo_label(path)
            } else {
                REPO_UNRESOLVED.to_string()
            }
        },
    ))
}

/// The reason every candidate of a tick that never ran the role was skipped.
fn tick_skip_reason(result: RoleTickResult) -> Option<PickSkipReason> {
    match result {
        RoleTickResult::Success | RoleTickResult::Failure => None,
        RoleTickResult::SkippedNoTokenPool | RoleTickResult::SkippedPoolExhausted => {
            Some(PickSkipReason::Quota)
        }
        RoleTickResult::RuntimeRejected
        | RoleTickResult::SkippedModelRuntimeMismatch
        | RoleTickResult::SkippedLoad
        | RoleTickResult::SkippedQueueEmpty => Some(PickSkipReason::TickSkipped),
    }
}

/// A role tick's decision from the listings its gate read.
#[must_use]
pub fn role_record(
    tick: PickTick,
    result: RoleTickResult,
    listings: Vec<(String, Vec<GateRow>)>,
    repo: &str,
    holds: &[&str],
) -> PickDecisionRecord {
    let skip_all = tick_skip_reason(result);
    let mut rank = 0u32;
    let mut ranked = Vec::new();
    for (stage, rows) in listings {
        for row in rows {
            rank += 1;
            let held = row.labels.iter().any(|l| holds.contains(&l.as_str()));
            let verdict = if held {
                PickVerdict::Skipped(PickSkipReason::OperatorHold)
            } else {
                skip_all.map_or(PickVerdict::Undecided, PickVerdict::Skipped)
            };
            ranked.push((
                PickCandidate {
                    rank,
                    repo: repo.to_string(),
                    number: row.number,
                    stage: stage.clone(),
                    sort_key: Some(PickSortKey {
                        name: "listing_order".to_string(),
                        value: rank.to_string(),
                    }),
                },
                verdict,
            ));
        }
    }
    PickDecisionRecord::build(tick, ranked)
}

/// Emit one role tick's decision. Always drains this thread's stashed
/// listings; builds and emits only with an OTLP exporter running.
pub fn emit_role_tick(
    root: &Path,
    role: &str,
    started_at: DateTime<Utc>,
    outcome: &RoleTickOutcome,
    tick_id: Option<String>,
) {
    let listings = take_gate_listings(root);
    if !exporting() {
        return;
    }
    let (result, _) = crate::role_tick_telemetry::classify(outcome);
    let tick = PickTick {
        role: role.to_string(),
        host: crate::sweep_registry::host_identity(),
        tick_id: tick_id.unwrap_or_else(|| super::lifecycle::role_execution_id(role, started_at)),
        started_at,
        ended_at: Utc::now(),
        outcome: crate::role_tick_telemetry::result_label(result),
    };
    let holds: &[&str] = crate::role_runner::demand::DebtAxis::for_role(role)
        .map_or(&[], crate::role_runner::demand::axis_park_labels);
    emit(role_record(tick, result, listings, &repo_label(root), holds));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "pick_decision_tests.rs"]
mod tests;
