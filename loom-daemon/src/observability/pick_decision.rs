//! `pick.decision` emission (Issue #10212): per role tick and per work-finder
//! tick, the ranked candidates, what was acted on, and why the rest were
//! skipped. The payload and its chunk split live in
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
use crate::step_dispatch::item_draw::{Candidate, DrawRecord, Exclusion, ITEM_DRAW_SEED_SCHEME};
use crate::telemetry::kinds::pick_decision::{
    source, split_into_chunks, PickAction, PickCandidate, PickDecisionRecord, PickDraw,
    PickDrawCandidate, PickItemDraw, PickItemDrawStep, PickItemExclusion, PickItemPoolEntry,
    PickLane, PickQueueSkip, PickSkipReason, PickSortKey, PickTick, PickVerdict, PickViDemotion,
    PickWorkspaceDraw, CHUNK_BYTES, WORK_FINDER_ROLE,
};
use crate::telemetry::{RoleTickResult, TelemetryRecord};
use crate::types::{QueueDisposition, WorkFinderTickSummary};
use crate::work_finder::halt_cause::HaltCause;
use crate::work_finder::step_item::Lane;
use crate::work_finder::vi_cap::ViDemotion;
use crate::work_finder::workspace_draw::{self, DrawLog};

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

thread_local! {
    /// The lane plan the admission of this thread's run computed (#10630).
    static LANE_PLAN: RefCell<Vec<crate::role_runner::concurrent_dispatch::lane_rule::RepoLane>> =
        const { RefCell::new(Vec::new()) };
}

/// Stash the per-repository lane plan the admission computed, for the run's
/// end-of-tick record (#10630).
pub fn stash_lane_plan(
    _root: &Path,
    plan: Vec<crate::role_runner::concurrent_dispatch::lane_rule::RepoLane>,
) {
    LANE_PLAN.with(|s| *s.borrow_mut() = plan);
}

fn take_lane_plan() -> Vec<PickLane> {
    LANE_PLAN.with(|s| {
        std::mem::take(&mut *s.borrow_mut())
            .into_iter()
            .take(MAX_PICK_LANES)
            .map(|r| PickLane {
                repo: repo_label(&r.root),
                debt: r.debt,
                wanted: r.wanted,
                lanes: r.lanes,
            })
            .collect()
    })
}

/// Most repositories one record lists lanes for.
const MAX_PICK_LANES: usize = 32;

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
    LANE_PLAN.with(|s| s.borrow_mut().clear());
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
    emit_in(record, None);
}

/// [`emit`] joined to a role tick's trace (#11858), so every chunk's log
/// record carries the tick's trace id.
fn emit_in(record: PickDecisionRecord, context: Option<&crate::telemetry::trace::TraceContext>) {
    if let Some(sink) = super::ops::global_ops_sink() {
        // A record over the byte budget is split, never truncated (#11567):
        // the chunks share `tick_id` and their union is the original.
        for chunk in split_into_chunks(record, CHUNK_BYTES) {
            sink.emit_record_in(TelemetryRecord::PickDecision(chunk), context.cloned());
        }
    }
}

/// Always drains the stash (it must not leak into the next tick on this
/// thread); converts it (a cached local `git` read per repository) only with an
/// exporter running.
fn take_lane_plan_if_exporting() -> Vec<PickLane> {
    if exporting() {
        take_lane_plan()
    } else {
        LANE_PLAN.with(|s| s.borrow_mut().clear());
        Vec::new()
    }
}

fn exporting() -> bool {
    super::ops::global_ops_sink().is_some()
}

