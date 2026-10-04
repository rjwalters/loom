//! The tracker's operator-hold overlay (#10218): entry and exit pinned on
//! operator-label add and remove, with the pooled `merge_wait` untouched.

use super::{as_of, history_a, provenance};
use crate::eta::journal::JournalEntry;
use crate::eta::tracker::{Emission, EstimateContext, ItemKey, PrState, PrView, Tracker};
use crate::eta::{
    AgeSource, CurrentStage, CurrentState, NoEstimateReason, Registry, Stage, StageSamples,
};
use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeMap;

const REPO: &str = "rjwalters/loom";
const RR: &str = "loom:review-requested";
const PR: &str = "loom:pr";
const OP: &str = "loom:operator";
const CR: &str = "loom:changes-requested";

fn t(secs: i64) -> DateTime<Utc> {
    as_of() + Duration::seconds(secs)
}

fn pr(labels: &[&str], updated_secs: i64) -> PrView {
    PrView {
        number: 501,
        issue: 50,
        labels: labels.iter().map(|s| (*s).to_string()).collect(),
        created_at: Some(t(-7200)),
        updated_at: Some(t(updated_secs)),
    }
}

struct Harness {
    tracker: Tracker,
    registry: Registry,
    history: StageSamples,
    repo_ids: BTreeMap<String, u64>,
    /// Every journal row written so far, in order.
    rows: Vec<JournalEntry>,
}

impl Harness {
    fn new() -> Self {
        Harness {
            tracker: Tracker::new(provenance()),
            registry: Registry::builtin(),
            history: history_a(),
            repo_ids: BTreeMap::new(),
            rows: Vec::new(),
        }
    }

    fn key() -> ItemKey {
        ItemKey::new(REPO, 50)
    }

    fn ctx(&self) -> EstimateContext<'_> {
        EstimateContext {
            registry: &self.registry,
            current_start: None,
            current_finish: None,
            current_land: None,
            history: &self.history,
            refresh_secs: 300,
            host_id: Some("host-test"),
            repo_ids: &self.repo_ids,
        }
    }

    /// One listing pass; returns the rows it wrote.
    fn list(&mut self, labels: &[&str], updated_secs: i64, at: i64) -> Vec<JournalEntry> {
        let effects = self
            .tracker
            .on_listing(REPO, &[pr(labels, updated_secs)], t(at), 300);
        self.rows.extend(effects.journal.clone());
        effects.journal
    }

    /// The PR leaves the listings and is read as `state`.
    fn resolve(&mut self, state: PrState, at: i64) -> Vec<JournalEntry> {
        self.tracker.on_listing(REPO, &[], t(at), 300);
        let effects = self.tracker.on_pr_resolved(&Self::key(), state, t(at));
        self.rows.extend(effects.journal.clone());
        effects.journal
    }

    fn current(&self, at: i64) -> CurrentStage {
        let input = self
            .tracker
            .land_input(&Self::key(), &self.ctx(), t(at))
            .expect("a land input");
        match input.current {
            CurrentState::At(current) => current,
            CurrentState::Refused(reason) => panic!("refused {reason}"),
        }
    }

    /// The primary emissions at `at`.
    fn estimate(&mut self, at: i64) -> Vec<Emission> {
        let ctx = EstimateContext {
            registry: &self.registry,
            current_start: None,
            current_finish: None,
            current_land: None,
            history: &self.history,
            refresh_secs: 300,
            host_id: Some("host-test"),
            repo_ids: &self.repo_ids,
        };
        self.tracker
            .estimate(None, &ctx, t(at))
            .into_iter()
            .filter(|e| e.primary)
            .collect()
    }

    /// The samples every journal row so far contributes.
    fn samples(&self, stage: Stage) -> Vec<i64> {
        let mut history = StageSamples::default();
        history.push_journal(&self.rows, "host-test");
        history
            .stages
            .iter()
            .filter(|s| s.stage == stage)
            .map(|s| s.duration_sec)
            .collect()
    }
}

/// First seen in review at 0, approved at 300 (pooled `merge_wait` entered
/// at 300, exactly).
fn approved() -> Harness {
    let mut h = Harness::new();
    h.list(&[RR], -600, 0);
    h.list(&[PR], 250, 300);
    h.estimate(300);
    h
}

