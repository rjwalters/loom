//! The ETA tracker: which items are live, what stage each is in, which
//! estimates are waiting for their outcome, and what the journal must
//! record. Pure state: every forge read, file write and export happens in
//! the caller (`observability::eta`), which feeds observations in and acts
//! on the returned [`Effects`].
//!
//! # Where stages come from
//!
//! - A running sweep's stage comes from the bus: dispatch enters
//!   `sweep.curator`, and each completed phase (`curator`, `builder`,
//!   `judge`, `doctor`, `merge`) enters the next stage at that instant. A
//!   completed `judge` phase does not say which way the verdict went, so the
//!   item waits for the next phase event or the PR's labels to decide.
//! - After the sweep, the PR's review labels (the ETag-cached listings)
//!   drive the stage; a change between two listings is a transition
//!   observed at most one pass late.
//! - An item first seen mid-stage (a daemon restart) has only a lower bound
//!   on its entry (`updated_at`); the stage it leaves then has no duration.
//! - An approved PR held for a human (`merge_hold`, #10218) is an overlay on
//!   its pooled `merge_wait`, never a transition of it (`tracker_hold.rs`).
//!
//! # Outcomes
//!
//! `finish` resolves on the sweep's terminal bus event. `land` resolves on an
//! in-sweep `merge`, or on the PR's merge time when it leaves the review
//! listings. The three cases are the operator's (decision 4 on #9289):
//!
//! - **landed**: the PR merged, or the issue closed as **completed**.
//! - **abandoned**: the issue closed as **not planned**.
//! - Anything else — a PR closed unmerged, a sweep that ended before any PR —
//!   is **not an outcome**. The issue usually stays open and lands later
//!   through a replacement PR or a later sweep, so those estimates stay
//!   pending (`resolve` joins on repo, issue and kind, so the eventual
//!   landing scores them) until they expire after
//!   [`PENDING_MAX_AGE_DAYS`] — scored first as `censored` when their p90 is
//!   already behind them (#10233, `tracker_censor.rs`).
//!
//! A reopen starts a new series: on resolution every estimate of the series
//! emitted *after* the outcome instant is dropped unscored, so a second
//! landing cannot score the first landing's tail.
//!
//! # Outstanding forge reads ([`Effects::pr_checks`], [`Effects::issue_checks`])
//!
//! A PR that leaves the review listings, and an issue whose sweep ended before
//! any PR, need one forge read each to say how they ended. The tracker never
//! assumes the caller made that read: a check stays queued on the item and is
//! re-offered on **every** later listing pass until the answer arrives, so a
//! read the caller dropped over its budget or that failed is simply retried
//! (the [`crate::observability::ops::stage_dwell`] convention: work over the
//! budget waits for the next sample). While a check is outstanding the item
//! gets **no** `land` estimate — a merged PR must never keep receiving fresh
//! estimates because its read did not fit in a pass.
//!
//! # Which items get which estimates
//!
//! - `finish`: every item with a running issue sweep, from dispatch on.
//! - `land`: every item with a running sweep **from `sweep.curator` on** (a
//!   deliberate widening of the curated "land only from `sweep.builder`":
//!   the curator phase has local history like every other in-sweep phase,
//!   and no refusal reason fits it), and every open PR under a review label
//!   that closes an issue.
//! - `start` (#9326): every ready (`loom:issue`) issue on the last
//!   work-finder tick's dispatch plan that has no running sweep and no PR
//!   ([`Tracker::on_ready_queue`]). Such an item sits in `ready_wait` and
//!   also gets a `land` estimate, over the queue wait plus the post-dispatch
//!   chain. A row the plan gives no position (blocked) gets a
//!   `no_dispatch_plan` refusal for both; its dispatch settles `start`
//!   (`started`) and hands the item to the sweep's own `finish`/`land` chain.
//! - Nothing for intake or approval (no plan orders them).
//! - An item in `doctor` always has at least one rework round: `doctor` is
//!   entered only through a rejection, so an item first seen there (after a
//!   restart) is counted as having taken one.

use super::emit::{EmitState, Signature, Trigger};
use super::explanation::Explanation;
use super::journal::JournalEntry;
use super::labels::stage_from_pr_labels;
use super::score::{score, EstimateSummary, OutcomeKind, Score, StageObservation};
use super::{
    AgeSource, CurrentStage, CurrentState, DispatchInput, EstimateInput, Heuristic, Kind,
    NoEstimateReason, Provenance, Registry, Stage, StageSamples, Subject,
};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;

/// Pending estimates older than this are dropped unresolved.
pub const PENDING_MAX_AGE_DAYS: i64 = 30;

/// A repeat of the same completed phase within this many seconds is a
/// duplicate publication (registry and sweep child), not a new completion.
pub const PHASE_DEDUPE_SECS: i64 = 120;

/// Most pending estimates kept; the oldest go first.
pub const MAX_PENDING: usize = 50_000;

/// An item: one issue in one repo.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ItemKey {
    /// Lowercased `owner/repo`.
    pub repo: String,
    /// Issue.
    pub issue: u32,
}

