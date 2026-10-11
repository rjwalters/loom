//! `eta.stage_outcome` and `pr.resolved` from two consecutive `fleet.state`
//! passes (Issue #11126, #10196 R5). No ETA state is read.
//!
//! - **`eta.stage_outcome`**: a row whose stage changed between the previous
//!   view and this one, or that left the view, is one record.
//! - **`pr.resolved`**: a PR that left a repo's review listings between two
//!   complete listings of that repo is read once (`pulls/{n}`). A merged or
//!   closed PR is one record; a PR still open (its labels were removed) is
//!   none.
//!
//! Every host emits what it observes; nothing is elected. Duplicates across
//! hosts are expected, and `loom.fact_id` collapses them. The fact id is
//! keyed first on forge instants: a PR's `merged_at` / `closed_at`, for a
//! review-label move the new label's `labeled` event, and for a hold release
//! the hold label's `unlabeled` event. A move with no forge instant is still
//! emitted at the observing pass with `resolution_sec` set to the pass
//! interval, and its fact id (#11401) keys on the id of this host's sweep
//! that held the stage (only that host emits a sweep row), else on the
//! forge-dated entry. With neither, the record carries
//! `loom.fact_id_missing_reason` (`forge_read_failed` / `no_forge_instant`)
//! instead, and `host.export` counts it.
//!
//! **Entry instants (#11367).** A sweep's own checkpoint dates its entry
//! (`entered_at_source = checkpoint`). Any other row of a stage a label
//! enters is dated by that label's event in the item's history
//! (`entered_at_source = forge`), so a restart or a row first seen mid-stage
//! still gets its entry. Otherwise `entered_at` is absent and
//! `entered_at_source = unknown`. One events read (every page) per item that
//! left a stage, shared with the exit instant, at most once per pass.
//!
//! **`ready_wait` exits (#11367).** Every host that lists a repo's ready
//! queue sees a claimed issue leave it, so the exit is read from the issue's
//! history alone: the first `unlabeled loom:issue` or `closed` after the
//! entry is the instant, and a `loom:building` label by then is `advance`
//! to `sweep.curator`, anything else `cut_short`. Every host then emits the
//! same fact id. A row that left the view while the forge shows no
//! departure yet (a peer's live claim, label lag) is pending: no record,
//! dropped if the row comes back, emitted once the departure shows, and
//! after [`PENDING_READY_MAX_PASSES`] one `unknown` keyed on its forge-dated
//! entry when known.
//!
//! **Pre-ready exits (#11368).** `triage_wait` (creation or latest reopen to
//! the first `loom:curated` / `loom:issue` label; `approval_wait` follows
//! unless it was a one-step promotion) and `approval_wait` (the latest
//! `loom:curated` before the `loom:issue` label, to that label) are read from
//! an issue's history when it first shows in a complete `loom:curated`
//! listing or enters `ready_wait`, including the first pass and the first
//! complete listing after failed ones. They are never `fleet.state` rows. A
//! repo whose curated listing was incomplete is skipped; one issue's exit is
//! emitted once (its forge instants key the fact id), and a failed history
//! read is retried next pass.
//!
//! A `ready_wait` row that leaves the view is not a record unless the repo's
//! ready listing was whole in both passes (never before #11139): the work
//! finder's listing can drop a row that is still ready. A row of a repo no
//! longer managed is not a record either.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};

use super::history::{labeled, unlabeled, HistoryEvent};
pub use super::history::{LabelHistory, EVENTS_PAGE_SIZE};
use super::{FleetView, RepoListing, Source, MERGE_HOLD_LABELS, REVIEW_LABELS, TREATING};
use crate::telemetry::kinds::fleet_state::{FleetStage, FleetStateRow};
use crate::telemetry::kinds::pr_resolved::{PrResolution, PrResolvedRecord};
use crate::telemetry::kinds::stage_outcome::{
    EnteredAtSource, FactIdMissingReason, StageExit, StageOutcomeRecord, FLEET_STATE_EVENT,
};
use crate::telemetry::provenance::Provenance;
use crate::telemetry::TelemetryRecord;

/// The label of an issue in the ready queue.
const READY_LABEL: &str = "loom:issue";
/// The label of a claimed issue.
const CLAIM_LABEL: &str = "loom:building";
/// The label of a curated issue awaiting approval.
pub const CURATED_LABEL: &str = "loom:curated";

/// How long a pre-ready exit stays remembered as emitted.
const PRE_READY_MEMORY_DAYS: i64 = 14;

/// Passes a departed `ready_wait` row waits for its forge departure before
/// it is emitted as `unknown` (about an hour at the collector's cadence).
pub const PENDING_READY_MAX_PASSES: u32 = 12;

/// Failed issue reads a vanished row waits through before it is emitted as
/// `unknown` without a fact id (#11398).
pub const PENDING_ISSUE_MAX_PASSES: u32 = 12;

/// A PR's state from one `pulls/{n}` read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PullFacts {
    pub merged_at: Option<DateTime<Utc>>,
    pub closed_at: Option<DateTime<Utc>>,
}

/// An issue's state from one `issues/{n}` read (#11398).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IssueFacts {
    /// When the issue was closed; `None` while it is open.
    pub closed_at: Option<DateTime<Utc>>,
}