#[test]
fn approve_hold_release_merge() {
    let mut h = approved();
    let before = h.current(300);
    assert_eq!((before.stage, before.entered_at), (Stage::MergeWait, Some(t(300))));

    // The operator label is added: a boundary-only row, the overlay opens.
    let held = h.list(&[PR, OP], 550, 600);
    assert_eq!(held.len(), 1);
    let boundary = &held[0];
    assert_eq!(boundary.event, "label.transition");
    assert_eq!(boundary.stage, Some(Stage::MergeWait));
    assert_eq!(boundary.next_stage, Some(Stage::MergeHold));
    assert_eq!(boundary.left_at, Some(t(600)));
    assert_eq!((boundary.duration_sec, boundary.censored_sec), (None, None));
    let current = h.current(700);
    assert_eq!(current.stage, Stage::MergeHold);
    assert_eq!(current.entered_at, Some(t(600)));
    assert_eq!(current.age_sec, 100);
    assert_eq!(current.age_source, AgeSource::TrackerObserved);
    // Shipped: one `blocked` refusal on the transition, never refreshed.
    let refused = h.estimate(600);
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].explanation.no_estimate_reason, Some(NoEstimateReason::Blocked));
    assert_eq!(refused[0].trigger, crate::eta::emit::Trigger::Transition);
    assert!(h.list(&[PR, OP], 550, 900).is_empty());
    assert!(h.estimate(900).is_empty());

    // The label is removed: the hold closes with its exact duration.
    let released = h.list(&[PR], 1450, 1500);
    assert_eq!(released.len(), 1);
    let hold = &released[0];
    assert_eq!(hold.stage, Some(Stage::MergeHold));
    assert_eq!(hold.entered_at, Some(t(600)));
    assert_eq!(hold.left_at, Some(t(1500)));
    assert_eq!(hold.duration_sec, Some(900));
    assert_eq!(hold.next_stage, Some(Stage::MergeWait));
    let current = h.current(1600);
    assert_eq!(current.stage, Stage::MergeWait);
    assert_eq!(current.entered_at, Some(t(300)), "pooled: the approval");
    assert_eq!(current.age_sec, 1300);
    assert_eq!(current.episode_entered_at, Some(t(1500)), "split: the release");
    let after = h.estimate(1500);
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].trigger, crate::eta::emit::Trigger::Transition);
    assert_ne!(after[0].explanation.no_estimate_reason, Some(NoEstimateReason::Blocked));
    assert!(after[0].explanation.current_stage.is_some());

    // Merged: the pooled row runs from the approval, hold included.
    let merged = h.tracker.on_listing(REPO, &[], t(1800), 300);
    assert_eq!(merged.pr_checks, vec![(Harness::key(), 501)]);
    let effects = h
        .tracker
        .on_pr_resolved(&Harness::key(), PrState::Merged(t(1700)), t(1800));
    h.rows.extend(effects.journal.clone());
    assert_eq!(effects.journal.len(), 1);
    assert_eq!(effects.journal[0].stage, Some(Stage::MergeWait));
    assert_eq!(effects.journal[0].duration_sec, Some(1400));
    assert_eq!(h.samples(Stage::MergeHold), vec![900], "exactly one hold sample");
    assert_eq!(h.samples(Stage::MergeWait), vec![1400], "pooled merge_wait");
    for outcome in &effects.outcomes {
        assert!(outcome
            .score
            .stages_actual
            .iter()
            .all(|s| s.stage != Stage::MergeHold));
    }
}

#[test]
fn merged_while_held_writes_the_pooled_row_and_a_completed_hold_row() {
    let mut h = approved();
    h.list(&[PR, OP], 550, 600);
    let rows = h.resolve(PrState::Merged(t(850)), 900);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].stage, Some(Stage::MergeHold));
    assert_eq!(rows[0].duration_sec, Some(250));
    assert_eq!(rows[0].left_at, Some(t(850)));
    assert_eq!(rows[0].next_stage, None);
    assert_eq!(rows[0].raw["state"], "merged");
    assert_eq!(rows[1].stage, Some(Stage::MergeWait));
    assert_eq!(rows[1].duration_sec, Some(550));
    assert_eq!(h.samples(Stage::MergeHold), vec![250]);
    assert_eq!(h.samples(Stage::MergeWait), vec![550]);
}

#[test]
fn closed_while_held_censors_the_hold() {
    let mut h = approved();
    h.list(&[PR, OP], 550, 600);
    let rows = h.resolve(PrState::Closed, 1000);
    assert_eq!(rows[0].stage, Some(Stage::MergeHold));
    assert_eq!((rows[0].duration_sec, rows[0].censored_sec), (None, Some(400)));
    assert!(h.samples(Stage::MergeHold).is_empty());
}