impl ItemKey {
    /// The key for `repo#issue`.
    #[must_use]
    pub fn new(repo: &str, issue: u32) -> Self {
        ItemKey {
            repo: repo.to_ascii_lowercase(),
            issue,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StageTrack {
    stage: Stage,
    entered_at: DateTime<Utc>,
    source: AgeSource,
    /// `entered_at` was observed, not a lower bound.
    exact: bool,
}

#[derive(Debug, Clone, Default)]
struct Item {
    repo: String,
    issue: u32,
    pr_number: Option<u32>,
    sweep_id: Option<String>,
    sweep_running: bool,
    stage: Option<StageTrack>,
    /// A completed `judge` phase whose verdict is not yet known.
    verdict_pending_since: Option<DateTime<Utc>>,
    rework_rounds: u32,
    labels: Vec<String>,
    pr_created_at: Option<DateTime<Utc>>,
    refused: Option<NoEstimateReason>,
    observed: Vec<StageObservation>,
    in_review_listing: bool,
    landed: bool,
    /// A `pulls/{n}` read is outstanding: the PR left the review listings and
    /// nothing has said yet how it ended. Re-offered every pass until it is
    /// answered; no `land` estimate meanwhile.
    needs_pr_read: bool,
    /// An `issues/{n}` read is outstanding: the sweep ended before any PR, or
    /// the PR closed unmerged, so only the issue's own state decides.
    needs_issue_read: bool,
    /// The last completed phase and when, to drop duplicate publications.
    last_phase: Option<(String, DateTime<Utc>)>,
    emit: BTreeMap<(Kind, String), EmitState>,
    /// Whether each series' last emission answered (#10233, `tracker_answers.rs`).
    answered: BTreeMap<(Kind, String), bool>,
    /// On the last dispatch plan as a ready row, not yet dispatched (#9326).
    in_ready_queue: bool,
    /// Its plan position, when the plan gives it one.
    ready: Option<DispatchInput>,
    /// Issue, sweep and verdict observations, each with its `known_at`
    /// (#10231).
    facts: item_facts::ItemFacts,
    /// The operator-hold overlay on a pooled `merge_wait` (#10218).
    hold: hold::Hold,
}

/// A PR row from a review-label listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrView {
    /// PR number.
    pub number: u32,
    /// The issue it closes (first closing reference).
    pub issue: u32,
    /// Its labels.
    pub labels: Vec<String>,
    /// Created.
    pub created_at: Option<DateTime<Utc>>,
    /// Last updated: a lower bound on the current stage's entry.
    pub updated_at: Option<DateTime<Utc>>,
}

/// How a PR that left the review listings ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrState {
    /// Merged at.
    Merged(DateTime<Utc>),
    /// Closed without merging.
    Closed,
    /// Still open, just no longer under a review label.
    Open,
}

/// The state of an issue whose `land` outcome only the issue itself can
/// settle (operator decision 4 on #9289: closed-as-completed lands, closed-as
/// -not-planned is abandoned, open is neither).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueState {
    /// Closed as completed, at.
    ClosedCompleted(DateTime<Utc>),
    /// Closed as not planned, at.
    ClosedNotPlanned(DateTime<Utc>),
    /// Still open: the work has not landed and was not abandoned.
    Open,
}

/// One resolved estimate.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// The estimate, as it was emitted.
    pub estimate: EstimateSummary,
    /// Its score.
    pub score: Score,
    /// What resolved it: `bus`, `pulls_read`, `sweep_terminal`.
    pub outcome_source: String,
    /// How late the resolution may be, seconds.
    pub outcome_resolution_sec: Option<i64>,
    /// The sweep's terminal class for `finish` (`exited`, `crashed`).
    pub result: Option<String>,
}

/// What one tracker call produced.
#[derive(Debug, Default)]
pub struct Effects {
    /// Rows to append to the stage journal now.
    pub journal: Vec<JournalEntry>,
    /// Estimates resolved by this call.
    pub outcomes: Vec<Resolved>,
    /// Items whose state changed: estimate them now.
    pub dirty: Vec<ItemKey>,
    /// PRs whose state is still unknown: read `pulls/{n}`. Re-offered every
    /// pass until [`Tracker::on_pr_resolved`] answers it.
    pub pr_checks: Vec<(ItemKey, u32)>,
    /// Issues whose state is still unknown: read `issues/{n}`. Re-offered
    /// every pass until [`Tracker::on_issue_resolved`] answers it.
    pub issue_checks: Vec<ItemKey>,
}

impl Effects {
    fn merge(&mut self, other: Effects) {
        self.journal.extend(other.journal);
        self.outcomes.extend(other.outcomes);
        self.dirty.extend(other.dirty);
        self.pr_checks.extend(other.pr_checks);
        self.issue_checks.extend(other.issue_checks);
    }
}

/// An estimate that was, or would have been, emitted.
#[derive(Debug, Clone, PartialEq)]
pub struct Emission {
    /// Why now.
    pub trigger: Trigger,
    /// The estimate.
    pub explanation: Explanation,
    /// Whether this is the `current` heuristic's estimate for its kind — the
    /// one every existing consumer reads (#9328).
    ///
    /// A `false` here is a **shadow** estimate: computed, journaled and
    /// emitted as its own `eta.estimate` so a candidate accumulates a live
    /// record, but never the subject's answer. Nothing downstream may treat a
    /// shadow estimate as the item's ETA.
    pub primary: bool,
}

/// Context the caller supplies to every estimate.
#[derive(Clone)]
pub struct EstimateContext<'a> {
    /// The heuristics.
    pub registry: &'a Registry,
    /// Configured current heuristic per kind.
    pub current_start: Option<&'a str>,
    /// Configured current heuristic per kind.
    pub current_finish: Option<&'a str>,
    /// Configured current heuristic per kind.
    pub current_land: Option<&'a str>,
    /// History observed so far.
    pub history: &'a StageSamples,
    /// Refresh cadence.
    pub refresh_secs: u64,
    /// This host, recorded as a feature.
    pub host_id: Option<&'a str>,
    /// Resolved repo ids by lowercased slug.
    pub repo_ids: &'a BTreeMap<String, u64>,
}