/// The part of a ready-queue row's `detail` a record may carry: a
/// file-overlap deferral's shared paths (#9781, daemon-templated from parsed
/// paths) and a halted row's cause token, only when it is in the closed
/// [`HaltCause`] vocabulary (#11811), and an `already_landed` row's
/// evidence token (#11858). Every other detail (e.g. free-form
/// dispatch-error text) is dropped.
fn exportable_detail(disposition: QueueDisposition, detail: Option<&str>) -> Option<String> {
    let detail = detail?;
    match disposition {
        QueueDisposition::DeferredFileOverlap => Some(detail.to_string()),
        QueueDisposition::WorkspaceHalted => {
            HaltCause::from_wire(detail).map(|c| c.as_str().to_string())
        }
        // #11858: an already-landed row's evidence, closed vocabulary only.
        QueueDisposition::AlreadyLanded => super::ops::queue::LANDED_EVIDENCE
            .iter()
            .find(|e| **e == detail)
            .map(|e| (*e).to_string()),
        _ => None,
    }
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
                // #11811: the un-collapsed disposition and hold expiry ride
                // beside the closed-set reason.
                Some(reason) => PickVerdict::SkippedQueue(PickQueueSkip {
                    reason,
                    disposition: row.disposition,
                    detail: exportable_detail(row.disposition, row.detail.as_deref()),
                    held_until: row.plan.held_until,
                }),
            };
            (candidate, verdict)
        })
        .collect();
    let outcome = if summary.dispatched > 0 {
        "dispatched"
    } else if summary.halted_fully() {
        // #11857: a partial hold (one held root among healthy ones with
        // candidates) is not a halt; the outcome follows the rows, as the
        // `loom.dispatch.tick` result does.
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

/// The tick's workspace draws (#11103) as `pick.decision` carries them.
/// `resolve` maps a workspace index to the `repo` the record names.
#[must_use]
pub fn workspace_draw_record(
    log: &DrawLog,
    resolve: impl Fn(usize) -> String,
) -> PickWorkspaceDraw {
    let name =
        |key: &str| workspace_draw::workspace_idx(key).map_or_else(|| key.to_string(), &resolve);
    PickWorkspaceDraw {
        seed: log.seed.to_string(),
        draws_total: log.draws_total,
        draws: log
            .steps
            .iter()
            .map(|step| PickDraw {
                candidates: step
                    .draw
                    .candidates
                    .iter()
                    .map(|c| PickDrawCandidate {
                        repo: name(&c.workspace),
                        weight: c.weight,
                        base_weight: Some(c.base_weight),
                        bonus: Some(c.bonus),
                    })
                    .collect(),
                total_weight: step.draw.total_weight,
                roll: step.draw.roll,
                picked: resolve(step.workspace_idx),
                number: step.issue,
            })
            .collect(),
        seed_scheme: log.seed_inputs.as_ref().map(|i| i.scheme.to_string()),
        seed_minute: log.seed_inputs.as_ref().map(|i| i.minute.to_string()),
        seed_host: log.seed_inputs.as_ref().map(|i| i.host.clone()),
    }
}

/// One workspace's item draws (#11896), as [`item_draw_record`] reads them.
#[derive(Debug, Clone, Copy)]
pub struct ItemDrawWorkspace<'a> {
    /// The forge slug: the `repo` the record names and the seed's
    /// `workspace` input.
    pub repo: &'a str,
    /// [`Weighing::excluded`](crate::step_dispatch::item_draw::Weighing::excluded).
    pub excluded: &'a std::collections::BTreeMap<Exclusion, usize>,
    /// The pool before the first draw ([`crate::step_dispatch::item_draw::ItemDraw::starting_pool`]).
    pub pool: &'a [Candidate],
    /// The draws taken, in order.
    pub draws: &'a [DrawRecord],
}

/// The step engine's item draws for one tick (#11896) as `pick.decision`
/// carries them: the `loom-draw/v2` seed inputs shared by every workspace,
/// the weight-0 counts by reason and every draw's weight terms. `None` when
/// no workspace was drawn.
#[must_use]
pub fn item_draw_record(
    minute: u64,
    host: &str,
    tick_counter: u64,
    workspaces: &[ItemDrawWorkspace<'_>],
) -> Option<PickItemDraw> {
    if workspaces.is_empty() {
        return None;
    }
    let excluded = workspaces
        .iter()
        .flat_map(|w| {
            w.excluded
                .iter()
                .map(|(&reason, &count)| PickItemExclusion {
                    repo: w.repo.to_string(),
                    reason: reason.as_str().to_string(),
                    skip_reason: PickSkipReason::from_exclusion(reason),
                    count,
                })
        })
        .collect();
    let draws: Vec<PickItemDrawStep> = workspaces
        .iter()
        .flat_map(|w| {
            w.draws.iter().map(|d| {
                let c = &d.picked;
                PickItemDrawStep {
                    repo: w.repo.to_string(),
                    draw_index: d.draw_index,
                    seed: d.seed.to_string(),
                    pool_size: d.pool_size,
                    total_weight: d.total_weight,
                    roll: d.roll,
                    picked_index: d.picked_index,
                    number: c.target(),
                    issue: c.item.issue,
                    pr: c.item.pr,
                    step: c.step.as_str().to_string(),
                    weight: c.terms.weight,
                    actionable: c.terms.actionable,
                    priority: c.terms.priority,
                    level: c.terms.level.as_str().to_string(),
                    lane: lane_name(c.item.lane).to_string(),
                    age_permille: c.terms.age_permille,
                    age_minutes: c.terms.age_minutes,
                    age_source: c.terms.age_source.as_str().to_string(),
                }
            })
        })
        .collect();
    let pools = workspaces
        .iter()
        .flat_map(|w| {
            w.pool.iter().enumerate().map(|(i, c)| PickItemPoolEntry {
                repo: w.repo.to_string(),
                index: u32::try_from(i).unwrap_or(u32::MAX),
                number: c.target(),
                issue: c.item.issue,
                pr: c.item.pr,
                step: c.step.as_str().to_string(),
                weight: c.terms.weight,
                priority: c.terms.priority,
                age_permille: c.terms.age_permille,
                age_source: c.terms.age_source.as_str().to_string(),
            })
        })
        .collect();
    Some(PickItemDraw {
        seed_scheme: ITEM_DRAW_SEED_SCHEME.to_string(),
        seed_minute: minute.to_string(),
        seed_host: host.to_string(),
        tick_counter: tick_counter.to_string(),
        draws_total: draws.len(),
        excluded,
        pools,
        draws,
    })
}

fn lane_name(lane: Lane) -> &'static str {
    match lane {
        Lane::Normal => "normal",
        Lane::RedMain => "red_main",
        Lane::Capability => "capability",
    }
}

