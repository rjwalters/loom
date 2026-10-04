//! ETA wiring (#9289): feeds the [`crate::eta::tracker::Tracker`] from the
//! event bus and the forge, writes the stage-sample journal, and emits
//! `eta.estimate` / `eta.outcome`.
//!
//! Two triggers, per the operator decisions on #9289:
//!
//! - **Bus** ([`spawn_task`]): sweep dispatch, phase and terminal events.
//!   Each transition is journaled the moment it is seen and the item is
//!   re-estimated immediately. Issue-sweep dispatches and completions also
//!   feed the slot-turnover ledger whose samples `start-v1` reads (#9326).
//! - **Pass** ([`record`], on the collector's 5-minute snapshot cadence):
//!   each managed repo's review-label listings (the ETag-cached REST
//!   listing, where an unchanged listing is a free `304`), at most
//!   [`FORGE_READ_BUDGET`] `pulls/{n}` + `issues/{n}` reads for items whose
//!   outcome is still unknown, the last work-finder tick's dispatch plan
//!   ingested as ready items (#9326), history reloaded, the fleet view (every
//!   listed PR plus the journal's stage events) handed over for the queue
//!   features (#10201), and every live item re-estimated. An unchanged
//!   estimate is refreshed every `refreshSecs`.
//!
//! **Reads over the budget, and reads that fail, are not lost.** The tracker
//! keeps every unanswered check queued and re-offers it on the next pass
//! (`stage_dwell`'s "work over the budget waits for the next sample"), and
//! refuses to emit a `land` estimate for an item whose check is outstanding —
//! so a merge train landing more than [`FORGE_READ_BUDGET`] PRs in one pass
//! resolves over the following passes instead of leaving phantom live ETAs.
//!
//! Enabled by default (`autonomous.eta.enabled`). The records are OTLP-only
//! and go through the OTLP exporters' queues registered by
//! [`super::spawn_task`]. Without an OTLP exporter the tracker still runs and
//! journals, and emits nothing. With `LOOM_ETA_DRY_RUN=1` every would-be
//! record is logged at `info` and none is enqueued.
//!
//! Every record's trace context is the issue's D32 story
//! (`story_context(repo_id, issue)`), so estimates and outcomes land in the
//! issue's story trace. A repo with no resolvable GitHub `repo_id` gets no
//! trace context; one is never derived from the name.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::cmd_out::CmdOutcome;
use crate::event_bus::RecvError;
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
use chrono::{DateTime, Utc};

use super::queue::{DurableQueue, FanoutQueue, QueueSink};
use crate::eta::config::EtaConfig;
use crate::eta::journal::{self, JournalEntry};
use crate::eta::queue_features::EventLog;
use crate::eta::score::EstimateSummary;
use crate::eta::shadow::{self, ShadowLedger};
use crate::eta::tracker::{
    events_from_journal, Effects, Emission, EstimateContext, IssueState, ItemKey, ListedPr,
    PrState, PrView, ReadyPlan, ReadyRow, Resolved, Tracker,
};
use crate::eta::{Kind, Provenance, Registry, StageSamples};
use crate::event_bus::EventBus;
use crate::forge_listing::RestIssue;
use crate::telemetry::kinds::eta::{EtaEstimateRecord, EtaOutcomeRecord};
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};
use crate::types::{Event, SweepKind};
use crate::workspace_pool::WorkspacePool;

/// Forge reads (`pulls/{n}` and `issues/{n}`) allowed per pass, across all
/// repos. Checks that do not fit stay queued in the tracker and are retried
/// on the next pass.
pub const FORGE_READ_BUDGET: usize = 8;

/// The review labels whose listings drive post-sweep stages.
pub const REVIEW_LABELS: [&str; 3] = [
    crate::eta::labels::REVIEW_REQUESTED,
    crate::eta::labels::CHANGES_REQUESTED,
    crate::eta::labels::APPROVED,
];

const GH_TIMEOUT: Duration = Duration::from_secs(30);