/// A row that vanished with no resolved PR whose issue read failed, awaiting
/// a successful read (#11398).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingIssue {
    pub row: FleetStateRow,
    pub source: Option<Source>,
    /// The pass that first saw it gone.
    pub since: DateTime<Utc>,
    /// Failed reads so far.
    pub passes: u32,
}

/// The per-item forge reads, injected so the diff is testable offline.
pub trait ForgeReads {
    /// PR `number` in `repo`, or `None` when the read failed.
    fn pull(&mut self, repo: &str, number: u32) -> Option<PullFacts>;
    /// The label history of issue or PR `number`, or `None` when the read
    /// failed.
    fn label_history(&mut self, repo: &str, number: u32) -> Option<LabelHistory>;
    /// Issue `number` in `repo`, or `None` when the read failed.
    fn issue(&mut self, repo: &str, number: u32) -> Option<IssueFacts>;
}

/// Open PR number → the issue its body links, per completely listed repo.
pub type Listed = BTreeMap<String, BTreeMap<u32, Option<u32>>>;

/// The listed PRs of this pass's complete listings.
#[must_use]
pub fn listed(listings: &[RepoListing]) -> Listed {
    listings
        .iter()
        .map(|l| {
            let prs = l.prs.iter().map(|pr| (pr.number, pr.issue)).collect();
            (l.repo.clone(), prs)
        })
        .collect()
}

/// Open `loom:curated` issue number → creation instant, per repo whose
/// listing was complete (a repo with a failed or partial listing is absent).
pub type Curated = BTreeMap<String, BTreeMap<u32, Option<DateTime<Utc>>>>;

/// A `ready_wait` row that left the view before the forge showed it leave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingReady {
    /// The row as it was.
    pub row: FleetStateRow,
    pub source: Option<Source>,
    /// The stage the view showed next (this host's own claim), if any.
    pub next: Option<FleetStage>,
    /// The pass that first saw it gone.
    pub since: DateTime<Utc>,
    /// Passes waited so far.
    pub passes: u32,
}

/// What the previous pass left for the next diff.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Memory {
    /// Each repo's last complete review listing.
    pub listed: Listed,
    /// Departed PRs whose `pulls/{n}` read failed, per repo (number → issue),
    /// retried each pass until read.
    pub unread: Listed,
    /// PRs whose terminal stage outcome was emitted while a source still
    /// supplied their row (repo, PR number); the row's later stage changes or
    /// departure add no second one. Pruned once no row carries the PR.
    pub settled: BTreeSet<(String, u32)>,
    /// Departed `ready_wait` rows awaiting their forge departure, by
    /// `(repo, issue)`.
    pub pending_ready: BTreeMap<(String, u32), PendingReady>,
    /// Vanished rows with no resolved PR whose issue read failed (#11398),
    /// retried each pass.
    pub pending_issue: BTreeMap<(String, u32), PendingIssue>,
    /// Each repo's last complete `loom:curated` listing (#11368).
    pub curated: Curated,
    /// This pass's complete curated listings, set by the caller before
    /// [`diff`] and moved into `curated` by [`remember`].
    pub curated_now: Option<Curated>,
    /// Pre-ready exits already emitted: `(repo, issue, stage, left)`.
    pub pre_ready_done: BTreeSet<(String, u32, FleetStage, DateTime<Utc>)>,
    /// Issues whose history read failed, retried each pass while they are
    /// still curated or ready.
    pub pre_ready_retry: BTreeSet<(String, u32)>,
    /// Label histories this pass already read for the row enrichment
    /// (#11391), set by the caller before [`diff`], which takes them as its
    /// per-pass cache so no history is read twice.
    pub warm: BTreeMap<(String, u32), Option<LabelHistory>>,
    /// The previous view.
    pub view: Option<FleetView>,
    /// When the previous pass ran.
    pub at: Option<DateTime<Utc>>,
}

/// One pass's input to the diff.
pub struct Pass<'a> {
    pub view: &'a FleetView,
    /// This pass's complete review listings.
    pub listed: &'a Listed,
    /// Every managed repo.
    pub managed: &'a BTreeSet<String>,
    pub now: DateTime<Utc>,
}

/// The labels whose `labeled` event moves an open PR into `stage`.
fn stage_labels(stage: FleetStage) -> Vec<&'static str> {
    match stage {
        FleetStage::ReviewWait => vec![REVIEW_LABELS[0]],
        FleetStage::Doctor => vec![REVIEW_LABELS[1]],
        FleetStage::MergeWait => vec![REVIEW_LABELS[2]],
        FleetStage::MergeHold => MERGE_HOLD_LABELS.iter().copied().collect(),
        _ => Vec::new(),
    }
}