/// The tracker.
#[derive(Debug)]
pub struct Tracker {
    items: BTreeMap<ItemKey, Item>,
    pending: Vec<EstimateSummary>,
    loom: Provenance,
    /// Pending estimates dropped because they were emitted after their own
    /// outcome (a late resolution's tail; a reopen must not score them).
    orphaned: usize,
    /// Pending estimates dropped by the [`MAX_PENDING`] cap.
    cap_dropped: usize,
    /// The last pass's fleet view and dispatch plan, for `features` (#10201).
    context: features::PassContext,
    /// Latest queue-friction readings (#10193), copied onto every estimate's
    /// features. Filled by the caller's forge reads, never by the tracker.
    pub friction: super::friction::FrictionBook,
    /// Per-pass answer states, drained by the caller (#10233).
    answers: Vec<answers::PassAnswers>,
}

/// What [`Tracker::drain_dropped`] reports.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Dropped {
    /// Emitted after their outcome instant, so never scored.
    pub orphaned: usize,
    /// Dropped by the [`MAX_PENDING`] cap.
    pub over_cap: usize,
}

/// The stage entered when sweep phase `phase` completes, for the phases that
/// name their successor.
fn next_after_phase(phase: &str) -> Option<Stage> {
    match phase {
        "curator" => Some(Stage::SweepBuilder),
        "builder" | "doctor" => Some(Stage::ReviewWait),
        _ => None,
    }
}

impl Tracker {
    /// An empty tracker observing as `loom`.
    #[must_use]
    pub fn new(loom: Provenance) -> Self {
        Tracker {
            items: BTreeMap::new(),
            pending: Vec::new(),
            loom,
            orphaned: 0,
            cap_dropped: 0,
            context: features::PassContext::default(),
            friction: super::friction::FrictionBook::default(),
            answers: Vec::new(),
        }
    }

    /// Pending estimates dropped since the last call, and reset.
    pub fn drain_dropped(&mut self) -> Dropped {
        Dropped {
            orphaned: std::mem::take(&mut self.orphaned),
            over_cap: std::mem::take(&mut self.cap_dropped),
        }
    }

    /// Pending estimates, oldest first.
    #[must_use]
    pub fn pending(&self) -> &[EstimateSummary] {
        &self.pending
    }

    /// Restore pending estimates persisted by an earlier process.
    pub fn restore_pending(&mut self, pending: Vec<EstimateSummary>) {
        self.pending = pending;
        self.pending.sort_by_key(|p| p.as_of);
    }

    /// Items currently tracked.
    #[must_use]
    pub fn item_keys(&self) -> Vec<ItemKey> {
        self.items.keys().cloned().collect()
    }

    fn item(&mut self, repo: &str, issue: u32) -> &mut Item {
        self.items
            .entry(ItemKey::new(repo, issue))
            .or_insert_with(|| Item {
                repo: repo.to_string(),
                issue,
                ..Item::default()
            })
    }

    fn row(&self, event: &str, item: &Item, at: DateTime<Utc>) -> JournalEntry {
        let mut row = JournalEntry::new(event, &item.repo, at, &self.loom);
        row.issue = Some(item.issue);
        row.pr_number = item.pr_number;
        row.sweep_id = item.sweep_id.clone();
        row
    }

    /// Move `key` to `next` at `at`, closing the current stage as a completed
    /// one: an exactly-observed entry yields a `duration_sec` and a history
    /// observation. Returns the journal row for the boundary.
    #[allow(clippy::too_many_arguments)]
    fn transition(
        &mut self,
        key: &ItemKey,
        next: Option<Stage>,
        at: DateTime<Utc>,
        source: AgeSource,
        event: &str,
        in_sweep: bool,
        resolution_sec: i64,
    ) -> JournalEntry {
        self.transition_opts(key, next, at, source, event, in_sweep, resolution_sec, true)
    }

    /// [`Self::transition`], but the stage being left did **not** complete —
    /// it ended by closure (a PR closed unmerged). The row names the stage it
    /// leaves and carries no `duration_sec`, so
    /// [`JournalEntry::history_sample`] skips it and no truncated stage enters
    /// the distributions.
    ///
    /// It does carry a `censored_sec` **lower bound** when the entry instant
    /// was exactly observed (#9328): the stage provably lasted at least that
    /// long without completing. Only `land-v2`'s Kaplan–Meier grids read it
    /// ([`JournalEntry::censored_sample`]); every v1 distribution is
    /// unchanged.
    #[allow(clippy::too_many_arguments)]
    fn transition_unobserved(
        &mut self,
        key: &ItemKey,
        next: Option<Stage>,
        at: DateTime<Utc>,
        source: AgeSource,
        event: &str,
        in_sweep: bool,
        resolution_sec: i64,
    ) -> JournalEntry {
        self.transition_opts(key, next, at, source, event, in_sweep, resolution_sec, false)
    }

