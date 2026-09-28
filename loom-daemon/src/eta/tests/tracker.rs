//! The tracker: stage resolution, eager journal rows, outcome joins.

use super::{as_of, history_a, provenance};
use crate::eta::score::OutcomeKind;
use crate::eta::tracker::{EstimateContext, ItemKey, PrState, PrView, Tracker};
use crate::eta::{Kind, NoEstimateReason, Registry, Stage};
use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeMap;

const REPO: &str = "rjwalters/loom";

fn t(secs: i64) -> DateTime<Utc> {
    as_of() + Duration::seconds(secs)
}

struct Harness {
    tracker: Tracker,
    registry: Registry,
    history: crate::eta::StageSamples,
    repo_ids: BTreeMap<String, u64>,
}

impl Harness {
    fn new() -> Self {
        let mut repo_ids = BTreeMap::new();
        repo_ids.insert(REPO.to_string(), 1_073_994_527);
        Harness {
            tracker: Tracker::new(provenance()),
            registry: Registry::builtin(),
            history: history_a(),
            repo_ids,
        }
    }

    fn estimate(&mut self, at: DateTime<Utc>) -> Vec<crate::eta::tracker::Emission> {
        let ctx = EstimateContext {
            registry: &self.registry,
            current_finish: None,
            current_land: None,
            history: &self.history,
            refresh_secs: 300,
            host_id: Some("host-test"),
            repo_ids: &self.repo_ids,
        };
        self.tracker.estimate(None, &ctx, at)
    }
}

fn pr(number: u32, issue: u32, labels: &[&str], updated_secs: i64) -> PrView {
    PrView {
        number,
        issue,
        labels: labels.iter().map(|s| (*s).to_string()).collect(),
        created_at: Some(t(-7200)),
        updated_at: Some(t(updated_secs)),
    }
}

#[test]
fn in_sweep_lifecycle_journals_every_boundary_and_resolves_both_kinds() {
    let mut h = Harness::new();
    let dispatch = h.tracker.on_dispatch(REPO, 42, "sweep-issue-42-1", t(0));
    assert_eq!(dispatch.journal[0].event, "sweep.dispatch");
    assert_eq!(dispatch.journal[0].next_stage, Some(Stage::SweepCurator));

    let first = h.estimate(t(1));
    let kinds: Vec<Kind> = first.iter().map(|e| e.explanation.kind).collect();
    assert_eq!(kinds, vec![Kind::Finish, Kind::Land]);
    assert!(first.iter().all(|e| e.explanation.quantiles().is_some()));
    assert_eq!(first[0].explanation.subject.repo_id, Some(1_073_994_527));

    let curator = h.tracker.on_phase(REPO, 42, "curator", None, t(600));
    let row = curator.journal.last().unwrap();
    assert_eq!(row.stage, Some(Stage::SweepCurator));
    assert_eq!(row.duration_sec, Some(600), "journaled the moment it is seen");
    assert!(row.in_sweep, "sweep.outcome also carries it");
    assert_eq!(row.raw["phase"], "curator");

    h.tracker.on_phase(REPO, 42, "builder", Some(4242), t(3000));
    // Duplicate publication of the same completion (registry + child).
    let repeat = h.tracker.on_phase(REPO, 42, "builder", Some(4242), t(3005));
    assert_eq!(repeat.journal[0].event, "sweep.phase.repeat");
    assert!(repeat.dirty.is_empty());

    h.estimate(t(3001));
    h.tracker.on_phase(REPO, 42, "judge", Some(4242), t(3900));
    // Verdict pending: no estimate until the next event settles it.
    assert!(h.estimate(t(3901)).is_empty());
    let doctor = h.tracker.on_phase(REPO, 42, "doctor", Some(4242), t(5100));
    let verdict = doctor
        .journal
        .iter()
        .find(|r| r.event == "verdict")
        .unwrap();
    assert_eq!(verdict.verdict.as_deref(), Some("fail"));
    assert_eq!(verdict.attempt, Some(1));
    let doctor_row = doctor
        .journal
        .iter()
        .find(|r| r.event == "sweep.phase")
        .unwrap();
    assert_eq!(doctor_row.stage, Some(Stage::Doctor));
    assert_eq!(doctor_row.duration_sec, Some(1200));

    let after_rework = h.estimate(t(5101));
    let land = after_rework
        .iter()
        .find(|e| e.explanation.kind == Kind::Land)
        .unwrap();
    assert_eq!(
        land.explanation
            .current_stage
            .as_ref()
            .unwrap()
            .rework_rounds,
        1
    );

    h.tracker.on_phase(REPO, 42, "judge", Some(4242), t(6000));
    let merged = h.tracker.on_phase(REPO, 42, "merge", Some(4242), t(6300));
    assert!(!merged.outcomes.is_empty());
    assert!(merged
        .outcomes
        .iter()
        .all(|o| o.estimate.kind == Kind::Land));
    assert!(merged
        .outcomes
        .iter()
        .all(|o| o.score.outcome == OutcomeKind::Landed));
    let scored = merged
        .outcomes
        .iter()
        .find(|o| o.estimate.as_of == t(1))
        .unwrap();
    assert_eq!(scored.score.lead_sec, 6299);
    assert!(scored.score.pinball_loss_sec.is_some());
    // Per-stage actuals: every stage completed after the estimate.
    let stages: Vec<Stage> = scored.score.stages_actual.iter().map(|s| s.stage).collect();
    assert_eq!(
        stages,
        vec![
            Stage::SweepCurator,
            Stage::SweepBuilder,
            Stage::ReviewWait,
            Stage::Doctor,
            Stage::ReviewWait,
            Stage::MergeWait
        ]
    );
    assert_eq!(scored.score.rework_rounds_actual, 1);
    assert_eq!(scored.estimate.loom, provenance());

    let exited = h.tracker.on_terminal(REPO, 42, "exited", Some(0), t(6400));
    assert!(exited
        .outcomes
        .iter()
        .all(|o| o.estimate.kind == Kind::Finish));
    assert!(exited
        .outcomes
        .iter()
        .all(|o| o.score.outcome == OutcomeKind::Finished));
    assert!(exited
        .outcomes
        .iter()
        .all(|o| o.result.as_deref() == Some("exited")));
    assert!(h.tracker.pending().is_empty(), "every emitted estimate was scored");
    assert!(h.tracker.item_keys().is_empty());
}