/// Where pending estimates persist across restarts.
#[must_use]
pub fn pending_path(workspace_root: &Path) -> PathBuf {
    workspace_root
        .join(".loom")
        .join("state")
        .join("eta")
        .join("pending.jsonl")
}

/// The OTLP queues ETA records are offered to.
static SINK: OnceLock<(Arc<dyn QueueSink>, String)> = OnceLock::new();

/// Register the OTLP queues (called once from [`super::spawn_task`]). No
/// OTLP exporter ⇒ nothing registered ⇒ records are computed, journaled and
/// logged, never enqueued.
pub fn register_sink(otlp_queues: Vec<Arc<DurableQueue>>, host_id: &str) {
    if otlp_queues.is_empty() {
        return;
    }
    let queue: Arc<dyn QueueSink> = Arc::new(FanoutQueue::new(otlp_queues));
    let _ = SINK.set((queue, host_id.to_string()));
}

struct State {
    tracker: Tracker,
    turnover: super::ops::turnaround::TurnoverLedger,
    /// Live paired scores per `(kind, current, candidate)` (#9328): the second
    /// promotion gate's evidence, accumulated as outcomes resolve.
    shadow: ShadowLedger,
    config: EtaConfig,
    registry: Registry,
    history: StageSamples,
    repo_ids: BTreeMap<String, u64>,
    workspace_root: PathBuf,
    host_id: String,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn lock() -> std::sync::MutexGuard<'static, Option<State>> {
    STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What one delivery produced, for the pass log line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Delivered {
    /// Estimates with a result.
    pub emitted: usize,
    /// Estimates that were refusals.
    pub refused: usize,
    /// Outcomes.
    pub outcomes: usize,
    /// Records dropped for invalid provenance.
    pub invalid: usize,
}

/// The envelope for `record`, in the issue's story trace when `repo_id` is
/// known.
fn envelope(
    host_id: &str,
    record: TelemetryRecord,
    repo_id: Option<u64>,
    issue: u32,
) -> TelemetryEnvelope {
    let mut envelope = TelemetryEnvelope::new(host_id, record);
    envelope.trace_context =
        repo_id.and_then(|id| crate::telemetry::trace::story_context(id, issue).ok());
    envelope
}

fn describe_estimate(e: &crate::eta::Explanation) -> String {
    match (e.quantiles(), e.no_estimate_reason) {
        (Some((p25, p50, p75)), _) => format!("p50={p50}s p25={p25}s p75={p75}s"),
        (None, Some(reason)) => format!("no_estimate reason={reason}"),
        (None, None) => "no_estimate".to_string(),
    }
}

/// Turn emissions and outcomes into records and offer them to `sink`
/// (`None`: no OTLP exporter). Dry run logs each would-be record and offers
/// none. A record whose provenance does not validate is never offered.
pub fn deliver(
    emissions: Vec<Emission>,
    outcomes: Vec<Resolved>,
    loom: &Provenance,
    host_id: &str,
    dry_run: bool,
    sink: Option<&dyn QueueSink>,
) -> Delivered {
    let mut delivered = Delivered::default();
    for emission in emissions {
        let e = &emission.explanation;
        let line = format!(
            "eta.estimate kind={} heuristic={} primary={} repo={} issue={} {}",
            e.kind,
            e.heuristic,
            emission.primary,
            e.subject.repo,
            e.subject.issue,
            describe_estimate(e)
        );
        let (repo_id, issue) = (e.subject.repo_id, e.subject.issue);
        let refused = e.result.is_none();
        let record = EtaEstimateRecord {
            trigger: emission.trigger,
            primary: emission.primary,
            explanation: Box::new(emission.explanation),
        };
        if !record.has_provenance() {
            log::warn!("eta: dropped {line}: invalid provenance");
            delivered.invalid += 1;
            continue;
        }
        if refused {
            delivered.refused += 1;
        } else {
            delivered.emitted += 1;
        }
        if dry_run {
            log::info!("eta: would emit {line}");
        } else if let Some(sink) = sink {
            log::debug!("eta: emit {line}");
            sink.offer(envelope(host_id, TelemetryRecord::EtaEstimate(record), repo_id, issue));
        }
    }
    for resolved in outcomes {
        let line = format!(
            "eta.outcome kind={} heuristic={} repo={} issue={} outcome={:?} error_sec={:?}",
            resolved.estimate.kind,
            resolved.estimate.heuristic,
            resolved.estimate.repo,
            resolved.estimate.issue,
            resolved.score.outcome,
            resolved.score.error_sec
        );
        let (repo_id, issue) = (resolved.estimate.repo_id, resolved.estimate.issue);
        let record = EtaOutcomeRecord {
            estimate: resolved.estimate,
            loom: loom.clone(),
            score: resolved.score,
            outcome_source: resolved.outcome_source,
            outcome_resolution_sec: resolved.outcome_resolution_sec,
            result: resolved.result,
        };
        if !record.has_provenance() {
            log::warn!("eta: dropped {line}: invalid provenance");
            delivered.invalid += 1;
            continue;
        }
        delivered.outcomes += 1;
        if dry_run {
            log::info!("eta: would emit {line}");
        } else if let Some(sink) = sink {
            log::debug!("eta: emit {line}");
            sink.offer(envelope(host_id, TelemetryRecord::EtaOutcome(record), repo_id, issue));
        }
    }
    delivered
}

fn append_journal(workspace_root: &Path, rows: &[JournalEntry]) {
    if let Err(error) = journal::append(&journal::journal_path(workspace_root), rows) {
        log::warn!("eta: stage journal append failed: {error}");
    }
}

fn read_pending(path: &Path) -> Vec<EstimateSummary> {
    std::fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn write_pending(path: &Path, pending: &[EstimateSummary]) {
    let mut text = String::new();
    for estimate in pending {
        if let Ok(line) = serde_json::to_string(estimate) {
            text.push_str(&line);
            text.push('\n');
        }
    }
    let result = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| {
            let tmp = path.with_extension("jsonl.tmp");
            std::fs::write(&tmp, text)?;
            std::fs::rename(&tmp, path)
        });
    if let Err(error) = result {
        log::warn!("eta: persisting pending estimates failed: {error}");
    }
}

/// Estimate `keys` (all when `None`) under the lock, returning the
/// emissions plus what delivery needs.
fn estimate_locked(
    state: &mut State,
    keys: Option<&[ItemKey]>,
    now: DateTime<Utc>,
) -> Vec<Emission> {
    let ctx = EstimateContext {
        registry: &state.registry,
        current_start: state.config.current_start.as_deref(),
        current_finish: state.config.current_finish.as_deref(),
        current_land: state.config.current_land.as_deref(),
        history: &state.history,
        refresh_secs: state.config.refresh_secs,
        host_id: Some(state.host_id.as_str()),
        repo_ids: &state.repo_ids,
    };
    state.tracker.estimate(keys, &ctx, now)
}

fn sink() -> Option<&'static dyn QueueSink> {
    SINK.get().map(|(queue, _)| queue.as_ref())
}