/// The forge instant at or before `bound` at which `history` says the item
/// entered `stage`: the label that puts it there (for `merge_wait`, also a
/// released hold). `None` for a stage no label enters.
pub(super) fn forge_entry(
    history: &LabelHistory,
    stage: FleetStage,
    bound: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let holds: Vec<&str> = MERGE_HOLD_LABELS.iter().copied().collect();
    let approved = [REVIEW_LABELS[2]];
    let latest = |labels: &[&str]| history.latest(bound, |e| labeled(e, labels));
    match stage {
        FleetStage::ReadyWait => latest(&[READY_LABEL]),
        FleetStage::ReviewWait => latest(&[REVIEW_LABELS[0]]),
        FleetStage::Doctor => latest(&[REVIEW_LABELS[1]]).or_else(|| latest(&[TREATING])),
        FleetStage::MergeWait => {
            history.latest(bound, |e| labeled(e, &approved) || unlabeled(e, &holds))
        }
        FleetStage::MergeHold => {
            history.latest(bound, |e| labeled(e, &approved) || labeled(e, &holds))
        }
        _ => None,
    }
}

/// The dwell of a completed exit with a known entry.
fn dwell(
    exit: StageExit,
    entered_at: Option<DateTime<Utc>>,
    left_at: DateTime<Utc>,
) -> Option<i64> {
    let completed = !matches!(exit, StageExit::CutShort | StageExit::Unknown);
    entered_at
        .filter(|_| completed)
        .map(|at| (left_at - at).num_seconds())
        .filter(|sec| *sec >= 0)
}

/// The terminal exit and forge instant a resolved PR gives its row, if the PR
/// merged or closed.
fn resolution_exit(facts: &PullFacts) -> Option<(StageExit, DateTime<Utc>)> {
    match (facts.merged_at, facts.closed_at) {
        (Some(at), _) => Some((StageExit::Landed, at)),
        (None, Some(at)) => Some((StageExit::CutShort, at)),
        (None, None) => None,
    }
}

/// How a row left its stage.
struct Leaving {
    exit: StageExit,
    next_stage: Option<FleetStage>,
    /// The forge's instant for the move, when known.
    forge_at: Option<DateTime<Utc>>,
}

struct Diff<'a, F> {
    forge: &'a mut F,
    loom: &'a Provenance,
    now: DateTime<Utc>,
    prev_at: DateTime<Utc>,
    /// This pass's history reads, by `(repo, number)`; a failed read is
    /// `None` and is not repeated within the pass.
    histories: BTreeMap<(String, u32), Option<LabelHistory>>,
    /// PRs whose `pulls/{n}` read failed this pass, `(repo, number)`.
    unread_pulls: BTreeSet<(String, u32)>,
}

