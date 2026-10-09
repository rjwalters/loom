//! `fleet.state` emission (Issue #10196). This host's own view of the work it
//! holds, the PRs under review in its repos and its ready queue is sent to the
//! **OTLP** exporters on the collector's 5-minute snapshot pass. Record
//! semantics are in [`crate::telemetry::kinds::fleet_state`].
//!
//! Every row comes from state the daemon keeps for its own work, never from
//! the ETA subsystem ([`sources`]):
//!
//! - **held rows** (`sweep.*` and the post-sweep stages of a sweep this host
//!   runs): the sweep registries, with the stage read from the sweep's own
//!   checkpoint and the slot from its overflow mark (#9244).
//! - **PR rows** (`review_wait`, `doctor`, `merge_wait`, `merge_hold`) and the
//!   per-repo census: the ETag-cached review-label listings, each walked page
//!   by page (`forge_listing::list_issues_cached_all_as`; an unchanged page is
//!   a free `304`). A walk that fails, hits its page limit or sees the
//!   listing shift is a failed listing: the repo keeps its previous PR rows
//!   and has no census, so a PR it could not see is never reported removed.
//! - **`ready_wait` rows**: the work finder's last tick, the same source the
//!   `queue.snapshot` producer reads, with each row's planner rank and the
//!   inputs it was ranked on. Nothing re-ranks here. A repo whose ready
//!   listing the work finder walked to its last page is `ready_complete`
//!   (#11139) and its ready rows are diffed. A repo whose listing came back
//!   partial (a later page failed, the page cap, a mid-walk change) is not:
//!   its ready rows are what the tick saw, and every record that names it
//!   carries all of them with `ready_replace` (see [`decide`]). A repo whose
//!   ready listing failed keeps its earlier ready rows.
//!
//! When one `(repo, issue)` is seen by several sources, the held row wins,
//! then the PR row, then the ready row.
//!
//! [`build_view`] and [`decide`] are pure. A full anchor goes out on the first
//! pass of a process, whenever the planner stamps change, and once the last
//! anchor is [`ANCHOR_INTERVAL_SECS`] old; in between a delta goes out only
//! when something changed. There is no row cap: a record over
//! [`CHUNK_BYTES`] is split into chunks ([`split_into_chunks`]). The only case
//! with no record at all is no OTLP exporter (no sink registered); whether
//! ETA is enabled does not matter. An anchor with zero rows is still sent,
//! because it truthfully says "nothing here".

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;

use super::queue::{DurableQueue, FanoutQueue, QueueSink};
use crate::telemetry::kinds::fleet_state::{
    split_into_chunks, FleetCapacity, FleetHoldKind, FleetPrCensus, FleetSlot, FleetSlots,
    FleetStage, FleetStateRecord, FleetStateRepo, FleetStateRow, MainCi, PlannerStamps,
    ANCHOR_INTERVAL_SECS, CHUNK_BYTES, FLEET_STATE_SCHEMA,
};
use crate::telemetry::{RepoVisibility, TelemetryEnvelope, TelemetryRecord};

mod sources;

/// The review labels whose listings give the PR stages and the census.
pub const REVIEW_LABELS: [&str; 3] = ["loom:review-requested", "loom:changes-requested", "loom:pr"];

/// A Doctor holds the PR (accompanies `loom:changes-requested`).
const TREATING: &str = "loom:treating";

/// The operator holds that make an approved PR `merge_hold`: the label
/// registry's `merge_hold` set.
static MERGE_HOLD_LABELS: crate::label_registry::LabelSet =
    crate::label_registry::LabelSet::new(|| crate::label_registry::embedded_set("merge_hold"));