/// The `current` heuristic id of each kind, per `autonomous.eta.current` and
/// the registry's own default. The one definition of "the subject's answer":
/// every other heuristic of a kind is a shadow candidate (#9328), which
/// pairs outcomes in the ledger below but is never shown as *the* ETA.
fn current_ids(state: &State) -> BTreeMap<Kind, String> {
    [Kind::Start, Kind::Finish, Kind::Land]
        .into_iter()
        .map(|kind| {
            (
                kind,
                state
                    .registry
                    .current(kind, state.config.current(kind))
                    .id()
                    .to_string(),
            )
        })
        .collect()
}

/// What [`super::eta_snapshot`] needs to build one `eta.snapshot` (#9329):
/// every estimate still awaiting an outcome, and the `current` heuristic id
/// per kind. `None` when ETA is disabled — there is then no tracker, and no
/// snapshot is emitted at all.
///
/// Read under the one tracker lock and cloned out, so the snapshot builder
/// runs no tracker code and holds no lock: it is a reader of state the
/// tracker already keeps in memory, never a second tick loop.
pub(super) fn snapshot_input() -> Option<(Vec<EstimateSummary>, BTreeMap<Kind, String>)> {
    let guard = lock();
    let state = guard.as_ref()?;
    Some((state.tracker.pending().to_vec(), current_ids(state)))
}