impl<F: ForgeReads> Diff<'_, F> {
    fn interval(&self) -> i64 {
        (self.now - self.prev_at).num_seconds().max(0)
    }

    /// The forge instant of `pr` entering `stage`, when it falls after the
    /// previous observation and no later than this one: the previous view
    /// had the row elsewhere, so an older `labeled` event (an earlier visit
    /// to the stage, even one inside the preceding interval) is not this move.
    /// The history of `number`, read at most once per pass.
    fn history(&mut self, repo: &str, number: u32) -> Option<&LabelHistory> {
        let key = (repo.to_string(), number);
        if !self.histories.contains_key(&key) {
            let read = self.forge.label_history(repo, number);
            self.histories.insert(key.clone(), read);
        }
        self.histories.get(&key).and_then(Option::as_ref)
    }

    /// The forge instant of `pr` entering `stage`, when it falls after the
    /// previous observation and no later than this one: the previous view
    /// had the row elsewhere, so an older `labeled` event (an earlier visit
    /// to the stage, even one inside the preceding interval) is not this move.
    fn label_instant(&mut self, repo: &str, pr: u32, stage: FleetStage) -> Option<DateTime<Utc>> {
        let labels = stage_labels(stage);
        if labels.is_empty() {
            return None;
        }
        let (prev_at, now) = (self.prev_at, self.now);
        let at = self
            .history(repo, pr)?
            .latest(now, |e| labeled(e, &labels))?;
        (at > prev_at).then_some(at)
    }

    /// The forge instant of `pr`'s hold release: the latest `unlabeled` hold
    /// event after the previous observation and no later than this one.
    fn release_instant(&mut self, repo: &str, pr: u32) -> Option<DateTime<Utc>> {
        let holds: Vec<&str> = MERGE_HOLD_LABELS.iter().copied().collect();
        let (prev_at, now) = (self.prev_at, self.now);
        let at = self
            .history(repo, pr)?
            .latest(now, |e| unlabeled(e, &holds))?;
        (at > prev_at).then_some(at)
    }

    /// Why a record of `was` has no forge instant: a read this pass needed
    /// failed (the item's history, or the PR), else none dates it.
    fn missing_reason(&self, repo: &str, issue: u32, was: &FleetStateRow) -> FactIdMissingReason {
        let failed = |n: u32| {
            let key = (repo.to_string(), n);
            matches!(self.histories.get(&key), Some(None)) || self.unread_pulls.contains(&key)
        };
        if failed(issue) || was.pr.is_some_and(failed) {
            FactIdMissingReason::ForgeReadFailed
        } else {
            FactIdMissingReason::NoForgeInstant
        }
    }

    /// When `was` entered its stage, and how that is known. An exact
    /// checkpoint wins; else the forge's label event, the latest at or before
    /// the first pass that saw this visit (a later relabel is not this
    /// visit's entry), or at or before `left_at` when no pass saw it begin.
    fn entry(
        &mut self,
        repo: &str,
        issue: u32,
        was: &FleetStateRow,
        source: Option<Source>,
        left_at: DateTime<Utc>,
    ) -> (Option<DateTime<Utc>>, EnteredAtSource) {
        if source == Some(Source::Held) && !was.entered_at_lower_bound {
            return (Some(was.entered_at), EnteredAtSource::Checkpoint);
        }
        let number = match was.stage {
            FleetStage::ReadyWait => Some(issue),
            FleetStage::ReviewWait
            | FleetStage::Doctor
            | FleetStage::MergeWait
            | FleetStage::MergeHold => was.pr,
            _ => None,
        };
        let bound = if was.entered_at_lower_bound {
            left_at
        } else {
            was.entered_at.min(left_at)
        };
        let at = number
            .and_then(|n| self.history(repo, n))
            .and_then(|h| forge_entry(h, was.stage, bound));
        match at {
            Some(at) => (Some(at), EnteredAtSource::Forge),
            None => (None, EnteredAtSource::Unknown),
        }
    }

    /// How a `ready_wait` row left, from the issue's history alone so every
    /// host reads the same exit, next stage and instant. `None` while the
    /// forge shows no departure yet, or when the read failed.
    fn ready_departure(&mut self, repo: &str, issue: u32, was: &FleetStateRow) -> Option<Leaving> {
        let bound = if was.entered_at_lower_bound {
            self.now
        } else {
            was.entered_at
        };
        let (now, slack) = (self.now, Duration::seconds(self.interval()));
        let history = self.history(repo, issue)?;
        let entered = forge_entry(history, FleetStage::ReadyWait, bound)?;
        let (left, _) = history
            .first_after(entered, |e| unlabeled(e, &[READY_LABEL]) || *e == HistoryEvent::Closed)?;
        if left > now {
            return None;
        }
        let closed = history.any_within(left, left, |e| *e == HistoryEvent::Closed);
        let claimed =
            !closed && history.any_within(entered, left + slack, |e| labeled(e, &[CLAIM_LABEL]));
        Some(Leaving {
            exit: if claimed {
                StageExit::Advance
            } else {
                StageExit::CutShort
            },
            next_stage: claimed.then_some(FleetStage::SweepCurator),
            forge_at: Some(left),
        })
    }

    /// The pre-ready exits of `issue`, each at most once. `ready` is whether
    /// the issue is in `ready_wait` now. A failed history read is `false`.
    fn pre_ready(
        &mut self,
        key: (&str, u32),
        created_at: Option<DateTime<Utc>>,
        ready: bool,
        done: &mut BTreeSet<(String, u32, FleetStage, DateTime<Utc>)>,
        out: &mut Vec<TelemetryRecord>,
    ) -> bool {
        let (repo, issue) = key;
        let now = self.now;
        let Some(history) = self.history(repo, issue) else {
            return false;
        };
        for exit in pre_ready_exits(history, created_at, now, ready) {
            if !done.insert((repo.to_string(), issue, exit.stage, exit.left)) {
                continue;
            }
            out.push(TelemetryRecord::StageOutcome(StageOutcomeRecord {
                repo: repo.to_string(),
                issue,
                pr_number: None,
                stage: exit.stage,
                entered_at: Some(exit.entered),
                entered_at_source: Some(EnteredAtSource::Forge),
                left_at: exit.left,
                dwell_sec: dwell(StageExit::Advance, Some(exit.entered), exit.left),
                exit: StageExit::Advance,
                next_stage: Some(exit.next),
                event: FLEET_STATE_EVENT.to_string(),
                observed_at: now,
                resolution_sec: Some(0),
                forge_transition_at: Some(exit.left),
                sweep_id: None,
                fact_id_missing_reason: None,
                loom: self.loom.clone(),
            }));
        }
        true
    }

    fn record(
        &mut self,
        repo: &str,
        issue: u32,
        was: &FleetStateRow,
        source: Option<Source>,
        leaving: Leaving,
    ) -> StageOutcomeRecord {
        let Leaving {
            exit,
            next_stage,
            forge_at,
        } = leaving;
        let left_at = forge_at.unwrap_or(self.now).min(self.now);
        let (entered_at, entered_at_source) = self.entry(repo, issue, was, source, left_at);
        let dwell_sec = dwell(exit, entered_at, left_at);
        let fact_id_missing_reason = forge_at
            .is_none()
            .then(|| self.missing_reason(repo, issue, was));
        StageOutcomeRecord {
            repo: repo.to_string(),
            issue,
            pr_number: was.pr,
            stage: was.stage,
            entered_at,
            entered_at_source: Some(entered_at_source),
            left_at,
            dwell_sec,
            exit,
            next_stage,
            event: FLEET_STATE_EVENT.to_string(),
            observed_at: self.now,
            resolution_sec: Some(if forge_at.is_some() {
                0
            } else {
                self.interval()
            }),
            forge_transition_at: forge_at,
            sweep_id: was.sweep_id.clone(),
            fact_id_missing_reason,
            loom: self.loom.clone(),
        }
    }
}

/// One pre-ready stage an issue's history says it left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PreReadyExit {
    stage: FleetStage,
    entered: DateTime<Utc>,
    left: DateTime<Utc>,
    next: FleetStage,
}