/// The hold kind the hold labels in `labels` name on their own, or `None`
/// when none holds. Labels alone cannot split `merge_risk` / `critical_file` /
/// `ac_hold` out of `operator`; that refinement is left to the reader. Pure.
#[must_use]
pub fn hold_kind(labels: &[String]) -> Option<FleetHoldKind> {
    let has = |label: &str| labels.iter().any(|l| l == label);
    if has("loom:operator-decision") {
        Some(FleetHoldKind::OperatorDecision)
    } else if has("loom:operator-only") || has("loom:operator-mechanical") {
        Some(FleetHoldKind::OperatorOnly)
    } else if has("loom:operator") {
        Some(FleetHoldKind::Operator)
    } else if has("loom:blocked") {
        Some(FleetHoldKind::Blocked)
    } else if MERGE_HOLD_LABELS.iter().any(|l| has(l)) {
        Some(FleetHoldKind::Other)
    } else {
        None
    }
}

/// Where a row came from, in ascending precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// The work finder's last tick.
    Ready,
    /// The review-label listings.
    Review,
    /// A sweep this host runs.
    Held,
}

/// A sweep this host runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldSweep {
    /// Lowercased forge slug.
    pub repo: String,
    pub issue: u32,
    pub stage: FleetStage,
    pub entered_at: DateTime<Utc>,
    pub entered_at_lower_bound: bool,
    pub pr: Option<u32>,
    /// Admitted through the overflow slot (#9244).
    pub overflow: bool,
}

/// One open item from a repo's review listings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedPr {
    pub number: u32,
    pub labels: Vec<String>,
    /// The issue the PR body links (a closing keyword first, else `Part of`).
    pub issue: Option<u32>,
}

/// One repo's complete review listings (a repo whose listing failed has none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoListing {
    /// Lowercased forge slug.
    pub repo: String,
    pub prs: Vec<ListedPr>,
}

/// One row of the work finder's last tick, with the planner's rank and inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyItem {
    /// Lowercased forge slug.
    pub repo: String,
    pub issue: u32,
    pub rank: u32,
    pub star: bool,
    pub star_at: Option<DateTime<Utc>>,
    pub level: u8,
    pub fleet_priority: u32,
    pub created_at: Option<DateTime<Utc>>,
    pub main_red_fix: bool,
}

/// The work finder's last tick, as `fleet.state` reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadyQueue {
    pub items: Vec<ReadyItem>,
    /// Repos whose ready listing the tick read without a failure. Their
    /// `ready_wait` rows are exactly the tick's rows; a managed repo outside
    /// it keeps its earlier `ready_wait` rows.
    pub listed: BTreeSet<String>,
    /// Repos whose ready listing is known to be whole (the wire
    /// `ready_complete`, #11139). A subset of `listed`.
    pub complete: BTreeSet<String>,
    pub slots: Option<FleetSlots>,
}

/// Everything one pass read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FleetInput {
    /// This host's id.
    pub host_id: String,
    /// Every managed repo's lowercased slug.
    pub managed: BTreeSet<String>,
    pub held: Vec<HeldSweep>,
    pub listings: Vec<RepoListing>,
    /// When the listings were read; `None` when none completed.
    pub listed_at: Option<DateTime<Utc>>,
    /// `None` before the work finder's first tick.
    pub ready: Option<ReadyQueue>,
    /// Host capacity, when read.
    pub capacity: Option<FleetCapacity>,
    /// Each managed repo's `main` CI status, by lowercased slug.
    pub main_ci: BTreeMap<String, MainCi>,
}

/// One repo's full state at one pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoView {
    /// The census, when the repo was completely listed this pass.
    pub census: Option<FleetPrCensus>,
    /// Rows by issue.
    pub rows: BTreeMap<u32, FleetStateRow>,
    /// Each row's source.
    pub sources: BTreeMap<u32, Source>,
    /// `main` CI status, when the host read it.
    pub main_ci: Option<MainCi>,
}

/// The full state at one pass, before visibility tagging.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FleetView {
    /// When the census listings were read.
    pub census_at: Option<DateTime<Utc>>,
    /// Plan slot use.
    pub slots: Option<FleetSlots>,
    /// Host capacity beside `slots`.
    pub capacity: Option<FleetCapacity>,
    /// Per-repo state, by lowercased slug.
    pub repos: BTreeMap<String, RepoView>,
    /// The repos each listing source observed completely this pass. For
    /// [`Source::Ready`] that is the `ready_complete` repos only.
    pub observed: BTreeMap<Source, BTreeSet<String>>,
}