/// Fold `outcomes` into the shadow ledger and persist it (#9328).
///
/// Every heuristic of a kind estimated the same subject at the same `as_of`,
/// and `Tracker::resolve` scores all of them against one outcome, so the pairs
/// are already formed — this only sorts them into `(current, candidate)` runs
/// and adds them up. Best-effort: a failed persist costs a longer wait for the
/// 50-pair gate, never a wrong answer.
fn note_outcomes(state: &mut State, outcomes: &[Resolved]) {
    if outcomes.is_empty() {
        return;
    }
    let ids = current_ids(state);
    state
        .shadow
        .record(&|kind| ids.get(&kind).cloned().unwrap_or_default(), outcomes);
    let path = shadow::ledger_path(&state.workspace_root);
    if let Err(error) = shadow::write_ledger(&path, &state.shadow) {
        log::warn!("eta: persisting the shadow ledger failed: {error}");
    }
}

/// Journal, estimate and deliver the aftermath of one bus event.
async fn apply_event(effects: Effects, now: DateTime<Utc>) {
    let (dry_run, root, host_id) = {
        let guard = lock();
        let Some(state) = guard.as_ref() else {
            return;
        };
        (state.config.dry_run, state.workspace_root.clone(), state.host_id.clone())
    };
    let mut dirty = effects.dirty;
    dirty.sort();
    dirty.dedup();
    let emissions = estimate_isolated(Some(dirty), now).await;
    append_journal(&root, &effects.journal);
    if let Some(state) = lock().as_mut() {
        note_outcomes(state, &effects.outcomes);
    }
    let loom = Provenance::current();
    deliver(emissions, effects.outcomes, &loom, &host_id, dry_run, sink());
}

/// The workspace root → slug, resolving and caching on first sight.
async fn slug_for(cache: &mut HashMap<String, String>, root: &str) -> Option<String> {
    super::collector::resolve_repo_slug_cached(cache, root).await
}