/// The pre-ready exits `history` shows by `now`. The triage entry is the
/// latest `reopened`, else `created_at` (none known: no exit, never a guessed
/// duration); it ends at the first later `loom:curated` / `loom:issue` label.
/// With `ready`, a curated issue's approval ran from its latest
/// `loom:curated` before its latest `loom:issue` label to that label; a
/// one-step promotion (or equal instants) has none.
fn pre_ready_exits(
    history: &LabelHistory,
    created_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    ready: bool,
) -> Vec<PreReadyExit> {
    let mut out = Vec::new();
    let entered = history
        .latest(now, |e| *e == HistoryEvent::Reopened)
        .or(created_at);
    let first = entered.and_then(|entered| {
        history
            .first_after(entered, |e| labeled(e, &[CURATED_LABEL, READY_LABEL]))
            .filter(|(at, _)| *at <= now)
            .map(|(at, event)| (entered, at, *event == HistoryEvent::Labeled(CURATED_LABEL.into())))
    });
    if let Some((entered, left, curated)) = first {
        out.push(PreReadyExit {
            stage: FleetStage::TriageWait,
            entered,
            left,
            next: if curated {
                FleetStage::ApprovalWait
            } else {
                FleetStage::ReadyWait
            },
        });
    }
    // The approval needs no triage entry: it is the latest curated label
    // before the label that made the issue ready (after the entry, when known).
    if let (true, Some(approved)) = (ready, history.latest(now, |e| labeled(e, &[READY_LABEL]))) {
        let before = approved - Duration::nanoseconds(1);
        let floor = entered.filter(|entered| *entered < approved);
        if let Some(at) = history
            .latest(before, |e| labeled(e, &[CURATED_LABEL]))
            .filter(|at| floor.is_none_or(|f| *at > f))
        {
            out.push(PreReadyExit {
                stage: FleetStage::ApprovalWait,
                entered: at,
                left: approved,
                next: FleetStage::ReadyWait,
            });
        }
    }
    out
}

/// Derive this pass's pre-ready exits (#11368): for issues new to a complete
/// curated listing or to `ready_wait`, and for earlier failed reads.
fn pre_ready_pass<F: ForgeReads>(
    memory: &mut Memory,
    prev: &FleetView,
    pass: &Pass<'_>,
    diff: &mut Diff<'_, F>,
    out: &mut Vec<TelemetryRecord>,
) {
    let curated_now = memory.curated_now.clone().unwrap_or_default();
    let curated_of = |repo: &str, issue: u32| {
        curated_now
            .get(repo)
            .and_then(|c| c.get(&issue))
            .or_else(|| memory.curated.get(repo).and_then(|c| c.get(&issue)))
            .copied()
    };
    let ready_row = |view: &FleetView, repo: &str, issue: u32| {
        view.repos
            .get(repo)
            .and_then(|r| r.rows.get(&issue))
            .filter(|row| row.stage == FleetStage::ReadyWait)
            .map(|row| row.created_at)
    };
    // (repo, issue) -> creation instant, when known.
    let mut candidates: BTreeMap<(String, u32), Option<DateTime<Utc>>> = BTreeMap::new();
    for (repo, items) in &curated_now {
        // No earlier complete listing (startup, or earlier reads failed):
        // every curated issue is new to what has been observed.
        let was = memory.curated.get(repo);
        for (&issue, &created) in items
            .iter()
            .filter(|(n, _)| was.is_none_or(|was| !was.contains_key(n)))
        {
            candidates.insert((repo.clone(), issue), created);
        }
    }
    for (repo, now_repo) in &pass.view.repos {
        if !pass.managed.contains(repo) {
            continue;
        }
        for (&issue, row) in &now_repo.rows {
            if row.stage == FleetStage::ReadyWait && ready_row(prev, repo, issue).is_none() {
                let created = row.created_at.or_else(|| curated_of(repo, issue).flatten());
                candidates.insert((repo.clone(), issue), created);
            }
        }
    }
    for (repo, issue) in &memory.pre_ready_retry {
        // A repo missing from `curated_now` had no complete listing this
        // pass: that is no evidence the issue left, so keep the retry.
        let listing = curated_now.get(repo);
        let created = match (listing.and_then(|c| c.get(issue)), ready_row(pass.view, repo, *issue))
        {
            (None, None) if listing.is_some() => continue,
            (c, r) => r
                .flatten()
                .or_else(|| c.copied().flatten())
                .or_else(|| curated_of(repo, *issue).flatten()),
        };
        candidates.entry((repo.clone(), *issue)).or_insert(created);
    }
    let mut done = std::mem::take(&mut memory.pre_ready_done);
    let mut retry = BTreeSet::new();
    for ((repo, issue), created) in candidates {
        if !pass.managed.contains(&repo) {
            continue;
        }
        let ready = ready_row(pass.view, &repo, issue).is_some();
        if !diff.pre_ready((&repo, issue), created, ready, &mut done, out) {
            retry.insert((repo, issue));
        }
    }
    let horizon = pass.now - Duration::days(PRE_READY_MEMORY_DAYS);
    done.retain(|(repo, _, _, left)| *left >= horizon && pass.managed.contains(repo));
    memory.pre_ready_done = done;
    memory.pre_ready_retry = retry;
}