impl FleetView {
    /// Whether `repo`'s ready queue is known to be whole this pass (the wire
    /// `ready_complete`, #11139).
    #[must_use]
    pub fn ready_complete(&self, repo: &str) -> bool {
        self.observed
            .get(&Source::Ready)
            .is_some_and(|set| set.contains(repo))
    }
}

/// What the previous record left behind, which the next delta is relative
/// to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Emitted {
    /// The full state the last record brought a reader to.
    pub view: FleetView,
    /// The stamps it carried.
    pub stamps: PlannerStamps,
    /// That record's `as_of`.
    pub as_of: DateTime<Utc>,
    /// The `as_of` of the anchor its chain started from.
    pub anchor_as_of: DateTime<Utc>,
}

/// The stage an open PR's labels name: exactly one review label (an approved
/// PR under an operator hold is `merge_hold`), or `loom:treating` alone.
/// `None` when the labels name no single stage. Pure.
#[must_use]
pub fn pr_stage(labels: &[String]) -> Option<FleetStage> {
    let has = |label: &str| labels.iter().any(|l| l == label);
    let stages: Vec<FleetStage> = REVIEW_LABELS
        .iter()
        .zip([
            FleetStage::ReviewWait,
            FleetStage::Doctor,
            FleetStage::MergeWait,
        ])
        .filter(|(label, _)| has(label))
        .map(|(_, stage)| stage)
        .collect();
    match stages.as_slice() {
        [FleetStage::MergeWait] if MERGE_HOLD_LABELS.iter().any(|l| has(l)) => {
            Some(FleetStage::MergeHold)
        }
        [stage] => Some(*stage),
        [] if has(TREATING) => Some(FleetStage::Doctor),
        _ => None,
    }
}

/// A sweep's stage from its checkpoint's completion marker (written by this
/// run) and the instant that marker was written. No marker: the sweep is in
/// `sweep.curator` since it started. A marker with no timestamp leaves
/// `entered_at` a lower bound (the start). An unknown marker (a newer sweep
/// skill) is read as no marker. Pure.
#[must_use]
pub fn held_stage(
    phase: Option<&str>,
    phase_at: Option<DateTime<Utc>>,
    started_at: DateTime<Utc>,
) -> (FleetStage, DateTime<Utc>, bool) {
    let stage = match phase {
        Some("curator-done") => FleetStage::SweepBuilder,
        Some("builder-done" | "doctor-done") => FleetStage::ReviewWait,
        Some("judge-rejected") => FleetStage::Doctor,
        // `merge-done`: the work landed and the sweep is finishing.
        Some("judge-done" | "merge-done") => FleetStage::MergeWait,
        _ => return (FleetStage::SweepCurator, started_at, false),
    };
    match phase_at {
        Some(at) => (stage, at, false),
        None => (stage, started_at, true),
    }
}

/// The candidate rows of one pass, keyed by `(repo, issue)`.
type Candidates = BTreeMap<(String, u32), (Source, FleetStateRow)>;

/// Insert `row` unless a row of higher precedence (or, within one source,
/// one already there) holds the key.
fn offer(candidates: &mut Candidates, repo: &str, source: Source, row: FleetStateRow) {
    let key = (repo.to_string(), row.issue);
    match candidates.get(&key) {
        Some((held, _)) if *held >= source => {}
        _ => {
            candidates.insert(key, (source, row));
        }
    }
}