/// Start the tracker and its bus subscriber. `None` when ETA is disabled.
pub fn spawn_task(
    bus: &EventBus,
    workspace_root: PathBuf,
    host_id: String,
) -> Option<tokio::task::JoinHandle<()>> {
    let config = crate::eta::config::read(&workspace_root);
    if !config.enabled {
        log::info!("eta: disabled (autonomous.eta.enabled=false)");
        return None;
    }
    let loom = Provenance::current();
    if !loom.complete {
        // Once per process: records still go out, marked incomplete, and the
        // accuracy queries leave them out.
        log::warn!(
            "eta: this build's provenance is incomplete (revision={}, tree_state={}); \
             ETA records are emitted with complete=false and excluded from accuracy queries",
            loom.revision,
            loom.tree_state
        );
    }
    let mut tracker = Tracker::new(loom);
    tracker.restore_pending(read_pending(&pending_path(&workspace_root)));
    log::info!(
        "eta: enabled (dry_run={}, refresh={}s, {} pending restored)",
        config.dry_run,
        config.refresh_secs,
        tracker.pending().len()
    );
    let shadow = shadow::read_ledger(&shadow::ledger_path(&workspace_root));
    *lock() = Some(State {
        tracker,
        turnover: super::ops::turnaround::TurnoverLedger::default(),
        shadow,
        config,
        registry: Registry::builtin(),
        history: StageSamples::default(),
        repo_ids: BTreeMap::new(),
        workspace_root: workspace_root.clone(),
        host_id,
    });
    let default_root = workspace_root.to_string_lossy().to_string();
    let mut subscription = bus.subscribe([
        "sweep.global.dispatch",
        "sweep.global.completed",
        "sweep.issue",
    ]);
    Some(tokio::spawn(async move {
        let mut slugs: HashMap<String, String> = HashMap::new();
        loop {
            let event = match subscription.recv().await {
                Ok(event) => event,
                Err(RecvError::Closed) => break,
                // A lagged receiver lost events; the next pass's listings
                // re-anchor post-sweep stages.
                Err(_) => continue,
            };
            let now = Utc::now();
            // Slot turnover (#9326): host-wide, so before any repo lookup.
            let turnover = {
                let mut guard = lock();
                let Some(state) = guard.as_mut() else {
                    break;
                };
                state
                    .turnover
                    .observe(&event, now)
                    .map(|t| state.tracker.on_slot_turnover(&t))
            };
            if let Some(effects) = turnover {
                apply_event(effects, now).await;
            }
            let root = match &event {
                Event::SweepGlobalDispatch { repo, .. }
                | Event::SweepPhase { repo, .. }
                | Event::SweepExited { repo, .. }
                | Event::SweepCrashed { repo, .. } => {
                    repo.clone().unwrap_or_else(|| default_root.clone())
                }
                _ => continue,
            };
            let Some(slug) = slug_for(&mut slugs, &root).await else {
                continue;
            };
            let effects = {
                let mut guard = lock();
                let Some(state) = guard.as_mut() else {
                    break;
                };
                match &event {
                    Event::SweepGlobalDispatch {
                        sweep_id,
                        kind: SweepKind::Issue(issue),
                        ..
                    } => state.tracker.on_dispatch(&slug, *issue, sweep_id, now),
                    Event::SweepPhase {
                        issue,
                        phase,
                        pr_number,
                        ..
                    } => state.tracker.on_phase(
                        &slug,
                        *issue,
                        phase,
                        pr_number.and_then(|n| u32::try_from(n).ok()),
                        now,
                    ),
                    Event::SweepExited {
                        issue, exit_code, ..
                    } => state
                        .tracker
                        .on_terminal(&slug, *issue, "exited", *exit_code, now),
                    Event::SweepCrashed { issue, .. } => state
                        .tracker
                        .on_terminal(&slug, *issue, "crashed", None, now),
                    _ => continue,
                }
            };
            apply_event(effects, now).await;
        }
    }))
}

fn parse_time(raw: Option<&str>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw?)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// The PR rows of a repo's review listings, each keyed to the issue it
/// closes. A PR that closes no issue is not tracked.
#[must_use]
pub fn pr_views(listings: &[Vec<RestIssue>]) -> Vec<PrView> {
    let mut seen = BTreeSet::new();
    let mut views = Vec::new();
    for item in listings.iter().flatten() {
        if !item.is_pull_request || !seen.insert(item.number) {
            continue;
        }
        let Some(issue) =
            super::ops::stage_dwell::closing_refs(item.body.as_deref().unwrap_or_default())
                .first()
                .copied()
        else {
            continue;
        };
        views.push(PrView {
            number: item.number,
            issue,
            labels: item.labels.clone(),
            created_at: parse_time(item.created_at.as_deref()),
            updated_at: parse_time(item.updated_at.as_deref()),
        });
    }
    views
}