#[test]
fn external_review_path_from_listings_to_merge() {
    let mut h = Harness::new();
    // First sight mid-review: entry is a lower bound, nothing completed.
    let first =
        h.tracker
            .on_listing(REPO, &[pr(501, 50, &["loom:review-requested"], -600)], t(0), 300);
    assert_eq!(first.journal[0].event, "label.first_seen");
    let emitted = h.estimate(t(0));
    assert_eq!(emitted.len(), 1, "land only: no sweep is running");
    let current = emitted[0].explanation.current_stage.clone().unwrap();
    assert_eq!(current.age_sec, 600);
    assert_eq!(current.age_source, crate::eta::AgeSource::UpdatedAtLowerBound);

    // Approved between two listings: a verdict row, but the stage it closes
    // was entered at an unobserved time, so it has no duration.
    let approved = h
        .tracker
        .on_listing(REPO, &[pr(501, 50, &["loom:pr"], 250)], t(300), 300);
    let row = &approved.journal[0];
    assert_eq!(row.event, "label.transition");
    assert_eq!(row.verdict.as_deref(), Some("pass"));
    assert_eq!(row.stage, Some(Stage::ReviewWait));
    assert_eq!(row.duration_sec, None, "never a lower bound as a sample");
    assert!(!row.in_sweep);
    assert_eq!(row.resolution_sec, Some(300));
    h.estimate(t(300));

    // Gone from every review listing: one PR read decides.
    let gone = h.tracker.on_listing(REPO, &[], t(600), 300);
    assert_eq!(gone.pr_checks, vec![(ItemKey::new(REPO, 50), 501)]);
    let merged = h
        .tracker
        .on_pr_resolved(&ItemKey::new(REPO, 50), PrState::Merged(t(500)), t(600));
    let merge_row = &merged.journal[0];
    assert_eq!(merge_row.stage, Some(Stage::MergeWait));
    assert_eq!(merge_row.duration_sec, Some(200), "observed entry: a real sample");
    assert_eq!(merged.outcomes.len(), 2);
    for outcome in &merged.outcomes {
        assert_eq!(outcome.score.outcome, OutcomeKind::Landed);
        assert_eq!(outcome.score.actual_at, t(500));
        assert_eq!(outcome.outcome_source, "pulls_read");
        assert_eq!(outcome.outcome_resolution_sec, Some(100));
    }
    assert!(h.tracker.pending().is_empty());
}