/// The full state of `input` at `now`. `prev` is the previous pass's view:
/// a row still in the same stage keeps its entry instant, a row new to a
/// listing that was complete last pass entered at `now`, and any other row is
/// first seen mid-stage (`entered_at_lower_bound`). A managed repo whose
/// listing failed this pass keeps that source's previous rows; a repo whose
/// ready listing was read but not known whole has exactly the tick's ready
/// rows (no carry-forward, so no ready row outlives its absence). Pure.
#[must_use]
pub fn build_view(input: &FleetInput, prev: Option<&FleetView>, now: DateTime<Utc>) -> FleetView {
    let mut candidates = Candidates::new();
    for sweep in &input.held {
        let row = FleetStateRow {
            entered_at_lower_bound: sweep.entered_at_lower_bound,
            pr: sweep.pr,
            host: Some(input.host_id.clone()),
            slot: Some(if sweep.overflow {
                FleetSlot::Overflow
            } else {
                FleetSlot::Regular
            }),
            ..FleetStateRow::new(sweep.issue, sweep.stage, sweep.entered_at)
        };
        offer(&mut candidates, &sweep.repo, Source::Held, row);
    }

    let mut observed: BTreeMap<Source, BTreeSet<String>> = BTreeMap::new();
    let mut census: BTreeMap<String, FleetPrCensus> = BTreeMap::new();
    // Label-derived hold kind of every listed PR that names an issue, so a
    // sweep-owned row (which wins on precedence) can still carry the hold.
    let mut pr_holds: BTreeMap<(String, u32), Option<FleetHoldKind>> = BTreeMap::new();
    for listing in &input.listings {
        observed
            .entry(Source::Review)
            .or_default()
            .insert(listing.repo.clone());
        let mut repo_census = FleetPrCensus::default();
        let mut prs: Vec<&ListedPr> = listing.prs.iter().collect();
        prs.sort_by_key(|pr| pr.number);
        for pr in prs {
            let stage = pr_stage(&pr.labels);
            repo_census.open += 1;
            *repo_census
                .by_stage
                .entry(stage.map_or("held", FleetStage::as_str).to_string())
                .or_default() += 1;
            if let Some(issue) = pr.issue {
                let slot = pr_holds
                    .entry((listing.repo.clone(), issue))
                    .or_insert(None);
                *slot = slot.or_else(|| hold_kind(&pr.labels));
            }
            if let (Some(stage), Some(issue)) = (stage, pr.issue) {
                let row = FleetStateRow {
                    pr: Some(pr.number),
                    hold_kind: hold_kind(&pr.labels),
                    ..FleetStateRow::new(issue, stage, now)
                };
                offer(&mut candidates, &listing.repo, Source::Review, row);
            }
        }
        census.insert(listing.repo.clone(), repo_census);
    }

    // The repos whose ready rows this pass read, whole or not.
    let mut ready_read: BTreeSet<String> = BTreeSet::new();
    if let Some(ready) = &input.ready {
        observed
            .entry(Source::Ready)
            .or_default()
            .extend(ready.complete.iter().cloned());
        ready_read.extend(ready.listed.iter().chain(&ready.complete).cloned());
        for item in &ready.items {
            let row = FleetStateRow {
                rank: Some(item.rank),
                star: item.star,
                star_at: item.star_at,
                level: item.level,
                fleet_priority: Some(item.fleet_priority),
                created_at: item.created_at,
                main_red_fix: item.main_red_fix,
                ..FleetStateRow::new(item.issue, FleetStage::ReadyWait, now)
            };
            offer(&mut candidates, &item.repo, Source::Ready, row);
        }
    }

    // Carry forward what a failed listing could not see this pass.
    let empty = BTreeSet::new();
    if let Some(prev) = prev {
        for (repo, state) in &prev.repos {
            for (issue, source) in &state.sources {
                let read = match source {
                    Source::Review => observed.get(source).unwrap_or(&empty),
                    Source::Ready => &ready_read,
                    Source::Held => continue,
                };
                let unseen = input.managed.contains(repo) && !read.contains(repo);
                if unseen {
                    if let Some(row) = state.rows.get(issue) {
                        offer(&mut candidates, repo, *source, row.clone());
                    }
                }
            }
        }
    }

    // Entry instants for the listing sources.
    let prev_observed = |source: Source, repo: &str| {
        prev.and_then(|p| p.observed.get(&source))
            .is_some_and(|set| set.contains(repo))
    };
    let mut repos: BTreeMap<String, RepoView> = BTreeMap::new();
    for ((repo, issue), (source, mut row)) in candidates {
        let was = prev
            .and_then(|p| p.repos.get(&repo))
            .and_then(|r| r.rows.get(&issue));
        if source != Source::Held {
            match was {
                Some(was) if was.stage == row.stage => {
                    row.entered_at = was.entered_at;
                    row.entered_at_lower_bound = was.entered_at_lower_bound;
                }
                _ => row.entered_at_lower_bound = !prev_observed(source, &repo),
            }
        }
        // A sweep-owned row keeps its stage/host/slot provenance but takes
        // its hold from the linked PR's labels. A repo whose listing failed
        // this pass leaves the hold unknown, so the previous one carries.
        let review_read = observed
            .get(&Source::Review)
            .is_some_and(|set| set.contains(&repo));
        if source == Source::Held {
            row.hold_kind = if review_read {
                pr_holds.get(&(repo.clone(), issue)).copied().flatten()
            } else {
                was.and_then(|w| w.hold_kind)
            };
        }
        let hold_source = if source == Source::Held {
            Source::Review
        } else {
            source
        };
        // Hold timing. A hold seen before keeps its start; a new one starts
        // now (a lower bound when the listing was not seen last pass); one
        // that cleared is stamped with the pass that saw it clear, and keeps
        // the stamp while nothing else changes.
        match (row.hold_kind, was) {
            (Some(_), Some(was)) if was.hold_kind.is_some() => {
                row.held_since = was.held_since;
                row.held_since_lower_bound = was.held_since_lower_bound;
            }
            (Some(_), _) => {
                row.held_since = Some(now);
                row.held_since_lower_bound = !prev_observed(hold_source, &repo);
            }
            (None, Some(was))
                if was.hold_kind.is_some() && (source != Source::Held || review_read) =>
            {
                row.hold_released_at = Some(now);
            }
            (None, Some(was)) => row.hold_released_at = was.hold_released_at,
            (None, None) => {}
        }
        let state = repos.entry(repo).or_default();
        state.rows.insert(issue, row);
        state.sources.insert(issue, source);
    }
    for (repo, repo_census) in census {
        repos.entry(repo).or_default().census = Some(repo_census);
    }
    for (repo, main_ci) in &input.main_ci {
        repos.entry(repo.clone()).or_default().main_ci = Some(*main_ci);
    }
    FleetView {
        census_at: input.listed_at.filter(|_| !input.listings.is_empty()),
        slots: input.ready.as_ref().and_then(|r| r.slots),
        capacity: input.capacity.clone(),
        repos,
        observed,
    }
}