    #[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
    fn transition_opts(
        &mut self,
        key: &ItemKey,
        next: Option<Stage>,
        at: DateTime<Utc>,
        source: AgeSource,
        event: &str,
        in_sweep: bool,
        resolution_sec: i64,
        completed: bool,
    ) -> JournalEntry {
        let item = self.items.get(key).cloned().unwrap_or_default();
        let mut row = self.row(event, &item, at);
        row.in_sweep = in_sweep;
        row.next_stage = next;
        row.left_at = Some(at);
        row.resolution_sec = Some(resolution_sec);
        let observed_source = match source {
            AgeSource::Bus => "bus",
            AgeSource::LabelEvent => "label_event",
            AgeSource::Checkpoint => "checkpoint",
            AgeSource::UpdatedAtLowerBound => "updated_at",
            AgeSource::TrackerObserved => "listing",
        };
        let item = self
            .items
            .get_mut(key)
            .unwrap_or_else(|| unreachable!("caller created it"));
        if let Some(old) = item.stage.take() {
            row.stage = Some(old.stage);
            if old.exact && completed {
                row.entered_at = Some(old.entered_at);
                row.duration_sec = Some((at - old.entered_at).num_seconds().max(0));
                item.observed.push(StageObservation {
                    stage: old.stage,
                    entered_at: old.entered_at,
                    left_at: at,
                    source: observed_source.to_string(),
                });
            } else if old.exact {
                // Cut short, but the entry instant was exact: a right-censored
                // lower bound, never a duration (#9328).
                row.entered_at = Some(old.entered_at);
                row.censored_sec = Some((at - old.entered_at).num_seconds().max(0));
            }
        }
        item.stage = next.map(|stage| StageTrack {
            stage,
            entered_at: at,
            source,
            exact: true,
        });
        row
    }

    /// `sweep.global.dispatch` for an issue sweep.
    pub fn on_dispatch(
        &mut self,
        repo: &str,
        issue: u32,
        sweep_id: &str,
        at: DateTime<Utc>,
    ) -> Effects {
        let key = ItemKey::new(repo, issue);
        let item = self.item(repo, issue);
        item.sweep_id = Some(sweep_id.to_string());
        item.sweep_running = true;
        item.landed = false;
        item.verdict_pending_since = None;
        // The dispatch is the `start` outcome (#9326). From here the sweep
        // drives the item; the queue wait leaves no duration (its entry is
        // only the tracker's first sight of the row).
        let was_ready = std::mem::take(&mut item.in_ready_queue);
        item.ready = None;
        if was_ready {
            item.refused = None;
        }
        let outcomes =
            self.resolve(&key, Kind::Start, OutcomeKind::Started, at, "bus", Some(0), None);
        let mut row = self.transition(
            &key,
            Some(Stage::SweepCurator),
            at,
            AgeSource::Bus,
            "sweep.dispatch",
            true,
            0,
        );
        row.raw = serde_json::json!({"sweep_id": sweep_id});
        Effects {
            journal: vec![row],
            outcomes,
            dirty: vec![key],
            ..Effects::default()
        }
    }

    /// `sweep.issue.{N}.phase`: phase `phase` completed at `at`.
    ///
    /// The registry and the sweep child can both publish the same
    /// completion; a repeat of the last phase within [`PHASE_DEDUPE_SECS`]
    /// is journaled raw and otherwise ignored.
    pub fn on_phase(
        &mut self,
        repo: &str,
        issue: u32,
        phase: &str,
        pr_number: Option<u32>,
        at: DateTime<Utc>,
    ) -> Effects {
        let key = ItemKey::new(repo, issue);
        let raw = serde_json::json!({"phase": phase, "pr_number": pr_number});
        let item = self.item(repo, issue);
        item.sweep_running = true;
        if pr_number.is_some() {
            item.pr_number = pr_number;
        }
        let repeat = item.last_phase.as_ref().is_some_and(|(last, last_at)| {
            last == phase && (at - *last_at).num_seconds().abs() <= PHASE_DEDUPE_SECS
        });
        let mut effects = Effects::default();
        if repeat {
            let item = item.clone();
            let mut row = self.row("sweep.phase.repeat", &item, at);
            row.in_sweep = true;
            row.raw = raw;
            effects.journal.push(row);
            return effects;
        }
        item.last_phase = Some((phase.to_string(), at));

        // A verdict the next phase settles: `doctor` means it was a
        // rejection and the Doctor ran from the verdict until now; `merge`
        // means approval and the merge ran from the verdict until now.
        if let Some(since) = item.verdict_pending_since {
            let settled = match phase {
                "doctor" => Some((Stage::Doctor, "fail")),
                "merge" => Some((Stage::MergeWait, "pass")),
                _ => None,
            };
            if let Some((stage, verdict)) = settled {
                let row = self.settle_verdict(&key, stage, verdict, since, true, at);
                effects.journal.push(row);
            }
        }

        // A completion of a stage other than the current one (a restart, a
        // resumed sweep): the current stage's duration is unknown, so it is
        // dropped rather than attributed to the wrong stage.
        let completed = Stage::from_sweep_phase(phase);
        let item = self
            .items
            .get_mut(&key)
            .unwrap_or_else(|| unreachable!("created above"));
        let current = item.stage.as_ref().map(|s| s.stage);
        if current.is_some() && current != completed {
            item.stage = None;
        }
        if phase == "doctor" && current != Some(Stage::Doctor) {
            // A Doctor pass we never saw start: the rework still happened.
            item.rework_rounds += 1;
        }
        let mut row = match phase {
            "judge" => {
                let row = self.transition(&key, None, at, AgeSource::Bus, "sweep.phase", true, 0);
                if let Some(item) = self.items.get_mut(&key) {
                    item.verdict_pending_since = Some(at);
                }
                row
            }
            "merge" => {
                let row = self.transition(&key, None, at, AgeSource::Bus, "sweep.phase", true, 0);
                effects.outcomes.extend(self.resolve(
                    &key,
                    Kind::Land,
                    OutcomeKind::Landed,
                    at,
                    "bus",
                    Some(0),
                    None,
                ));
                if let Some(item) = self.items.get_mut(&key) {
                    item.landed = true;
                }
                row
            }
            _ => self.transition(
                &key,
                next_after_phase(phase),
                at,
                AgeSource::Bus,
                "sweep.phase",
                true,
                0,
            ),
        };
        row.raw = raw;
        effects.journal.push(row);
        effects.dirty.push(key);
        effects
    }

