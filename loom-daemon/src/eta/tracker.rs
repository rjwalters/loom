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
//!
//! # Outcomes
//!
//! `finish` resolves on the sweep's terminal bus event. `land` resolves on
//! an in-sweep `merge`, or on the PR's merge time when it leaves the review
//! listings; a PR closed unmerged, or a sweep that ended with no PR, is
//! `abandoned`. Every pending estimate of the series is scored.
//!
//! # Which items get which estimates
//!
//! - `finish`: every item with a running issue sweep, from dispatch on.
//! - `land`: every item with a running sweep **from `sweep.curator` on** (a
//!   deliberate widening of the curated "land only from `sweep.builder`":
//!   the curator phase has local history like every other in-sweep phase,
//!   and no refusal reason fits it), and every open PR under a review label
//!   that closes an issue.
//! - Nothing for an issue with no running sweep and no open PR: that is
//!   intake, approval or the ready queue, which a later phase covers.
//! - An item in `doctor` always has at least one rework round: `doctor` is
//!   entered only through a rejection, so an item first seen there (after a
//!   restart) is counted as having taken one.

use super::emit::{EmitState, Signature, Trigger};
use super::explanation::{Explanation, FeatureOmitted, Features};
use super::journal::JournalEntry;
use super::labels::stage_from_pr_labels;
use super::score::{score, EstimateSummary, OutcomeKind, Score, StageObservation};
use super::{
    AgeSource, CurrentStage, CurrentState, EstimateInput, Kind, NoEstimateReason, Provenance,
    Registry, Stage, StageSamples, Subject,
};
use chrono::{DateTime, Datelike, Duration, Timelike, Utc};
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
    /// The last completed phase and when, to drop duplicate publications.
    last_phase: Option<(String, DateTime<Utc>)>,
    emit: BTreeMap<(Kind, String), EmitState>,
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
    /// PRs that left the review listings: read their state.
    pub pr_checks: Vec<(ItemKey, u32)>,
}

impl Effects {
    fn merge(&mut self, other: Effects) {
        self.journal.extend(other.journal);
        self.outcomes.extend(other.outcomes);
        self.dirty.extend(other.dirty);
        self.pr_checks.extend(other.pr_checks);
    }
}

/// An estimate that was, or would have been, emitted.
#[derive(Debug, Clone, PartialEq)]
pub struct Emission {
    /// Why now.
    pub trigger: Trigger,
    /// The estimate.
    pub explanation: Explanation,
}