/// The records two consecutive passes give. Pure apart from `forge`.
#[must_use]
pub fn diff(
    memory: &mut Memory,
    pass: &Pass<'_>,
    forge: &mut impl ForgeReads,
    loom: &Provenance,
) -> Vec<TelemetryRecord> {
    // The first pass has no earlier view to diff, but its complete curated
    // listing and ready rows already show pre-ready exits (#11368).
    let first = memory.view.is_none() || memory.at.is_none();
    let prev = memory.view.clone().unwrap_or_default();
    let prev_at = memory.at.unwrap_or(pass.now);
    let prev = &prev;
    let mut diff = Diff {
        forge,
        loom,
        now: pass.now,
        prev_at,
        histories: std::mem::take(&mut memory.warm),
        unread_pulls: BTreeSet::new(),
    };
    let mut out = Vec::new();
    pre_ready_pass(memory, prev, pass, &mut diff, &mut out);
    if first {
        return out;
    }

    let mut resolved: BTreeMap<(String, u32), PullFacts> = BTreeMap::new();
    let mut unread = std::mem::take(&mut memory.unread);
    unread.retain(|repo, _| pass.managed.contains(repo));
    for (repo, now_prs) in pass.listed {
        let Some(was_prs) = memory.listed.get(repo) else {
            continue;
        };
        // Departures of this pass, and earlier ones not yet read.
        let mut departed: BTreeMap<u32, Option<u32>> = unread.remove(repo).unwrap_or_default();
        departed.extend(was_prs.iter().map(|(&n, &issue)| (n, issue)));
        departed.retain(|n, _| !now_prs.contains_key(n));
        for (number, issue) in departed {
            let Some(facts) = diff.forge.pull(repo, number) else {
                log::debug!("fleet.state: pulls/{number} of {repo} unread; retrying next pass");
                diff.unread_pulls.insert((repo.clone(), number));
                unread
                    .entry(repo.clone())
                    .or_default()
                    .insert(number, issue);
                continue;
            };
            resolved.insert((repo.clone(), number), facts);
            let (state, at) = match (facts.merged_at, facts.closed_at) {
                (Some(merged), _) => (PrResolution::Merged, merged),
                (None, Some(closed)) => (PrResolution::Closed, closed),
                (None, None) => continue,
            };
            out.push(TelemetryRecord::PrResolved(PrResolvedRecord {
                repo: repo.clone(),
                pr_number: number,
                issue,
                state,
                resolved_at: at.min(pass.now),
                observed_at: pass.now,
                resolution_sec: 0,
                closed_at: facts.closed_at.or(facts.merged_at),
                loom: loom.clone(),
            }));
        }
    }
    memory.unread = unread;
    let mut settled = std::mem::take(&mut memory.settled);
    let mut pending = settle_pending_ready(memory, pass, &mut diff, &mut out);
    let mut pending_issue = settle_pending_issue(memory, pass, &mut diff, &mut out);

    let empty = super::RepoView::default();
    for (repo, was_repo) in &prev.repos {
        let now_repo = pass.view.repos.get(repo).unwrap_or(&empty);
        for (&issue, was) in &was_repo.rows {
            if was
                .pr
                .is_some_and(|pr| settled.contains(&(repo.clone(), pr)))
            {
                continue;
            }
            // A PR that left the review listings and merged or closed ends
            // its row's stage even when another source (a held sweep, a stale
            // ready tick) still supplies the row: reconcile that before
            // treating the surviving row as an ordinary stage change.
            if let (Some(pr), true) = (was.pr, now_repo.rows.contains_key(&issue)) {
                if let Some((exit, at)) =
                    resolved.get(&(repo.clone(), pr)).and_then(resolution_exit)
                {
                    let leaving = Leaving {
                        exit,
                        next_stage: None,
                        forge_at: Some(at),
                    };
                    let source = was_repo.sources.get(&issue).copied();
                    out.push(TelemetryRecord::StageOutcome(
                        diff.record(repo, issue, was, source, leaving),
                    ));
                    settled.insert((repo.clone(), pr));
                    continue;
                }
            }
            match now_repo.rows.get(&issue) {
                Some(now) if now.stage == was.stage => {}
                next if was.stage == FleetStage::ReadyWait => {
                    if next.is_none()
                        && !(pass.managed.contains(repo)
                            && prev.ready_complete(repo)
                            && pass.view.ready_complete(repo))
                    {
                        continue;
                    }
                    let source = was_repo.sources.get(&issue).copied();
                    match diff.ready_departure(repo, issue, was) {
                        Some(leaving) => out.push(TelemetryRecord::StageOutcome(
                            diff.record(repo, issue, was, source, leaving),
                        )),
                        None => {
                            let waiting = PendingReady {
                                row: was.clone(),
                                source,
                                next: next.map(|n| n.stage),
                                since: pass.now,
                                passes: 0,
                            };
                            pending.insert((repo.clone(), issue), waiting);
                        }
                    }
                }
                Some(now) => {
                    // A hold release is the removal of the hold label; `loom:pr`
                    // stays applied, so the hold's `unlabeled` event dates it
                    // (#11401), not a `labeled` one.
                    let released =
                        was.stage == FleetStage::MergeHold && now.stage == FleetStage::MergeWait;
                    let forge_at = match (now_repo.sources.get(&issue), now.pr) {
                        (Some(Source::Review), Some(pr)) if released => {
                            diff.release_instant(repo, pr)
                        }
                        (Some(Source::Review), Some(pr)) => diff.label_instant(repo, pr, now.stage),
                        _ => None,
                    };
                    let leaving = Leaving {
                        exit: StageExit::between(was.stage, now.stage),
                        next_stage: Some(now.stage),
                        forge_at,
                    };
                    let source = was_repo.sources.get(&issue).copied();
                    out.push(TelemetryRecord::StageOutcome(
                        diff.record(repo, issue, was, source, leaving),
                    ));
                }
                None => {
                    if !pass.managed.contains(repo) {
                        continue;
                    }
                    let facts = was.pr.and_then(|pr| resolved.get(&(repo.clone(), pr)));
                    let source = was_repo.sources.get(&issue).copied();
                    let (exit, forge_at) = match facts {
                        Some(PullFacts {
                            merged_at: Some(at),
                            ..
                        }) => (StageExit::Landed, Some(*at)),
                        Some(PullFacts {
                            closed_at: Some(at),
                            ..
                        }) => (StageExit::CutShort, Some(*at)),
                        Some(_) => (StageExit::CutShort, None),
                        // No PR said how: the issue's own close is the
                        // departure (#11398).
                        None => match diff.forge.issue(repo, issue) {
                            Some(IssueFacts {
                                closed_at: Some(at),
                            }) => (StageExit::Abandoned, Some(at)),
                            Some(IssueFacts { closed_at: None }) => (StageExit::Unknown, None),
                            None => {
                                log::debug!(
                                    "fleet.state: issues/{issue} of {repo} unread; retrying next pass"
                                );
                                pending_issue.insert(
                                    (repo.clone(), issue),
                                    PendingIssue {
                                        row: was.clone(),
                                        source,
                                        since: pass.now,
                                        passes: 0,
                                    },
                                );
                                continue;
                            }
                        },
                    };
                    let leaving = Leaving {
                        exit,
                        next_stage: None,
                        forge_at,
                    };
                    out.push(TelemetryRecord::StageOutcome(
                        diff.record(repo, issue, was, source, leaving),
                    ));
                }
            }
        }
    }
    settled.retain(|(repo, pr)| {
        pass.view
            .repos
            .get(repo)
            .is_some_and(|v| v.rows.values().any(|row| row.pr == Some(*pr)))
    });
    memory.settled = settled;
    memory.pending_ready = pending;
    memory.pending_issue = pending_issue;
    out
}