fn gh_json(root: &Path, path: &str) -> Option<serde_json::Value> {
    let CmdOutcome::Ran(output) = GhInvocation::new(
        Operation::new("api.rest"),
        AccessIntent::Read,
        GhTarget::None,
        GH_TIMEOUT,
    )
    .args(["api", path])
    .current_dir(root)
    .run() else {
        return None;
    };
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

/// Every PR row of a repo's review listings, whatever it closes: the fleet
/// view's roster (#10201).
#[must_use]
pub fn listed_prs(listings: &[Vec<RestIssue>]) -> Vec<ListedPr> {
    let mut seen = BTreeSet::new();
    listings
        .iter()
        .flatten()
        .filter(|item| item.is_pull_request && seen.insert(item.number))
        .map(|item| ListedPr {
            number: item.number,
            labels: item.labels.clone(),
            updated_at: parse_time(item.updated_at.as_deref()),
        })
        .collect()
}

/// How PR `number` ended, from one `pulls/{n}` read. `None` when the read
/// failed: the tracker keeps the check queued and it is retried next pass.
fn read_pr_state(root: &Path, slug: &str, number: u32) -> Option<PrState> {
    let pull = gh_json(root, &format!("repos/{slug}/pulls/{number}"))?;
    if let Some(at) = parse_time(pull["merged_at"].as_str()) {
        return Some(PrState::Merged(at));
    }
    Some(if pull["state"] == "closed" {
        PrState::Closed
    } else {
        PrState::Open
    })
}

/// How issue `number` stands, from one `issues/{n}` read. A closed issue with
/// no `state_reason` is a completion (GitHub's own default for closes made
/// before the field existed). `None` when the read failed.
fn read_issue_state(root: &Path, slug: &str, number: u32) -> Option<IssueState> {
    let issue = gh_json(root, &format!("repos/{slug}/issues/{number}"))?;
    if issue["state"] != "closed" {
        return Some(IssueState::Open);
    }
    let at = parse_time(issue["closed_at"].as_str()).unwrap_or_else(Utc::now);
    Some(match issue["state_reason"].as_str() {
        Some("not_planned") => IssueState::ClosedNotPlanned(at),
        _ => IssueState::ClosedCompleted(at),
    })
}

/// One queued forge read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Check {
    /// `pulls/{n}`.
    Pr(u32),
    /// `issues/{n}`.
    Issue(u32),
}

/// What a queued read answered.
enum Answer {
    /// From `pulls/{n}`.
    Pr(PrState),
    /// From `issues/{n}`.
    Issue(IssueState),
}

/// Run `checks` (already inside the budget) on a blocking thread.
fn run_checks(checks: Vec<(PathBuf, String, ItemKey, Check)>) -> Vec<(ItemKey, Answer)> {
    checks
        .into_iter()
        .filter_map(|(root, slug, key, check)| match check {
            Check::Pr(pr) => read_pr_state(&root, &slug, pr).map(|s| (key, Answer::Pr(s))),
            Check::Issue(n) => read_issue_state(&root, &slug, n).map(|s| (key, Answer::Issue(s))),
        })
        .collect()
}

/// Estimate `keys` (all when `None`) off the async workers and behind a
/// `catch_unwind`, so the Monte Carlo neither blocks a tokio worker nor can
/// take the observability collector down with it: an ETA failure costs only
/// ETA.
async fn estimate_isolated(keys: Option<Vec<ItemKey>>, now: DateTime<Utc>) -> Vec<Emission> {
    tokio::task::spawn_blocking(move || {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut guard = lock();
            guard
                .as_mut()
                .map_or_else(Vec::new, |state| estimate_locked(state, keys.as_deref(), now))
        }))
        .unwrap_or_else(|_| {
            log::error!("eta: estimation panicked; no estimates this round (ETA only)");
            Vec::new()
        })
    })
    .await
    .unwrap_or_default()
}

/// The history one ETA pass estimates from: the `sweep.outcome` journal of
/// every managed root plus the ETA stage journal, then the configured scope
/// applied on top (#9343).
///
/// The fleet half is a **cached file read** — `eta::fleet::load_all` lists
/// `<journal_root>/.loom/state/eta/fleet/` and parses what is there. No forge call
/// happens on a tick: the derivation is paid once by
/// `loom-daemon eta fleet backfill` and topped up incrementally by
/// `eta fleet refresh`. With no cached snapshot (the default state of a host
/// that has not opted in) this is exactly the pre-#9343 host-local history,
/// `scope = "local"`.
///
/// Fetching stays outside the estimator either way: what crosses into
/// `Heuristic::estimate` is a `StageSamples` value and nothing else.
///
/// The stage journal's rows also yield the stage events the queue features
/// count (#10201), each known at this read.
fn load_history(roots: &[PathBuf], journal_root: &Path, host_id: &str) -> (StageSamples, EventLog) {
    let mut history = StageSamples::default();
    let mut seen = BTreeSet::new();
    for root in roots {
        let path = crate::sweep_outcomes::default_outcome_telemetry_path(root);
        if seen.insert(path.clone()) {
            let samples = StageSamples::load_outcome_journal(&path);
            history.stages.extend(samples.stages);
            history.censored.extend(samples.censored);
            history.verdicts.extend(samples.verdicts);
            history.paths.extend(samples.paths);
        }
    }
    let rows = journal::read(&journal::journal_path(journal_root));
    history.push_journal(&rows, host_id);
    let events = events_from_journal(&rows, Utc::now());
    let mode = crate::eta::config::read(journal_root).history_scope;
    (crate::eta::fleet::apply_scope(mode, journal_root, history), events)
}