/// The tick's flood-cap demotions (#11509) as `pick.decision` carries them.
/// `resolve` maps a workspace index to the `repo` the record names.
#[must_use]
pub fn vi_demotion_records(
    demoted: &[ViDemotion],
    resolve: impl Fn(usize) -> String,
) -> Vec<PickViDemotion> {
    demoted
        .iter()
        .map(|d| PickViDemotion {
            repo: resolve(d.workspace_idx),
            number: d.number,
            holder: d.holder,
        })
        .collect()
}

fn resolve_repo(repo: &str) -> String {
    let path = Path::new(repo);
    if path.is_absolute() {
        repo_label(path)
    } else {
        REPO_UNRESOLVED.to_string()
    }
}

/// Emit the work finder's decision for a completed tick (a no-op with no OTLP
/// exporter). Called from the shared end-of-tick seam, so empty ticks count.
/// `draw` is the tick's workspace-draw log (#11103) and `roots[i]` workspace
/// `i`'s repo root, which names the draw's workspaces. `demoted` is the
/// tick's `loom:very-important` flood-cap demotions (#11509). `dispatch_result`
/// is the tick's canonical `loom.dispatch.result` (#11840), classified once by
/// the caller from the tick's own report; it rides beside the legacy
/// `outcome`, which is unchanged.
pub fn emit_work_finder(
    summary: &WorkFinderTickSummary,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
    (draw, demoted): (Option<&DrawLog>, &[ViDemotion]),
    roots: &[std::path::PathBuf],
    dispatch_result: &str,
) {
    if !exporting() {
        return;
    }
    emit(completed_work_finder_record(
        summary,
        &crate::sweep_registry::host_identity(),
        (started_at, ended_at),
        (draw, demoted),
        roots,
        dispatch_result,
    ));
}

/// The record [`emit_work_finder`] emits for a completed tick: the ranked
/// rows, the draws, the demotions and the caller's canonical
/// `dispatch_result` (#11840), decided by `host`. Reads only the cached repo
/// slug of `roots`.
#[must_use]
pub fn completed_work_finder_record(
    summary: &WorkFinderTickSummary,
    host: &str,
    (started_at, ended_at): (DateTime<Utc>, DateTime<Utc>),
    (draw, demoted): (Option<&DrawLog>, &[ViDemotion]),
    roots: &[std::path::PathBuf],
    dispatch_result: &str,
) -> PickDecisionRecord {
    let name = |idx: usize| {
        roots
            .get(idx)
            .map_or_else(|| REPO_UNRESOLVED.to_string(), |r| resolve_repo(&r.display().to_string()))
    };
    let draw = draw.map(|log| workspace_draw_record(log, name));
    let demoted = vi_demotion_records(demoted, name);
    work_finder_record(summary, host, started_at, ended_at, resolve_repo)
        .with_workspace_draw(draw)
        .with_very_important_demoted(demoted)
        .with_dispatch_result(Some(dispatch_result))
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
    // #11858: the record names its repo even when no candidate does.
    record.repo = Some(repo.to_string());
    record.raise_candidates_total(total);
    record.add_actions(acts.into_iter().map(|(number, action)| PickAction {
        repo: repo.to_string(),
        number,
        action: action.to_string(),
    }));
    record
}

/// Emit one role tick's decision. Always drains this thread's stashed
/// listings; builds and emits only with an OTLP exporter running. `trace` is
/// the tick's root span when it launched: it names the tick and (#11858) the
/// record joins its trace.
pub fn emit_role_tick(
    root: &Path,
    role: &str,
    started_at: DateTime<Utc>,
    outcome: &RoleTickOutcome,
    trace: Option<&super::lifecycle::RoleTrace>,
) {
    let gate = take_gate_listings(root);
    let journal = super::pick_journal::take(root);
    let lane_plan = take_lane_plan_if_exporting();
    if !exporting() {
        return;
    }
    let (result, _) = crate::role_tick_telemetry::classify(outcome);
    let tick = PickTick {
        role: role.to_string(),
        host: crate::sweep_registry::host_identity(),
        tick_id: trace.map_or_else(
            || super::lifecycle::role_execution_id(role, started_at),
            |t| t.execution.clone(),
        ),
        started_at,
        ended_at: Utc::now(),
        outcome: crate::role_tick_telemetry::result_label(result),
    };
    // #11641: the PRs this tick's agent began working, from the same journal.
    if let Some(journal) = &journal {
        super::work_started::emit_role_tick(
            role,
            &repo_label(root),
            &tick.tick_id,
            &journal.entries,
        );
    }
    let holds: &[&str] = crate::role_runner::demand::DebtAxis::for_role(role)
        .map_or(&[], crate::role_runner::demand::axis_park_labels);
    let observed = RoleObservation { journal, gate };
    let mut record = role_record(tick, result, observed, &repo_label(root), holds);
    record.lanes = lane_plan;
    emit_in(record, trace.map(|t| &t.context));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "pick_decision_tests.rs"]
mod tests;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "pick_decision_result_tests.rs"]
mod result_tests;