/// Settle the `ready_wait` rows still awaiting their forge departure: drop
/// one whose row is back in `ready_wait` (or whose repo is no longer
/// managed), emit one whose departure the forge now shows, and emit one
/// past [`PENDING_READY_MAX_PASSES`] as the view saw it (`unknown` unless
/// this host's own claim showed the next stage), with no forge exit instant.
/// Returns those still waiting.
fn settle_pending_ready<F: ForgeReads>(
    memory: &mut Memory,
    pass: &Pass<'_>,
    diff: &mut Diff<'_, F>,
    out: &mut Vec<TelemetryRecord>,
) -> BTreeMap<(String, u32), PendingReady> {
    let mut kept = BTreeMap::new();
    for ((repo, issue), waiting) in std::mem::take(&mut memory.pending_ready) {
        let back = pass
            .view
            .repos
            .get(&repo)
            .and_then(|r| r.rows.get(&issue))
            .is_some_and(|row| row.stage == FleetStage::ReadyWait);
        if back || !pass.managed.contains(&repo) {
            continue;
        }
        if let Some(leaving) = diff.ready_departure(&repo, issue, &waiting.row) {
            let record = diff.record(&repo, issue, &waiting.row, waiting.source, leaving);
            out.push(TelemetryRecord::StageOutcome(record));
            continue;
        }
        let passes = waiting.passes + 1;
        if passes < PENDING_READY_MAX_PASSES {
            kept.insert((repo, issue), PendingReady { passes, ..waiting });
            continue;
        }
        log::debug!(
            "fleet.state: {repo}#{issue} left ready_wait at {} with no forge departure; emitting it as seen",
            waiting.since
        );
        let leaving = Leaving {
            exit: waiting
                .next
                .map_or(StageExit::Unknown, |n| StageExit::between(FleetStage::ReadyWait, n)),
            next_stage: waiting.next,
            forge_at: None,
        };
        let mut record = diff.record(&repo, issue, &waiting.row, waiting.source, leaving);
        record.left_at = waiting.since;
        record.dwell_sec = dwell(record.exit, record.entered_at, record.left_at);
        out.push(TelemetryRecord::StageOutcome(record));
    }
    kept
}