/// One ETA pass: list, resolve, reload history, estimate, deliver. A no-op
/// when ETA is disabled.
pub(super) async fn record(
    workspace_root: &Path,
    workspace_pool: &WorkspacePool,
    slug_cache: &mut HashMap<String, String>,
) {
    let resolution_sec = super::SNAPSHOT_INTERVAL.as_secs() as i64;
    if lock().is_none() {
        return;
    }
    let mut roots = super::collector::provisioned_roots(workspace_pool);
    if !roots.iter().any(|r| r == workspace_root) {
        roots.push(workspace_root.to_path_buf());
    }
    let mut repos: Vec<(PathBuf, String, Vec<PrView>, Vec<ListedPr>)> = Vec::new();
    let mut seen = BTreeSet::new();
    for root in &roots {
        let Some(slug) =
            super::collector::resolve_repo_slug_cached(slug_cache, &root.to_string_lossy()).await
        else {
            continue;
        };
        if !seen.insert(slug.to_ascii_lowercase()) {
            continue;
        }
        let mut listings = Vec::new();
        for label in REVIEW_LABELS {
            match super::queue_blocked::list_open(root.clone(), label, "eta").await {
                Some(listing) => listings.push(listing),
                None => break,
            }
        }
        // A partial listing would read as PRs leaving review; skip the repo.
        if listings.len() == REVIEW_LABELS.len() {
            repos.push((root.clone(), slug, pr_views(&listings), listed_prs(&listings)));
        }
    }
    // Before `now`: the fleet view is known strictly before the estimates.
    let listed_at = Utc::now();

    let slugs: Vec<String> = repos.iter().map(|(_, slug, ..)| slug.clone()).collect();
    let journal_root = workspace_root.to_path_buf();
    let history_roots = roots.clone();
    let host = lock()
        .as_ref()
        .map(|state| state.host_id.clone())
        .unwrap_or_default();
    let ((history, events), repo_ids) = tokio::task::spawn_blocking(move || {
        let ids: BTreeMap<String, u64> = slugs
            .iter()
            .filter_map(|slug| {
                crate::telemetry::repo_identity::resolve(slug)
                    .map(|id| (slug.to_ascii_lowercase(), id.id))
            })
            .collect();
        (load_history(&history_roots, &journal_root, &host), ids)
    })
    .await
    .unwrap_or_default();

    let ready = ready_rows(slug_cache).await;
    let now = Utc::now();
    let mut effects = Vec::new();
    let mut checks = Vec::new();
    let mut deferred = 0_usize;
    {
        let mut guard = lock();
        let Some(state) = guard.as_mut() else {
            return;
        };
        state.history = history;
        state.repo_ids.extend(repo_ids);
        // Before the listings, so a departed ready item's `issues/{n}` read
        // is offered in this same pass.
        if let Some((rows, plan)) = &ready {
            effects.push(state.tracker.on_ready_queue(rows, plan, now));
        }
        for (root, slug, prs, _) in &repos {
            let e = state.tracker.on_listing(slug, prs, now, resolution_sec);
            // Checks the tracker still wants answered: PRs that left review
            // and issues whose outcome only the issue can settle. Anything
            // past the budget is left queued in the tracker, which re-offers
            // it next pass — never dropped.
            let queued = e
                .pr_checks
                .iter()
                .map(|(key, pr)| (key.clone(), Check::Pr(*pr)))
                .chain(e.issue_checks.iter().map(|key| {
                    let issue = key.issue;
                    (key.clone(), Check::Issue(issue))
                }));
            for (key, check) in queued {
                if checks.len() < FORGE_READ_BUDGET {
                    checks.push((root.clone(), slug.clone(), key, check));
                } else {
                    deferred += 1;
                }
            }
            effects.push(e);
        }
        let fleet: Vec<(String, Vec<ListedPr>)> = repos
            .iter()
            .map(|(_, slug, _, listed)| (slug.clone(), listed.clone()))
            .collect();
        state.tracker.on_fleet_context(&fleet, events, listed_at);
    }

    let answers = tokio::task::spawn_blocking(move || run_checks(checks))
        .await
        .unwrap_or_default();

    let (rows, outcomes, dry_run, host_id, expired, dropped) = {
        let mut guard = lock();
        let Some(state) = guard.as_mut() else {
            return;
        };
        for (key, answer) in answers {
            effects.push(match answer {
                Answer::Pr(pr_state) => state.tracker.on_pr_resolved(&key, pr_state, now),
                Answer::Issue(issue_state) => {
                    state.tracker.on_issue_resolved(&key, issue_state, now)
                }
            });
        }
        let expired = state.tracker.expire(now);
        let all = crate::eta::tracker::merged(effects);
        note_outcomes(state, &all.outcomes);
        (
            all.journal,
            all.outcomes,
            state.config.dry_run,
            state.host_id.clone(),
            expired,
            state.tracker.drain_dropped(),
        )
    };
    let emissions = estimate_isolated(None, now).await;
    let pending = lock()
        .as_ref()
        .map(|state| state.tracker.pending().to_vec())
        .unwrap_or_default();
    append_journal(workspace_root, &rows);
    let delivered = deliver(emissions, outcomes, &Provenance::current(), &host_id, dry_run, sink());
    write_pending(&pending_path(workspace_root), &pending);
    if dropped.over_cap > 0 {
        log::warn!(
            "eta: dropped {} pending estimate(s) at the {} cap — the oldest, \
             which are the long-horizon estimates accuracy scoring needs most",
            dropped.over_cap,
            crate::eta::tracker::MAX_PENDING
        );
    }
    log::info!(
        "eta: pass emitted={} refused={} outcomes={} journaled={} pending={} expired={} \
         invalid={} reads={} deferred_reads={} orphaned={}",
        delivered.emitted,
        delivered.refused,
        delivered.outcomes,
        rows.len(),
        pending.len(),
        expired,
        delivered.invalid,
        reads_answered(&rows),
        deferred,
        dropped.orphaned
    );
}

