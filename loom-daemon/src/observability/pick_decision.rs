//! `pick.decision` emission (Issue #10212): per role tick and per work-finder
//! tick, the ranked candidates, what was acted on, and why the rest were
//! skipped. The payload and its caps live in
//! [`crate::telemetry::kinds::pick_decision`].
//!
//! **No forge calls are added.** The work-finder side reads the
//! [`WorkFinderTickSummary`] the tick already published. The role side reads
//! the agent's pick journal ([`super::pick_journal`], #10432: the serving
//! queue `pr-queue` printed, the listings the agent `gh` front served, the
//! writes the agent issued) and, as a fallback, the queue listing the role's
//! own gate probe (`forge_queue_probe` / `forge_merge_probe`) already fetched,
//! stashed in a thread-local by [`record_gate_listing`]. The probe, the launch
//! and the end-of-tick emit run on the same blocking thread, and
//! [`emit_role_tick`] drains both, so a tick can never see a previous tick's.
//!
//! The daemon gates on a role's queue but the role agent chooses among its
//! items, so role candidates and decisions come from the journal whenever one
//! was written; `candidate_source` / `decisions_observed` say what was seen.
//!
//! Records go to the OTLP exporters' queues through the ops sink; with no OTLP
//! exporter nothing is built.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};

use super::pick_journal::{
    known_action, skip_reason_for_labels, Journal, JournalEntry, JournalRow,
};
use crate::forge_listing::RestIssue;
use crate::role_runner::RoleTickOutcome;
use crate::telemetry::kinds::pick_decision::{
    source, PickAction, PickCandidate, PickDecisionRecord, PickSkipReason, PickSortKey, PickTick,
    PickVerdict, WORK_FINDER_ROLE,
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
    super::pick_journal::discard();
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
    .with_source(source::READY_QUEUE, true)
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

/// What a role tick saw and did: the journal its agent wrote (when one was
/// attached) and the gate listings the daemon itself read.
#[derive(Debug, Clone, Default)]
pub struct RoleObservation {
    /// The agent's pick journal; `None` when none was attached.
    pub journal: Option<Journal>,
    /// The admission gate's listings, in read order.
    pub gate: Vec<(String, Vec<GateRow>)>,
}

/// Rows deduplicated by number, first occurrence (its first rank) kept.
fn dedup(rows: impl IntoIterator<Item = JournalRow>) -> Vec<JournalRow> {
    let mut seen = std::collections::HashSet::new();
    rows.into_iter().filter(|r| seen.insert(r.number)).collect()
}

/// A role tick's decision (#10212, #10432).
///
/// Candidates come from the agent's **serving queue** (the latest `pr-queue`
/// snapshot) when it read one, else from the listings it read through the agent `gh` front
/// (Curator), else from the daemon's gate listing. A candidate the agent wrote
/// to is *acted*; the rest are *skipped* with a label-derived reason or
/// `not_selected` when the agent's writes were observed, and otherwise
/// undecided (only a hold label then names a reason).
#[must_use]
pub fn role_record(
    tick: PickTick,
    result: RoleTickResult,
    observed: RoleObservation,
    repo: &str,
    holds: &[&str],
) -> PickDecisionRecord {
    let skip_all = tick_skip_reason(result);
    let attached = observed.journal.is_some();
    let journal = observed.journal.unwrap_or_default();
    let mut queue: Option<(DateTime<Utc>, usize, Vec<JournalRow>)> = None;
    let mut listing: Option<Vec<JournalRow>> = None;
    let mut acts = Vec::new();
    let mut queue_acts_observable = true;
    for entry in journal.entries {
        match entry {
            JournalEntry::Queue {
                at,
                acts_observable,
                total,
                rows,
                ..
            } => {
                queue_acts_observable &= acts_observable;
                // The latest snapshot is the queue the role last served: an
                // earlier one may rank differently or hold items since gone.
                if queue.as_ref().is_none_or(|(prev, ..)| at >= *prev) {
                    queue = Some((at, total.max(rows.len()), rows));
                }
            }
            JournalEntry::Listing { rows, .. } => {
                listing.get_or_insert_with(Vec::new).extend(rows);
            }
            JournalEntry::Act { number, action, .. } => {
                if let Some(action) = known_action(&action) {
                    acts.push((number, action));
                }
            }
        }
    }
    // Decisions are observed when the agent ran with a journal that was read
    // (a missing or unreadable one observed nothing) and its `gh`
    // writes reach the front: expected at launch, or proven by the front's own
    // entries, and not contradicted by `pr-queue`'s PATH check.
    let front_active = journal.front_expected || listing.is_some() || !acts.is_empty();
    let decisions_observed =
        attached && journal.read && skip_all.is_none() && front_active && queue_acts_observable;
    let mut total = 0;
    let (source, rows) = if let Some((_, queue_total, rows)) = queue {
        total = queue_total;
        (source::SERVING_QUEUE, dedup(rows))
    } else if let Some(rows) = listing {
        (source::LISTING, dedup(rows))
    } else {
        let mut rank = 0u32;
        let rows: Vec<JournalRow> = observed
            .gate
            .into_iter()
            .flat_map(|(stage, rows)| rows.into_iter().map(move |r| (stage.clone(), r)))
            .map(|(stage, row)| {
                rank += 1;
                JournalRow {
                    number: row.number,
                    stage,
                    labels: row.labels,
                    sort_key: Some(PickSortKey {
                        name: "listing_order".to_string(),
                        value: rank.to_string(),
                    }),
                }
            })
            .collect();
        let rows = dedup(rows);
        (
            if rows.is_empty() {
                source::NONE
            } else {
                source::GATE_LISTING
            },
            rows,
        )
    };
    let ranked = rows
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            let held = row.labels.iter().any(|l| holds.contains(&l.as_str()));
            let verdict = if let Some(reason) = skip_all {
                PickVerdict::Skipped(reason)
            } else if let Some((_, action)) = acts.iter().find(|(n, _)| *n == row.number) {
                PickVerdict::Acted(action)
            } else if held {
                PickVerdict::Skipped(PickSkipReason::OperatorHold)
            } else if decisions_observed {
                PickVerdict::Skipped(
                    skip_reason_for_labels(&row.labels).unwrap_or(PickSkipReason::NotSelected),
                )
            } else {
                PickVerdict::Undecided
            };
            (
                PickCandidate {
                    rank: u32::try_from(i + 1).unwrap_or(u32::MAX),
                    repo: repo.to_string(),
                    number: row.number,
                    stage: row.stage,
                    sort_key: row.sort_key,
                },
                verdict,
            )
        })
        .collect();
    let mut record =
        PickDecisionRecord::build(tick, ranked).with_source(source, decisions_observed);
    record.raise_candidates_total(total);
    record.add_actions(acts.into_iter().map(|(number, action)| PickAction {
        repo: repo.to_string(),
        number,
        action: action.to_string(),
    }));
    record
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
    let gate = take_gate_listings(root);
    let journal = super::pick_journal::take(root);
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
    let observed = RoleObservation { journal, gate };
    emit(role_record(tick, result, observed, &repo_label(root), holds));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "pick_decision_tests.rs"]
mod tests;