#[test]
fn approve_hold_then_changes_requested() {
    let mut h = approved();
    h.list(&[PR, OP], 550, 600);
    let rows = h.list(&[CR], 1150, 1200);
    assert_eq!(rows.len(), 2);
    // The hold closes first, to `doctor`…
    assert_eq!(rows[0].stage, Some(Stage::MergeHold));
    assert_eq!(rows[0].duration_sec, Some(600));
    assert_eq!(rows[0].next_stage, Some(Stage::Doctor));
    // …then the pooled `merge_wait` closes exactly as it always has.
    assert_eq!(rows[1].stage, Some(Stage::MergeWait));
    assert_eq!(rows[1].duration_sec, Some(900));
    assert_eq!(rows[1].next_stage, Some(Stage::Doctor));
    let current = h.current(1300);
    assert_eq!(current.stage, Stage::Doctor);
    assert_eq!(current.rework_rounds, 1);
    assert_eq!(current.episode_entered_at, None);
    assert_eq!(h.samples(Stage::MergeHold), vec![600]);
    assert_eq!(h.samples(Stage::MergeWait), vec![900]);
}

#[test]
fn first_sight_while_held() {
    let mut h = Harness::new();
    let rows = h.list(&[PR, OP], -600, 0);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event, "label.first_seen");
    assert_eq!(rows[0].next_stage, Some(Stage::MergeHold));
    let current = h.current(0);
    assert_eq!(current.stage, Stage::MergeHold);
    assert_eq!(current.entered_at, Some(t(-600)));
    assert_eq!(current.age_source, AgeSource::UpdatedAtLowerBound);
    let first = h.estimate(0);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].explanation.no_estimate_reason, Some(NoEstimateReason::Blocked));

    // Released: an inexact entry never yields a duration.
    let rows = h.list(&[PR], 550, 600);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].stage, Some(Stage::MergeHold));
    assert_eq!((rows[0].entered_at, rows[0].duration_sec), (None, None));
    let current = h.current(700);
    assert_eq!(current.stage, Stage::MergeWait);
    assert_eq!(current.entered_at, Some(t(-600)));
    assert_eq!(current.episode_entered_at, Some(t(600)));

    h.resolve(PrState::Merged(t(900)), 1000);
    assert!(h.samples(Stage::MergeHold).is_empty());
    assert!(h.samples(Stage::MergeWait).is_empty(), "the pooled entry was a lower bound");
}

#[test]
fn review_wait_approved_and_held_in_one_interval_closes_review_at_the_approval() {
    // Review entered exactly (the Builder phase), then the sweep exits.
    let mut h = Harness::new();
    h.tracker
        .on_dispatch(REPO, 50, "sweep-issue-50-1", t(-3000));
    h.tracker.on_phase(REPO, 50, "curator", None, t(-2000));
    h.tracker.on_phase(REPO, 50, "builder", Some(501), t(-600));
    h.tracker.on_terminal(REPO, 50, "exited", Some(0), t(-300));

    let rows = h.list(&[PR, OP], 250, 300);
    assert_eq!(rows.len(), 1, "no separate merge_wait boundary row");
    let review = &rows[0];
    assert_eq!(review.stage, Some(Stage::ReviewWait));
    assert_eq!(review.duration_sec, Some(900), "closed at the approval, not the release");
    assert_eq!(review.verdict.as_deref(), Some("pass"));
    assert_eq!(review.attempt, Some(1));
    assert_eq!(review.next_stage, Some(Stage::MergeHold));
    assert_eq!(h.current(400).entered_at, Some(t(300)));

    let rows = h.list(&[PR], 850, 900);
    assert_eq!(rows[0].duration_sec, Some(600), "the hold, from the approval");
    let current = h.current(1000);
    assert_eq!(current.entered_at, Some(t(300)), "pooled merge_wait from the approval");
    assert_eq!(current.episode_entered_at, Some(t(900)));
}

#[test]
fn a_non_operator_hold_on_an_approved_pr_is_still_only_a_refusal() {
    let mut h = approved();
    let rows = h.list(&[PR, "loom:blocked"], 550, 600);
    assert!(rows.is_empty(), "no overlay, no row");
    let input = h
        .tracker
        .land_input(&Harness::key(), &h.ctx(), t(700))
        .unwrap();
    assert_eq!(input.current, CurrentState::Refused(NoEstimateReason::Blocked));
}