/// The last work-finder tick's ready rows, keyed by repo slug, and its plan
/// (#9326). `None` before the first tick, or when the tick carried no plan
/// (the single-workspace loop): no plan, no ready items.
async fn ready_rows(
    slug_cache: &mut HashMap<String, String>,
) -> Option<(Vec<ReadyRow>, ReadyPlan)> {
    let summary = crate::work_finder::last_tick_summary()?;
    let context = summary.plan?;
    let mut rows = Vec::with_capacity(summary.queue.len());
    for row in summary.queue {
        let Some(slug) = super::collector::resolve_repo_slug_cached(slug_cache, &row.repo).await
        else {
            continue;
        };
        rows.push(ReadyRow {
            repo: slug,
            issue: row.issue,
            plan: row.plan,
            disposition: row.disposition,
        });
    }
    let mut listing_failed = Vec::with_capacity(summary.listing_failed.len());
    for root in &summary.listing_failed {
        if let Some(slug) = super::collector::resolve_repo_slug_cached(slug_cache, root).await {
            listing_failed.push(slug);
        }
    }
    Some((
        rows,
        ReadyPlan {
            context,
            at: summary.at,
            listing_failed,
        },
    ))
}

/// How many of this pass's journal rows came from an answered forge read.
fn reads_answered(rows: &[JournalEntry]) -> usize {
    rows.iter()
        .filter(|row| row.event == "pr.resolved" || row.event == "issue.resolved")
        .count()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "eta_tests.rs"]
mod tests;