/// Whether the pass at `now` must send a full anchor: on the first pass of
/// this process, when the planner stamps changed (a regime boundary), or once
/// the last anchor is [`ANCHOR_INTERVAL_SECS`] old.
#[must_use]
pub fn needs_anchor(last: Option<&Emitted>, stamps: &PlannerStamps, now: DateTime<Utc>) -> bool {
    last.is_none_or(|last| {
        last.stamps != *stamps || now - last.anchor_as_of >= Duration::seconds(ANCHOR_INTERVAL_SECS)
    })
}

fn repo_entry(view: &FleetView, repo: &str, census: Option<FleetPrCensus>) -> FleetStateRepo {
    let main_ci = view.repos.get(repo).and_then(|r| r.main_ci);
    let ready_complete = view.ready_complete(repo);
    FleetStateRepo {
        repo: repo.to_string(),
        // Tagged by the caller once the record is known to be sent; private
        // until then is the safe default.
        visibility: RepoVisibility::Private,
        main_ci,
        census,
        ready_complete,
        ready_replace: !ready_complete,
        rows: Vec::new(),
        removed: Vec::new(),
    }
}

/// The record to send for `view` at `now`, given what was last sent, or
/// `None` when nothing changed and no anchor is due. Every repo is tagged
/// [`RepoVisibility::Private`] until the caller resolves it. The record is
/// unchunked; the caller splits it. Pure.
///
/// The change test ignores `census_at`, which advances every pass; it is
/// still carried on every record that is sent.
///
/// A repo that is not `ready_complete` has `ready_replace` set: its
/// `ready_wait` rows are not diffed. Every record that names it carries its
/// whole ready set, it is named whenever that set changed, and no
/// `ready_wait` row of it is ever sent in `removed`. A reader replaces the
/// repo's `ready_wait` rows with the set. Other rows are diffed as usual.
#[must_use]
pub fn decide(
    view: &FleetView,
    stamps: &PlannerStamps,
    last: Option<&Emitted>,
    now: DateTime<Utc>,
) -> Option<FleetStateRecord> {
    let header = |anchor: bool, anchor_as_of, prev_as_of, repos| FleetStateRecord {
        schema: FLEET_STATE_SCHEMA.to_string(),
        as_of: now,
        anchor,
        anchor_as_of,
        prev_as_of,
        chunk_index: 0,
        chunk_count: 1,
        stamps: stamps.clone(),
        census_at: view.census_at,
        slots: view.slots,
        capacity: view.capacity.clone(),
        repos,
    };
    let last = match last {
        Some(last) if !needs_anchor(Some(last), stamps, now) => last,
        _ => {
            let repos = view
                .repos
                .iter()
                .map(|(repo, state)| FleetStateRepo {
                    rows: state.rows.values().cloned().collect(),
                    ..repo_entry(view, repo, state.census.clone())
                })
                .collect();
            return Some(header(true, now, None, repos));
        }
    };

    let empty = RepoView::default();
    let names: BTreeSet<&String> = view.repos.keys().chain(last.view.repos.keys()).collect();
    let mut repos = Vec::new();
    for repo in names {
        let now_state = view.repos.get(repo).unwrap_or(&empty);
        let was = last.view.repos.get(repo).unwrap_or(&empty);
        let entry = repo_entry(view, repo, now_state.census.clone());
        let replace = entry.ready_replace;
        // With `ready_replace`, `ready_wait` rows go out as a whole set.
        let diffed = |row: &FleetStateRow| !replace || row.stage != FleetStage::ReadyWait;
        let ready_set = |state: &RepoView| -> Vec<FleetStateRow> {
            state
                .rows
                .values()
                .filter(|row| row.stage == FleetStage::ReadyWait)
                .cloned()
                .collect()
        };
        let mut rows: Vec<FleetStateRow> = now_state
            .rows
            .iter()
            .filter(|(issue, row)| diffed(row) && was.rows.get(issue) != Some(row))
            .map(|(_, row)| row.clone())
            .collect();
        let removed: Vec<u32> = was
            .rows
            .iter()
            .filter(|(issue, row)| diffed(row) && !now_state.rows.contains_key(issue))
            .map(|(issue, _)| *issue)
            .collect();
        let ready_now = ready_set(now_state);
        let ready_changed = replace && ready_now != ready_set(was);
        if rows.is_empty()
            && removed.is_empty()
            && !ready_changed
            && now_state.census == was.census
            && now_state.main_ci == was.main_ci
            && view.ready_complete(repo) == last.view.ready_complete(repo)
        {
            continue;
        }
        if replace {
            rows.extend(ready_now);
            rows.sort_by_key(|row| row.issue);
        }
        repos.push(FleetStateRepo {
            rows,
            removed,
            ..entry
        });
    }
    if repos.is_empty() && view.slots == last.view.slots && view.capacity == last.view.capacity {
        return None;
    }
    Some(header(false, last.anchor_as_of, Some(last.as_of), repos))
}