    /// Record a verdict that entered `stage` at `since`.
    fn settle_verdict(
        &mut self,
        key: &ItemKey,
        stage: Stage,
        verdict: &str,
        since: DateTime<Utc>,
        in_sweep: bool,
        known_at: DateTime<Utc>,
    ) -> JournalEntry {
        let item = self
            .items
            .get_mut(key)
            .unwrap_or_else(|| unreachable!("caller created it"));
        item.verdict_pending_since = None;
        let attempt = item.rework_rounds + 1;
        item.facts
            .note_verdict(attempt, verdict == "pass", known_at);
        if stage == Stage::Doctor {
            item.rework_rounds += 1;
        }
        item.stage = Some(StageTrack {
            stage,
            entered_at: since,
            source: AgeSource::Bus,
            exact: true,
        });
        let item = item.clone();
        let mut row = self.row("verdict", &item, since);
        row.in_sweep = in_sweep;
        row.verdict = Some(verdict.to_string());
        row.attempt = Some(attempt);
        row.next_stage = Some(stage);
        row
    }

    /// A sweep reached a terminal state (`class`: `exited` or `crashed`).
    pub fn on_terminal(
        &mut self,
        repo: &str,
        issue: u32,
        class: &str,
        exit_code: Option<i32>,
        at: DateTime<Utc>,
    ) -> Effects {
        let key = ItemKey::new(repo, issue);
        let Some(item) = self.items.get(&key) else {
            return Effects::default();
        };
        if !item.sweep_running {
            return Effects::default();
        }
        let mut effects = Effects::default();
        let mut row = self.row("sweep.terminal", item, at);
        row.in_sweep = true;
        row.raw = serde_json::json!({"class": class, "exit_code": exit_code});
        effects.journal.push(row);
        effects.outcomes.extend(self.resolve(
            &key,
            Kind::Finish,
            OutcomeKind::Finished,
            at,
            "sweep_terminal",
            Some(0),
            Some(class.to_string()),
        ));
        let item = self
            .items
            .get_mut(&key)
            .unwrap_or_else(|| unreachable!("checked above"));
        item.sweep_running = false;
        if item.landed {
            self.items.remove(&key);
            return effects;
        }
        // The sweep ending says nothing about whether the work lands: a
        // crash, a budget or rate-limit exit, a hold and a Curator closing
        // the issue all end a sweep, and all but the last usually land later
        // (operator decision 4 on #9289). So queue the one read that can tell
        // them apart and keep the `land` estimates pending meanwhile — never
        // guess `abandoned` here.
        if item.pr_number.is_none() {
            item.needs_issue_read = true;
        } else if !item.in_review_listing {
            // A PR exists but no review listing covers it; one read says
            // whether it merged, closed or is simply unlabelled.
            item.needs_pr_read = true;
        }
        effects.dirty.push(key);
        effects
    }