/// Context the caller supplies to every estimate.
#[derive(Clone)]
pub struct EstimateContext<'a> {
    /// The heuristics.
    pub registry: &'a Registry,
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

    /// Move `key` to `next` at `at`, closing the current stage. Returns the
    /// journal row for the boundary.
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
            if old.exact {
                row.entered_at = Some(old.entered_at);
                row.duration_sec = Some((at - old.entered_at).num_seconds().max(0));
                item.observed.push(StageObservation {
                    stage: old.stage,
                    entered_at: old.entered_at,
                    left_at: at,
                    source: observed_source.to_string(),
                });
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
                let row = self.settle_verdict(&key, stage, verdict, since, true);
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
    ) -> JournalEntry {
        let item = self
            .items
            .get_mut(key)
            .unwrap_or_else(|| unreachable!("caller created it"));
        item.verdict_pending_since = None;
        let attempt = item.rework_rounds + 1;
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
        let pre_pr = matches!(
            item.stage.as_ref().map(|s| s.stage),
            Some(Stage::SweepCurator | Stage::SweepBuilder)
        ) || item.pr_number.is_none();
        if item.landed || pre_pr {
            if !item.landed {
                // Ended before any PR: nothing is left to land.
                effects.outcomes.extend(self.resolve(
                    &key,
                    Kind::Land,
                    OutcomeKind::Abandoned,
                    at,
                    "sweep_terminal",
                    Some(0),
                    Some(class.to_string()),
                ));
            }
            self.items.remove(&key);
        } else {
            effects.dirty.push(key);
        }
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
                    let mut row = self.settle_verdict(&key, stage, verdict, since, true);
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
        // Tracked PRs of this repo that left every review listing.
        for (key, item) in &mut self.items {
            if key.repo == repo_key && item.in_review_listing && !seen.contains(key) {
                item.in_review_listing = false;
                if let Some(pr) = item.pr_number {
                    effects.pr_checks.push((key.clone(), pr));
                }
            }
        }
        effects
    }

    /// The state of a PR that left the review listings.
    pub fn on_pr_resolved(&mut self, key: &ItemKey, state: PrState, now: DateTime<Utc>) -> Effects {
        let mut effects = Effects::default();
        let Some(item) = self.items.get(key) else {
            return effects;
        };
        if item.landed {
            return effects;
        }
        let pr = item.pr_number;
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
                let mut row = self.transition(
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
                effects.outcomes.extend(self.resolve(
                    key,
                    Kind::Land,
                    OutcomeKind::Abandoned,
                    now,
                    "pulls_read",
                    None,
                    None,
                ));
                self.finish_item(key);
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
        let (matching, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|p| {
                p.kind == kind
                    && p.issue == key.issue
                    && p.repo.eq_ignore_ascii_case(&key.repo)
                    && p.as_of <= actual_at
            });
        self.pending = rest;
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

    /// Drop pending estimates older than [`PENDING_MAX_AGE_DAYS`], and the
    /// oldest past [`MAX_PENDING`]. Returns how many were dropped.
    pub fn expire(&mut self, now: DateTime<Utc>) -> usize {
        let before = self.pending.len();
        let cutoff = now - Duration::days(PENDING_MAX_AGE_DAYS);
        self.pending.retain(|p| p.as_of >= cutoff);
        if self.pending.len() > MAX_PENDING {
            let excess = self.pending.len() - MAX_PENDING;
            self.pending.drain(..excess);
        }
        before - self.pending.len()
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
        if kind == Kind::Land && item.landed {
            return None;
        }
        let current = if let Some(reason) = item.refused {
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
            })
        };
        let mut subject =
            Subject::new(&item.repo, ctx.repo_ids.get(&key.repo).copied(), item.issue);
        subject.pr_number = item.pr_number;
        subject.sweep_id = item.sweep_id.clone().filter(|_| item.sweep_running);
        let features = Features {
            labels: (!item.labels.is_empty()).then(|| item.labels.clone()),
            doctor_cycles_so_far: Some(item.rework_rounds),
            sweep_internal: Some(item.sweep_running),
            pr_created_at: item.pr_created_at,
            hour_utc: Some(now.hour()),
            weekday_utc: Some(now.weekday().num_days_from_monday()),
            host_id: ctx.host_id.map(str::to_string),
            ..Features::default()
        };
        let mut omitted = Vec::new();
        if item.labels.is_empty() {
            omitted.push(FeatureOmitted {
                name: "labels".to_string(),
                reason: "not_listed_yet".to_string(),
            });
        }
        Some(EstimateInput {
            subject,
            as_of: now,
            current,
            features,
            features_omitted: omitted,
            provenance: self.loom.clone(),
        })
    }

    /// Estimate `keys` (every item when `None`) at `now`, returning the
    /// estimates the emit policy lets out. Each emitted estimate becomes
    /// pending until its outcome.
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
            for kind in [Kind::Finish, Kind::Land] {
                let Some(input) = self.input_for(&key, &item, kind, ctx, now) else {
                    continue;
                };
                let configured = match kind {
                    Kind::Finish => ctx.current_finish,
                    Kind::Land => ctx.current_land,
                };
                let heuristic = ctx.registry.current(kind, configured);
                let signature = Signature {
                    stage: item.stage.as_ref().map(|s| s.stage),
                    rework_rounds: item.rework_rounds,
                    reason: item.refused,
                };
                let series = (kind, heuristic.id().to_string());
                let state = item.emit.get(&series).cloned().unwrap_or_default();
                let Some(trigger) = state.decide(signature, now, ctx.refresh_secs) else {
                    continue;
                };
                let explanation = heuristic.estimate(&input, ctx.history);
                if let Some(item) = self.items.get_mut(&key) {
                    item.emit.entry(series).or_default().record(signature, now);
                }
                self.pending.push(EstimateSummary::of(&explanation));
                out.push(Emission {
                    trigger,
                    explanation,
                });
            }
        }
        out
    }
}

/// Merge several effects.
#[must_use]
pub fn merged(all: Vec<Effects>) -> Effects {
    let mut out = Effects::default();
    for effects in all {
        out.merge(effects);
    }
    out
}