/// 12 hex of sha256 over the canonical JSON (sorted keys, no whitespace) of
/// the planner-relevant effective config: `autonomous.workFinder` (which
/// carries the slot and per-repo caps) and `autonomous.mergeSequencing`.
/// Unrelated config never changes it. Pure.
#[must_use]
pub fn planner_config_hash(effective: &Value) -> String {
    use sha2::{Digest, Sha256};
    let pick = |key: &str| {
        crate::config_resolver::get_path(effective, &format!("autonomous.{key}"))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let mut slice = serde_json::Map::new();
    slice.insert("mergeSequencing".to_string(), pick("mergeSequencing"));
    slice.insert("workFinder".to_string(), pick("workFinder"));
    let text = crate::tokens_pool::profile_merge::canonical_json(&Value::Object(slice));
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// Offers `fleet.state` envelopes to the OTLP exporters' queues.
#[derive(Clone)]
pub struct FleetStateSink {
    queue: Arc<dyn QueueSink>,
    host_id: String,
}

impl FleetStateSink {
    #[must_use]
    pub fn new(queue: Arc<dyn QueueSink>, host_id: impl Into<String>) -> Self {
        FleetStateSink {
            queue,
            host_id: host_id.into(),
        }
    }

    /// Enqueue one record.
    pub fn push(&self, record: FleetStateRecord) {
        self.queue.offer(TelemetryEnvelope::new(
            self.host_id.clone(),
            TelemetryRecord::FleetState(record),
        ));
    }
}

static SINK: OnceLock<FleetStateSink> = OnceLock::new();

/// Register the OTLP queues (called once from [`super::spawn_task`]). With no
/// OTLP exporter nothing is registered, and [`record`] returns before reading
/// anything.
pub fn register_sink(otlp_queues: Vec<Arc<DurableQueue>>, host_id: &str) {
    if otlp_queues.is_empty() {
        return;
    }
    let _ = SINK.set(FleetStateSink::new(Arc::new(FanoutQueue::new(otlp_queues)), host_id));
}

/// The previous pass's view, and what the previous record left behind.
#[derive(Debug, Clone, Default)]
struct PassState {
    prev: Option<FleetView>,
    emitted: Option<Emitted>,
}

static STATE: Mutex<PassState> = Mutex::new(PassState {
    prev: None,
    emitted: None,
});

/// One `fleet.state` pass.
pub(super) async fn record(
    workspace_root: &Path,
    workspace_pool: &crate::workspace_pool::WorkspacePool,
    slug_cache: &mut std::collections::HashMap<String, String>,
) {
    let Some(sink) = SINK.get() else {
        return;
    };
    let now = Utc::now();
    let input =
        sources::gather(workspace_root, workspace_pool, slug_cache, &sink.host_id, now).await;
    let stamps = sources::stamps(workspace_root);
    let state = STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let view = build_view(&input, state.prev.as_ref(), now);
    let Some(mut record) = decide(&view, &stamps, state.emitted.as_ref(), now) else {
        STATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .prev = Some(view);
        return;
    };
    for repo in &mut record.repos {
        repo.visibility = super::collector::resolve_visibility(&repo.repo).await;
    }
    let anchor_as_of = record.anchor_as_of;
    let chunks = split_into_chunks(record, CHUNK_BYTES);
    if let Some(first) = chunks.first() {
        log::debug!(
            "fleet.state: anchor={} chunks={} repos={} rows={} removed={}",
            first.anchor,
            chunks.len(),
            chunks.iter().map(|c| c.repos.len()).sum::<usize>(),
            chunks
                .iter()
                .map(FleetStateRecord::row_count)
                .sum::<usize>(),
            chunks
                .iter()
                .map(FleetStateRecord::removed_count)
                .sum::<usize>(),
        );
    }
    for chunk in chunks {
        sink.push(chunk);
    }
    *STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = PassState {
        prev: Some(view.clone()),
        emitted: Some(Emitted {
            view,
            stamps,
            as_of: now,
            anchor_as_of,
        }),
    };
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "fleet_state_tests.rs"]
mod tests;