    /// A complete review-label listing for `repo` (every open PR under
    /// review-requested, changes-requested or approved), observed at `now`.
    pub fn on_listing(
        &mut self,
        repo: &str,
        prs: &[PrView],
        now: DateTime<Utc>,
        resolution_sec: i64,
    ) -> Effects {
        let mut effects = Effects::default();
        let repo_key = repo.to_ascii_lowercase();
        let mut seen = Vec::new();
        for pr in prs {
            let key = ItemKey::new(repo, pr.issue);
            seen.push(key.clone());
            let is_new = !self.items.contains_key(&key);
            let item = self.item(repo, pr.issue);
            item.pr_number = Some(pr.number);
            item.labels = pr.labels.clone();
            item.pr_created_at = pr.created_at;
            item.in_review_listing = true;
            // An open PR under a review label answers both outstanding checks
            // by itself: the PR is live, so it has neither merged nor closed,
            // and the issue is being worked rather than settled. The PR
            // leaving the listing later re-queues the read.
            item.needs_pr_read = false;
            item.needs_issue_read = false;
            // #10218: `merge_hold` is an overlay; see `tracker_hold.rs`.
            if self.hold_listing(&key, pr, is_new, now, resolution_sec, &mut effects) {
                continue;
            }
            let item = self.item(repo, pr.issue);
            let resolved = stage_from_pr_labels(&pr.labels);
            let before = (item.refused, item.stage.as_ref().map(|s| s.stage), item.rework_rounds);
            item.refused = resolved.err();
            let sweep_drives = item.sweep_running && item.verdict_pending_since.is_none();
            let Ok(stage) = resolved else {
                if item.refused != before.0 {
                    effects.dirty.push(key);
                }
                continue;
            };
            if is_new || item.stage.is_none() && item.verdict_pending_since.is_none() {
                // First sight mid-stage: entry is at most `updated_at` ago.
                let entered_at = pr.updated_at.unwrap_or(now).min(now);
                if stage == Stage::Doctor {
                    item.rework_rounds = item.rework_rounds.max(1);
                }
                item.stage = Some(StageTrack {
                    stage,
                    entered_at,
                    source: AgeSource::UpdatedAtLowerBound,
                    exact: false,
                });
                let item = item.clone();
                let mut row = self.row("label.first_seen", &item, now);
                row.next_stage = Some(stage);
                row.raw = serde_json::json!({"labels": pr.labels, "updated_at": pr.updated_at});
                effects.journal.push(row);
                effects.dirty.push(key);
                continue;
            }
            if let Some(since) = item.verdict_pending_since {
                // The labels settle a pending in-sweep verdict.
                let settled = match stage {
                    Stage::Doctor => Some((Stage::Doctor, "fail")),
                    Stage::MergeWait => Some((Stage::MergeWait, "pass")),
                    _ => None,
                };
                if let Some((stage, verdict)) = settled {
                    let mut row = self.settle_verdict(&key, stage, verdict, since, true, now);
                    row.raw = serde_json::json!({"labels": pr.labels});
                    effects.journal.push(row);
                    effects.dirty.push(key);
                }
                continue;
            }
            if sweep_drives {
                continue;
            }
            let current = item.stage.as_ref().map(|s| s.stage);
            if current == Some(stage) {
                if item.refused != before.0 {
                    effects.dirty.push(key);
                }
                continue;
            }
            // An external transition between two listings.
            let verdict = match (current, stage) {
                (Some(Stage::ReviewWait), Stage::Doctor) => Some("fail"),
                (Some(Stage::ReviewWait), Stage::MergeWait) => Some("pass"),
                _ => None,
            };
            let attempt = item.rework_rounds + 1;
            if let Some(verdict) = verdict {
                item.facts.note_verdict(attempt, verdict == "pass", now);
            }
            if stage == Stage::Doctor {
                item.rework_rounds += 1;
            }
            let mut row = self.transition(
                &key,
                Some(stage),
                now,
                AgeSource::TrackerObserved,
                "label.transition",
                false,
                resolution_sec,
            );
            if let Some(verdict) = verdict {
                row.verdict = Some(verdict.to_string());
                row.attempt = Some(attempt);
            }
            row.raw = serde_json::json!({"labels": pr.labels});
            effects.journal.push(row);
            effects.dirty.push(key);
        }
        // Tracked PRs of this repo that left every review listing, plus every
        // read still outstanding from an earlier pass. A check the caller
        // could not make — over its read budget, timed out, non-2xx — is
        // simply offered again here, so nothing is ever dropped permanently.
        for (key, item) in &mut self.items {
            if key.repo != repo_key {
                continue;
            }
            if item.in_review_listing && !seen.contains(key) {
                item.in_review_listing = false;
                if item.pr_number.is_some() {
                    item.needs_pr_read = true;
                } else {
                    item.needs_issue_read = true;
                }
            }
            match (item.needs_pr_read, item.pr_number) {
                (true, Some(pr)) => effects.pr_checks.push((key.clone(), pr)),
                // Nothing to read: fall back to the issue.
                (true, None) => {
                    item.needs_pr_read = false;
                    item.needs_issue_read = true;
                }
                (false, _) => {}
            }
            if item.needs_issue_read {
                effects.issue_checks.push(key.clone());
            }
        }
        effects
    }

    /// The state of a PR that left the review listings. Answers the
    /// outstanding [`Effects::pr_checks`] entry for `key`; until this is
    /// called the check keeps being re-offered and the item gets no `land`
    /// estimate.
    pub fn on_pr_resolved(&mut self, key: &ItemKey, state: PrState, now: DateTime<Utc>) -> Effects {
        let mut effects = Effects::default();
        let Some(item) = self.items.get_mut(key) else {
            return effects;
        };
        item.needs_pr_read = false;
        let item = &*item;
        if item.landed {
            return effects;
        }
        let pr = item.pr_number;
        effects.journal.extend(self.hold_resolved(key, state, now));
        match state {
            PrState::Merged(at) => {
                let mut row =
                    self.transition(key, None, at, AgeSource::LabelEvent, "pr.resolved", false, 0);
                row.raw = serde_json::json!({"pr": pr, "state": "merged"});
                effects.journal.push(row);
                let late = (now - at).num_seconds().max(0);
                effects.outcomes.extend(self.resolve(
                    key,
                    Kind::Land,
                    OutcomeKind::Landed,
                    at,
                    "pulls_read",
                    Some(late),
                    None,
                ));
                self.finish_item(key);
            }
            PrState::Closed => {
                // The stage did not complete, it was cut short by the
                // closure: journal it raw so it never enters the stage
                // distributions as if the stage had finished normally.
                let mut row = self.transition_unobserved(
                    key,
                    None,
                    now,
                    AgeSource::TrackerObserved,
                    "pr.resolved",
                    false,
                    0,
                );
                row.raw = serde_json::json!({"pr": pr, "state": "closed"});
                effects.journal.push(row);
                // A PR closed unmerged is not an abandonment: a replacement
                // PR for the same issue is the common case. Only the issue's
                // own state decides (operator decision 4 on #9289).
                if let Some(item) = self.items.get_mut(key) {
                    item.needs_issue_read = true;
                }
                effects.issue_checks.push(key.clone());
            }
            PrState::Open => {
                if let Some(item) = self.items.get_mut(key) {
                    let reason = super::labels::check_holds(&item.labels)
                        .err()
                        .unwrap_or(NoEstimateReason::UnknownStage);
                    item.refused = Some(reason);
                }
                effects.dirty.push(key.clone());
            }
        }
        effects
    }