#[test]
fn closed_unmerged_and_pre_pr_crash_are_abandoned_not_scored() {
    let mut h = Harness::new();
    h.tracker
        .on_listing(REPO, &[pr(601, 60, &["loom:review-requested"], -60)], t(0), 300);
    h.estimate(t(0));
    h.tracker.on_listing(REPO, &[], t(300), 300);
    let closed = h
        .tracker
        .on_pr_resolved(&ItemKey::new(REPO, 60), PrState::Closed, t(300));
    assert_eq!(closed.outcomes.len(), 1);
    assert_eq!(closed.outcomes[0].score.outcome, OutcomeKind::Abandoned);
    assert_eq!(closed.outcomes[0].score.error_sec, None);

    h.tracker.on_dispatch(REPO, 61, "sweep-issue-61-1", t(0));
    h.estimate(t(1));
    let crashed = h.tracker.on_terminal(REPO, 61, "crashed", None, t(900));
    let finish: Vec<_> = crashed
        .outcomes
        .iter()
        .filter(|o| o.estimate.kind == Kind::Finish)
        .collect();
    let land: Vec<_> = crashed
        .outcomes
        .iter()
        .filter(|o| o.estimate.kind == Kind::Land)
        .collect();
    assert_eq!(finish.len(), 1);
    assert_eq!(finish[0].score.outcome, OutcomeKind::Finished);
    assert_eq!(finish[0].result.as_deref(), Some("crashed"));
    assert!(finish[0].score.error_sec.is_some(), "a crash still finished the sweep");
    assert_eq!(land.len(), 1);
    assert_eq!(land[0].score.outcome, OutcomeKind::Abandoned, "no fabricated land");
    assert_eq!(land[0].score.error_sec, None);
}

#[test]
fn a_hold_mid_estimate_is_one_refusal() {
    let mut h = Harness::new();
    h.tracker
        .on_listing(REPO, &[pr(701, 70, &["loom:review-requested"], -60)], t(0), 300);
    assert_eq!(h.estimate(t(0)).len(), 1);
    h.tracker.on_listing(
        REPO,
        &[pr(701, 70, &["loom:review-requested", "loom:operator"], 60)],
        t(300),
        300,
    );
    let refused = h.estimate(t(300));
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].explanation.no_estimate_reason, Some(NoEstimateReason::Blocked));
    assert_eq!(refused[0].trigger, crate::eta::emit::Trigger::Transition);
    // Not refreshed while it stays held.
    h.tracker.on_listing(
        REPO,
        &[pr(701, 70, &["loom:review-requested", "loom:operator"], 60)],
        t(900),
        300,
    );
    assert!(h.estimate(t(900)).is_empty());
}

#[test]
fn unchanged_items_refresh_on_the_cadence() {
    let mut h = Harness::new();
    h.tracker
        .on_listing(REPO, &[pr(801, 80, &["loom:pr"], -60)], t(0), 300);
    assert_eq!(h.estimate(t(0)).len(), 1);
    assert!(h.estimate(t(120)).is_empty());
    let refreshed = h.estimate(t(300));
    assert_eq!(refreshed.len(), 1);
    assert_eq!(refreshed[0].trigger, crate::eta::emit::Trigger::Refresh);
    assert_eq!(h.tracker.pending().len(), 2, "every emitted estimate waits for its outcome");
}

#[test]
fn pending_survives_a_restart() {
    let mut h = Harness::new();
    h.tracker
        .on_listing(REPO, &[pr(901, 90, &["loom:pr"], -60)], t(0), 300);
    h.estimate(t(0));
    let persisted: Vec<String> = h
        .tracker
        .pending()
        .iter()
        .map(|p| serde_json::to_string(p).unwrap())
        .collect();

    // A new process: items are gone, pending comes back from disk.
    let mut restarted = Tracker::new(provenance());
    restarted.restore_pending(
        persisted
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect(),
    );
    restarted.on_listing(REPO, &[pr(901, 90, &["loom:pr"], 100)], t(400), 300);
    restarted.on_listing(REPO, &[], t(700), 300);
    let merged = restarted.on_pr_resolved(&ItemKey::new(REPO, 90), PrState::Merged(t(650)), t(700));
    assert_eq!(merged.outcomes.len(), 1, "the join survived");
    assert_eq!(merged.outcomes[0].estimate.as_of, t(0));
}

#[test]
fn expire_drops_old_pending() {
    let mut h = Harness::new();
    h.tracker
        .on_listing(REPO, &[pr(111, 11, &["loom:pr"], -60)], t(0), 300);
    h.estimate(t(0));
    assert_eq!(h.tracker.expire(t(3600)), 0);
    assert_eq!(h.tracker.expire(t(0) + Duration::days(31)), 1);
    assert!(h.tracker.pending().is_empty());
}

#[test]
fn an_item_first_seen_in_doctor_has_taken_a_rejection() {
    let mut h = Harness::new();
    h.tracker.on_listing(
        REPO,
        &[pr(
            121,
            12,
            &["loom:changes-requested", "loom:treating"],
            -60,
        )],
        t(0),
        300,
    );
    let emitted = h.estimate(t(0));
    let current = emitted[0].explanation.current_stage.clone().unwrap();
    assert_eq!(current.stage, Stage::Doctor);
    assert_eq!(current.rework_rounds, 1);
}