/// Settle the vanished rows whose issue read failed (#11398): drop one whose
/// row is back or whose repo is no longer managed, emit one whose issue now
/// reads (`abandoned` at the close instant, or `unknown` while it is open),
/// and emit one past [`PENDING_ISSUE_MAX_PASSES`] as `unknown` at the pass
/// that first saw it gone, with no fact id. Returns those still waiting.
fn settle_pending_issue<F: ForgeReads>(
    memory: &mut Memory,
    pass: &Pass<'_>,
    diff: &mut Diff<'_, F>,
    out: &mut Vec<TelemetryRecord>,
) -> BTreeMap<(String, u32), PendingIssue> {
    let mut kept = BTreeMap::new();
    for ((repo, issue), waiting) in std::mem::take(&mut memory.pending_issue) {
        let back = pass
            .view
            .repos
            .get(&repo)
            .is_some_and(|r| r.rows.contains_key(&issue));
        if back || !pass.managed.contains(&repo) {
            continue;
        }
        let (exit, forge_at) = match diff.forge.issue(&repo, issue) {
            Some(IssueFacts {
                closed_at: Some(at),
            }) => (StageExit::Abandoned, Some(at)),
            Some(IssueFacts { closed_at: None }) => (StageExit::Unknown, None),
            None => {
                let passes = waiting.passes + 1;
                if passes < PENDING_ISSUE_MAX_PASSES {
                    kept.insert((repo, issue), PendingIssue { passes, ..waiting });
                    continue;
                }
                (StageExit::Unknown, None)
            }
        };
        let leaving = Leaving {
            exit,
            next_stage: None,
            forge_at,
        };
        let mut record = diff.record(&repo, issue, &waiting.row, waiting.source, leaving);
        if forge_at.is_none() {
            record.left_at = waiting.since;
            record.dwell_sec = dwell(record.exit, record.entered_at, record.left_at);
        }
        out.push(TelemetryRecord::StageOutcome(record));
    }
    kept
}

/// The next pass's memory: this pass's view and time, and each repo's latest
/// complete listing (a repo whose listing failed keeps its earlier one).
#[must_use]
pub fn remember(memory: Memory, pass: &Pass<'_>) -> Memory {
    let mut listed = memory.listed;
    listed.retain(|repo, _| pass.managed.contains(repo));
    listed.extend(pass.listed.iter().map(|(k, v)| (k.clone(), v.clone())));
    let mut curated = memory.curated;
    curated.retain(|repo, _| pass.managed.contains(repo));
    curated.extend(memory.curated_now.unwrap_or_default());
    Memory {
        curated,
        curated_now: None,
        pre_ready_done: memory.pre_ready_done,
        pre_ready_retry: memory.pre_ready_retry,
        listed,
        unread: memory.unread,
        settled: memory.settled,
        pending_ready: memory.pending_ready,
        pending_issue: memory.pending_issue,
        warm: BTreeMap::new(),
        view: Some(pass.view.clone()),
        at: Some(pass.now),
    }
}

/// Offer each record with valid provenance to `sink`; returns how many were
/// offered.
pub fn offer(records: Vec<TelemetryRecord>, sink: &super::FleetStateSink) -> usize {
    let mut offered = 0;
    for record in records {
        let valid = match &record {
            TelemetryRecord::PrResolved(r) => r.has_provenance(),
            TelemetryRecord::StageOutcome(r) => r.has_provenance(),
            _ => false,
        };
        if !valid {
            log::warn!("fleet.state: dropped a {} record: invalid provenance", record.kind());
            continue;
        }
        log::debug!("fleet.state: emit {}", record.kind());
        if let TelemetryRecord::StageOutcome(r) = &record {
            if let Some(reason) = r.missing_fact_id_reason() {
                log::debug!(
                    "fleet.state: {}#{} left {} with no fact id ({})",
                    r.repo,
                    r.issue,
                    r.stage.as_str(),
                    reason.as_str()
                );
                crate::telemetry::kinds::stage_outcome::note_fact_id_missing();
            }
        }
        sink.offer(record);
        offered += 1;
    }
    offered
}

/// The live reads, through `gh api` in each repo's own checkout.
pub struct GhForgeReads {
    pub roots: BTreeMap<String, PathBuf>,
}

fn parse_instant(raw: Option<&str>) -> Option<DateTime<Utc>> {
    raw.and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Utc))
}

impl GhForgeReads {
    fn root(&self, repo: &str) -> Option<&Path> {
        self.roots.get(repo).map(PathBuf::as_path)
    }
}

impl ForgeReads for GhForgeReads {
    fn pull(&mut self, repo: &str, number: u32) -> Option<PullFacts> {
        let pull = crate::observability::ops::stage_dwell::gh_json(
            self.root(repo)?,
            &format!("repos/{repo}/pulls/{number}"),
        )?;
        Some(PullFacts {
            merged_at: parse_instant(pull["merged_at"].as_str()),
            closed_at: parse_instant(pull["closed_at"].as_str()),
        })
    }

    fn issue(&mut self, repo: &str, number: u32) -> Option<IssueFacts> {
        let issue = crate::observability::ops::stage_dwell::gh_json(
            self.root(repo)?,
            &format!("repos/{repo}/issues/{number}"),
        )?;
        Some(IssueFacts {
            closed_at: parse_instant(issue["closed_at"].as_str()),
        })
    }

    fn label_history(&mut self, repo: &str, number: u32) -> Option<LabelHistory> {
        let root = self.root(repo)?.to_path_buf();
        LabelHistory::read(|page| {
            crate::observability::ops::stage_dwell::gh_json(
                &root,
                &format!(
                    "repos/{repo}/issues/{number}/events?per_page={EVENTS_PAGE_SIZE}&page={page}"
                ),
            )
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "outcomes_tests.rs"]
mod tests;