    /// The state of an issue whose `land` outcome nothing else can settle:
    /// the sweep ended before any PR, or the PR closed unmerged. Answers the
    /// outstanding [`Effects::issue_checks`] entry for `key`.
    ///
    /// Per operator decision 4 on #9289: closed-as-completed **lands** at
    /// `closed_at` (that is the "or the issue closed as completed" half of
    /// `land`), closed-as-not-planned is **abandoned**, and an open issue is
    /// neither — its estimates stay pending for the landing that is still to
    /// come.
    pub fn on_issue_resolved(
        &mut self,
        key: &ItemKey,
        state: IssueState,
        now: DateTime<Utc>,
    ) -> Effects {
        let mut effects = Effects::default();
        let Some(item) = self.items.get_mut(key) else {
            return effects;
        };
        item.needs_issue_read = false;
        if item.landed {
            return effects;
        }
        let item = item.clone();
        let (state_name, at) = match state {
            IssueState::ClosedCompleted(at) => ("closed_completed", Some(at)),
            IssueState::ClosedNotPlanned(at) => ("closed_not_planned", Some(at)),
            IssueState::Open => ("open", None),
        };
        let mut row = self.row("issue.resolved", &item, now);
        row.left_at = at;
        row.raw = serde_json::json!({"issue": key.issue, "state": state_name, "closed_at": at});
        effects.journal.push(row);
        match state {
            IssueState::ClosedCompleted(at) => {
                let late = (now - at).num_seconds().max(0);
                effects.outcomes.extend(self.resolve(
                    key,
                    Kind::Land,
                    OutcomeKind::Landed,
                    at,
                    "issues_read",
                    Some(late),
                    None,
                ));
                self.finish_item(key);
            }
            IssueState::ClosedNotPlanned(at) => {
                let late = (now - at).num_seconds().max(0);
                effects.outcomes.extend(self.resolve(
                    key,
                    Kind::Land,
                    OutcomeKind::Abandoned,
                    at,
                    "issues_read",
                    Some(late),
                    None,
                ));
                self.finish_item(key);
            }
            // Still open and nothing is tracking it any more: forget the
            // item, keep its estimates pending. `resolve` joins on repo,
            // issue and kind, so the later PR or sweep that lands the work
            // scores them.
            IssueState::Open => self.retire_open_item(key),
        }
        effects
    }

    /// Drop an item that has nothing left to observe (no running sweep, no
    /// PR under review), leaving its pending estimates in place.
    fn retire_open_item(&mut self, key: &ItemKey) {
        let keep = self
            .items
            .get(key)
            .is_some_and(|i| i.sweep_running || i.in_review_listing || i.in_ready_queue);
        if !keep {
            self.items.remove(key);
        }
    }

    fn finish_item(&mut self, key: &ItemKey) {
        let running = self.items.get(key).is_some_and(|i| i.sweep_running);
        if running {
            if let Some(item) = self.items.get_mut(key) {
                item.landed = true;
            }
        } else {
            self.items.remove(key);
        }
    }

    /// Score and drop every pending `kind` estimate of `key` made at or
    /// before `actual_at`.
    ///
    /// Estimates of the same series made *after* `actual_at` are dropped
    /// unscored: they describe a landing that had already happened (a late
    /// `pulls_read`/`issues_read` resolution, up to a pass or more behind), so
    /// scoring them against this outcome would be wrong — and leaving them
    /// pending would let a **reopen**'s second landing score them, when the
    /// rule is that the first outcome stands and a reopen starts a new series.
    #[allow(clippy::too_many_arguments)]
    fn resolve(
        &mut self,
        key: &ItemKey,
        kind: Kind,
        outcome: OutcomeKind,
        actual_at: DateTime<Utc>,
        source: &str,
        resolution_sec: Option<i64>,
        result: Option<String>,
    ) -> Vec<Resolved> {
        let observed = self
            .items
            .get(key)
            .map(|i| i.observed.clone())
            .unwrap_or_default();
        let mut matching = Vec::new();
        let mut keep = Vec::new();
        for estimate in std::mem::take(&mut self.pending) {
            let same_series = estimate.kind == kind
                && estimate.issue == key.issue
                && estimate.repo.eq_ignore_ascii_case(&key.repo);
            if !same_series {
                keep.push(estimate);
            } else if estimate.as_of <= actual_at {
                matching.push(estimate);
            } else {
                self.orphaned += 1;
            }
        }
        self.pending = keep;
        matching
            .into_iter()
            .map(|estimate| {
                let stages: Vec<StageObservation> = observed
                    .iter()
                    .filter(|o| o.left_at > estimate.as_of)
                    .cloned()
                    .collect();
                Resolved {
                    score: score(&estimate, outcome, actual_at, &stages),
                    estimate,
                    outcome_source: source.to_string(),
                    outcome_resolution_sec: resolution_sec,
                    result: result.clone(),
                }
            })
            .collect()
    }

    fn input_for(
        &self,
        key: &ItemKey,
        item: &Item,
        kind: Kind,
        ctx: &EstimateContext<'_>,
        now: DateTime<Utc>,
    ) -> Option<EstimateInput> {
        if kind == Kind::Finish && !item.sweep_running {
            return None;
        }
        let ready_only = features::ready_only(item);
        if kind == Kind::Start && !ready_only {
            return None;
        }
        if kind == Kind::Land && item.landed {
            return None;
        }
        if kind == Kind::Land && (item.needs_pr_read || item.needs_issue_read) {
            // A read is outstanding: the PR may already have merged, so a
            // fresh `land` estimate here would be a phantom live ETA. Wait
            // for the answer instead (it is retried every pass).
            return None;
        }
        let current = if let Some(held) = hold::held_stage(item, now) {
            CurrentState::At(held)
        } else if let Some(reason) = item.refused {
            CurrentState::Refused(reason)
        } else if item.verdict_pending_since.is_some() {
            // Between a verdict and the event that says which way it went:
            // settles within a pass, so no estimate is made meanwhile.
            return None;
        } else {
            let stage = item.stage.as_ref()?;
            if kind == Kind::Finish && !item.sweep_running {
                return None;
            }
            CurrentState::At(CurrentStage {
                stage: stage.stage,
                entered_at: Some(stage.entered_at),
                age_sec: (now - stage.entered_at).num_seconds().max(0),
                age_source: stage.source,
                rework_rounds: item.rework_rounds,
                episode_entered_at: hold::episode_entered_at(item),
            })
        };
        let mut subject =
            Subject::new(&item.repo, ctx.repo_ids.get(&key.repo).copied(), item.issue);
        subject.pr_number = item.pr_number;
        subject.sweep_id = item.sweep_id.clone().filter(|_| item.sweep_running);
        let (features, omitted) =
            self.recorded_features(key, item, &hold::described(&current), ctx, now, false);
        Some(EstimateInput {
            subject,
            as_of: now,
            current,
            features,
            features_omitted: omitted,
            provenance: self.loom.clone(),
            dispatch: item.ready.clone().filter(|_| ready_only),
        })
    }

    /// Estimate `keys` (every item when `None`) at `now`, returning the
    /// estimates the emit policy lets out. Each emitted estimate becomes
    /// pending until its outcome.
    ///
    /// # Shadow mode (#9328)
    ///
    /// **Every** registered heuristic of each kind is estimated, not only
    /// `current` — [`Emission::primary`] marks which one is the subject's
    /// answer. The additions are strictly additive: `current`'s estimate is
    /// computed from the identical input against the identical history and is
    /// emitted first, so no existing consumer sees a different number, only
    /// extra rows beside it.
    ///
    /// Each `(kind, heuristic)` series keeps its own emit state, so a shadow
    /// estimate's refresh cadence never gates `current`'s, and vice versa; and
    /// each becomes pending, so one outcome scores both sides at the same
    /// `as_of` — the pairing [`super::shadow::ShadowLedger`] reads.
    ///
    /// A held item's candidates that [`Heuristic::models_hold`] read its
    /// modeled view and their own series signature (#10284,
    /// `tracker_hold.rs`); every other heuristic, `current` included, reads
    /// the described input under the item's signature, as before.
    pub fn estimate(
        &mut self,
        keys: Option<&[ItemKey]>,
        ctx: &EstimateContext<'_>,
        now: DateTime<Utc>,
    ) -> Vec<Emission> {
        let targets: Vec<ItemKey> = match keys {
            Some(keys) => keys.to_vec(),
            None => self.items.keys().cloned().collect(),
        };
        let mut out = Vec::new();
        for key in targets {
            let Some(item) = self.items.get(&key).cloned() else {
                continue;
            };
            for kind in [Kind::Start, Kind::Finish, Kind::Land] {
                let Some(input) = self.input_for(&key, &item, kind, ctx, now) else {
                    continue;
                };
                let configured = match kind {
                    Kind::Start => ctx.current_start,
                    Kind::Finish => ctx.current_finish,
                    Kind::Land => ctx.current_land,
                };
                let current_id = ctx.registry.current(kind, configured).id();
                let signature = Signature {
                    stage: item.stage.as_ref().map(|s| s.stage),
                    rework_rounds: item.rework_rounds,
                    reason: item.refused,
                };
                // A held item's view for a heuristic that models the hold,
                // with its own series signature (#10284, `tracker_hold.rs`).
                let modeled = self.modeled_input(&key, &item, kind, &input, ctx);
                // `current` first, then every shadow candidate: the primary
                // estimate is emitted before any candidate can be mistaken for
                // it, and its ordering in the output is what it always was.
                let ordered: Vec<&dyn Heuristic> = ctx
                    .registry
                    .for_kind(kind)
                    .filter(|h| h.id() == current_id)
                    .chain(ctx.registry.for_kind(kind).filter(|h| h.id() != current_id))
                    .collect();
                for heuristic in ordered {
                    let (input, signature) = match &modeled {
                        Some((view, held)) if heuristic.models_hold() => {
                            (view, held.unwrap_or(signature))
                        }
                        _ => (&input, signature),
                    };
                    let series = (kind, heuristic.id().to_string());
                    let state = item.emit.get(&series).cloned().unwrap_or_default();
                    let Some(trigger) = state.decide(signature, now, ctx.refresh_secs) else {
                        continue;
                    };
                    let explanation = heuristic.estimate(input, ctx.history);
                    if let Some(item) = self.items.get_mut(&key) {
                        item.answered
                            .insert(series.clone(), explanation.result.is_some());
                        item.emit.entry(series).or_default().record(signature, now);
                    }
                    self.pending.push(EstimateSummary::of(&explanation));
                    out.push(Emission {
                        trigger,
                        explanation,
                        primary: heuristic.id() == current_id,
                    });
                }
                if keys.is_none() {
                    self.tally_pass(&key, kind);
                }
            }
        }
        out
    }
}

#[path = "tracker_censor.rs"]
mod censor;

#[path = "tracker_answers.rs"]
mod answers;

pub use answers::PassAnswers;
pub use censor::{censor, Expired, CENSOR_SOURCE};

#[path = "tracker_ready.rs"]
mod ready;

#[path = "tracker_features.rs"]
mod features;

#[path = "tracker_item.rs"]
mod item_facts;

pub use item_facts::{DispatchMeta, IssueRow, RegistryMeta};

pub use features::{events_from_journal, ListedPr, NOT_LISTED_YET};

#[path = "tracker_hold.rs"]
mod hold;

pub use ready::{
    ReadyPlan, ReadyRow, READY_FIRST_SEEN, READY_PLAN_MAX_AGE_SECS, SLOT_TURNOVER_REPO,
};

/// Merge several effects.
#[must_use]
pub fn merged(all: Vec<Effects>) -> Effects {
    let mut out = Effects::default();
    for effects in all {
        out.merge(effects);
    }
    out
}
