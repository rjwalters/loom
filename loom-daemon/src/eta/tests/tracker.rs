//! The tracker: stage resolution, eager journal rows, outcome joins.

use super::{as_of, history_a, provenance};
use crate::eta::score::OutcomeKind;
use crate::eta::tracker::{EstimateContext, IssueState, ItemKey, PrState, PrView, Tracker};
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

    /// Every emission, shadow candidates included (#9328).
    fn estimate_all(&mut self, at: DateTime<Utc>) -> Vec<crate::eta::tracker::Emission> {
        let ctx = EstimateContext {
            registry: &self.registry,
            current_start: None,
            current_finish: None,
            current_land: None,
            history: &self.history,
            refresh_secs: 300,
            host_id: Some("host-test"),
            repo_ids: &self.repo_ids,
            stalls: &crate::eta::stall::StallSnapshot::default(),
        };
        self.tracker.estimate(None, &ctx, at)
    }

    /// The **primary** emissions only — `current`'s estimate per kind, which
    /// is exactly what every consumer predating shadow mode saw. The lifecycle
    /// tests below assert on this so they keep pinning the behaviour they were
    /// written for; `shadow_mode_*` asserts on [`Self::estimate_all`].
    fn estimate(&mut self, at: DateTime<Utc>) -> Vec<crate::eta::tracker::Emission> {
        self.estimate_all(at)
            .into_iter()
            .filter(|e| e.primary)
            .collect()
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
    assert_eq!(
        merged.outcomes.len(),
        22,
        "two estimates x land-v1 + the land-v2, even-lark, brisk-petrel, held-heron, keen-wren, loop-kite, twin-otter-b, land-v4, little-v0 and tandem-wren shadows"
    );
    for outcome in &merged.outcomes {
        assert_eq!(outcome.score.outcome, OutcomeKind::Landed);
        assert_eq!(outcome.score.actual_at, t(500));
        assert_eq!(outcome.outcome_source, "pulls_read");
        assert_eq!(outcome.outcome_resolution_sec, Some(100));
    }
    assert!(h.tracker.pending().is_empty());
}

#[test]
fn only_the_issue_closing_not_planned_is_abandoned() {
    // Operator decision 4: abandoned means the issue closed as not planned.
    let mut h = Harness::new();
    h.tracker
        .on_listing(REPO, &[pr(601, 60, &["loom:review-requested"], -60)], t(0), 300);
    h.estimate(t(0));
    h.tracker.on_listing(REPO, &[], t(300), 300);
    let closed = h
        .tracker
        .on_pr_resolved(&ItemKey::new(REPO, 60), PrState::Closed, t(300));
    assert!(
        closed.outcomes.is_empty(),
        "a PR closed unmerged is not an outcome: the issue decides"
    );
    assert_eq!(closed.issue_checks, vec![ItemKey::new(REPO, 60)]);
    let closed_row = &closed.journal[0];
    assert_eq!(closed_row.event, "pr.resolved");
    assert_eq!(closed_row.stage, Some(Stage::ReviewWait));
    assert_eq!(
        closed_row.duration_sec, None,
        "the stage was cut short, not completed: never a history sample"
    );
    assert!(closed_row.history_sample("host-test").is_none());

    let not_planned = h.tracker.on_issue_resolved(
        &ItemKey::new(REPO, 60),
        IssueState::ClosedNotPlanned(t(400)),
        t(600),
    );
    // Eleven: `land-v1` (primary) and the `land-v2`,
    // `land-2026-10-06-even-lark`, `land-2026-10-06-brisk-petrel` (#10528),
    // `land-2026-10-06-held-heron`, `land-2026-10-06-keen-wren` (#10508),
    // `land-2026-10-06-loop-kite` (#10521),
    // `land-2026-10-04-twin-otter-b`, `land-v4`, `little-v0` and
    // `land-2026-10-06-tandem-wren` shadows, scored against the same outcome
    // at the same `as_of` — the live pairs (#9328, #10489, #10524, #10523,
    // #10508, #10244, #10210, #10208, #10510, #10528, #10521; `land-v3` and
    // `-amber-heron` retired, #10484; `-fresh-tide`, #10549;
    // `land-2026-10-04-twin-otter`, #10528; the IPCW wrappers
    // `-quick-tern`, `-swift-tern` and `-bold-lark`, #10949).
    assert_eq!(not_planned.outcomes.len(), 11);
    assert_eq!(not_planned.outcomes[0].score.outcome, OutcomeKind::Abandoned);
    assert_eq!(not_planned.outcomes[0].score.error_sec, None);
    assert_eq!(not_planned.outcomes[0].outcome_source, "issues_read");
    assert!(h.tracker.pending().is_empty());
}

#[test]
fn a_pre_pr_crash_with_the_issue_open_stays_pending() {
    let mut h = Harness::new();
    h.tracker.on_dispatch(REPO, 61, "sweep-issue-61-1", t(0));
    h.estimate(t(1));
    let crashed = h.tracker.on_terminal(REPO, 61, "crashed", None, t(900));
    let finish: Vec<_> = crashed
        .outcomes
        .iter()
        .filter(|o| o.estimate.kind == Kind::Finish)
        .collect();
    assert_eq!(finish.len(), 1);
    assert_eq!(finish[0].score.outcome, OutcomeKind::Finished);
    assert_eq!(finish[0].result.as_deref(), Some("crashed"));
    assert!(finish[0].score.error_sec.is_some(), "a crash still finished the sweep");
    assert!(
        crashed
            .outcomes
            .iter()
            .all(|o| o.estimate.kind == Kind::Finish),
        "a sweep ending before any PR says nothing about landing"
    );

    // The check is queued and re-offered until the issue answers.
    let pass = h.tracker.on_listing(REPO, &[], t(1200), 300);
    assert_eq!(pass.issue_checks, vec![ItemKey::new(REPO, 61)]);
    assert!(h.estimate(t(1200)).is_empty(), "no land estimate while unresolved");

    let open = h
        .tracker
        .on_issue_resolved(&ItemKey::new(REPO, 61), IssueState::Open, t(1200));
    assert!(open.outcomes.is_empty(), "still open: neither landed nor abandoned");
    assert_eq!(h.tracker.pending().len(), 11, "every land estimate waits for the landing");
    assert!(h.tracker.pending().iter().all(|p| p.kind == Kind::Land));
    assert!(h.tracker.item_keys().is_empty(), "nothing left to observe");

    // A later sweep lands it: `resolve` joins on repo, issue and kind.
    h.tracker.on_dispatch(REPO, 61, "sweep-issue-61-2", t(2000));
    h.tracker.on_phase(REPO, 61, "curator", None, t(2100));
    h.tracker.on_phase(REPO, 61, "builder", Some(611), t(2200));
    h.tracker.on_phase(REPO, 61, "judge", Some(611), t(2300));
    let merged = h.tracker.on_phase(REPO, 61, "merge", Some(611), t(2400));
    let landed: Vec<_> = merged
        .outcomes
        .iter()
        .filter(|o| o.estimate.as_of == t(1))
        .collect();
    assert_eq!(landed.len(), 11, "the original estimate, every land heuristic, scored");
    assert!(landed
        .iter()
        .all(|o| o.score.outcome == OutcomeKind::Landed));
}

#[test]
fn a_closed_pr_replaced_by_one_that_merges_lands() {
    let mut h = Harness::new();
    h.tracker
        .on_listing(REPO, &[pr(701, 71, &["loom:review-requested"], -60)], t(0), 300);
    h.estimate(t(0));
    assert_eq!(h.tracker.pending().len(), 11, "land-v1 + the ten land shadows");
    h.tracker.on_listing(REPO, &[], t(300), 300);
    h.tracker
        .on_pr_resolved(&ItemKey::new(REPO, 71), PrState::Closed, t(300));
    h.tracker
        .on_issue_resolved(&ItemKey::new(REPO, 71), IssueState::Open, t(300));
    assert_eq!(h.tracker.pending().len(), 11, "not abandoned: a replacement may land");

    // The replacement PR for the same issue.
    h.tracker
        .on_listing(REPO, &[pr(702, 71, &["loom:pr"], 400)], t(600), 300);
    h.tracker.on_listing(REPO, &[], t(900), 300);
    let merged = h
        .tracker
        .on_pr_resolved(&ItemKey::new(REPO, 71), PrState::Merged(t(800)), t(900));
    let first: Vec<_> = merged
        .outcomes
        .iter()
        .filter(|o| o.estimate.as_of == t(0))
        .collect();
    assert_eq!(first.len(), 11, "the first PR's estimates scored against the landing");
    assert!(first.iter().all(|o| o.score.outcome == OutcomeKind::Landed));
}

#[test]
fn an_issue_closed_as_completed_without_a_pr_lands() {
    let mut h = Harness::new();
    h.tracker.on_dispatch(REPO, 72, "sweep-issue-72-1", t(0));
    h.estimate(t(1));
    h.tracker.on_terminal(REPO, 72, "exited", Some(0), t(600));
    let pass = h.tracker.on_listing(REPO, &[], t(900), 300);
    assert_eq!(pass.issue_checks, vec![ItemKey::new(REPO, 72)]);
    let completed = h.tracker.on_issue_resolved(
        &ItemKey::new(REPO, 72),
        IssueState::ClosedCompleted(t(700)),
        t(900),
    );
    assert_eq!(completed.outcomes.len(), 11, "land-v1 + the ten land shadows");
    assert_eq!(completed.outcomes[0].score.outcome, OutcomeKind::Landed);
    assert_eq!(completed.outcomes[0].score.actual_at, t(700));
    assert_eq!(completed.outcomes[0].outcome_resolution_sec, Some(200));
    assert!(completed.outcomes[0].score.error_sec.is_some(), "a landing is scored");
}

#[test]
fn checks_over_the_budget_or_failing_are_retried_and_never_estimated_meanwhile() {
    // A merge train: more simultaneous exits than one pass's read budget,
    // plus a read that fails. Nothing may be dropped, and no item may keep
    // receiving `land` estimates while its check is outstanding.
    const BUDGET: usize = 8;
    let mut h = Harness::new();
    let listing: Vec<PrView> = (0..10)
        .map(|i| pr(1000 + i, 100 + i, &["loom:pr"], -60))
        .collect();
    h.tracker.on_listing(REPO, &listing, t(0), 300);
    assert_eq!(h.estimate(t(0)).len(), 10);

    // All ten leave review in the same pass.
    let gone = h.tracker.on_listing(REPO, &[], t(300), 300);
    assert_eq!(gone.pr_checks.len(), 10, "every leaver is offered");
    assert!(
        h.estimate(t(300)).is_empty(),
        "no estimate for an item whose check is outstanding"
    );

    // The caller's budget covers eight; the eighth read fails.
    for (key, _) in gone.pr_checks.iter().take(BUDGET - 1) {
        h.tracker
            .on_pr_resolved(key, PrState::Merged(t(250)), t(300));
    }
    assert!(h.estimate(t(310)).is_empty());

    // Next pass: the failed read and the two that did not fit come back.
    let retry = h.tracker.on_listing(REPO, &[], t(600), 300);
    let retried: Vec<u32> = retry.pr_checks.iter().map(|(k, _)| k.issue).collect();
    assert_eq!(retried, vec![107, 108, 109], "failed and over-budget checks return");
    assert!(h.estimate(t(600)).is_empty(), "still no phantom estimates");
    for (key, _) in &retry.pr_checks {
        h.tracker
            .on_pr_resolved(key, PrState::Merged(t(550)), t(600));
    }

    let settled = h.tracker.on_listing(REPO, &[], t(900), 300);
    assert!(settled.pr_checks.is_empty(), "all ten resolved over the passes");
    assert!(h.tracker.pending().is_empty(), "every estimate got its outcome");
    assert!(h.tracker.item_keys().is_empty());
}

#[test]
fn estimates_emitted_after_the_landing_are_dropped_not_scored_by_a_reopen() {
    let mut h = Harness::new();
    h.tracker
        .on_listing(REPO, &[pr(131, 13, &["loom:pr"], -60)], t(0), 300);
    assert_eq!(h.estimate(t(0)).len(), 1);
    // A refresh emitted while the PR was already merged (the read is late).
    assert_eq!(h.estimate(t(300)).len(), 1);
    assert_eq!(h.tracker.pending().len(), 22, "two estimates x eleven land heuristics");
    h.tracker.on_listing(REPO, &[], t(600), 300);
    let merged = h
        .tracker
        .on_pr_resolved(&ItemKey::new(REPO, 13), PrState::Merged(t(200)), t(600));
    assert_eq!(merged.outcomes.len(), 11, "only the estimates made before the landing");
    assert!(merged.outcomes.iter().all(|o| o.estimate.as_of == t(0)));
    assert!(
        h.tracker.pending().is_empty(),
        "the post-landing estimate is dropped, so a reopen cannot score it"
    );
    assert_eq!(h.tracker.drain_dropped().orphaned, 11);

    // The reopen lands again: nothing stale is waiting for it.
    h.tracker
        .on_listing(REPO, &[pr(132, 13, &["loom:pr"], 700)], t(900), 300);
    h.estimate(t(900));
    h.tracker.on_listing(REPO, &[], t(1200), 300);
    let again =
        h.tracker
            .on_pr_resolved(&ItemKey::new(REPO, 13), PrState::Merged(t(1100)), t(1200));
    assert_eq!(again.outcomes.len(), 11, "a reopen starts a new series");
    assert!(again.outcomes.iter().all(|o| o.estimate.as_of == t(900)));
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
    assert_eq!(h.tracker.pending().len(), 22, "every emitted estimate waits for its outcome");
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
    let dropped = restarted.restore_pending(
        persisted
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect(),
        &Registry::builtin(),
    );
    assert_eq!(dropped, 0, "every heuristic that estimated is still registered");
    restarted.on_listing(REPO, &[pr(901, 90, &["loom:pr"], 100)], t(400), 300);
    restarted.on_listing(REPO, &[], t(700), 300);
    let merged = restarted.on_pr_resolved(&ItemKey::new(REPO, 90), PrState::Merged(t(650)), t(700));
    assert_eq!(merged.outcomes.len(), 11, "the join survived");
    assert_eq!(merged.outcomes[0].estimate.as_of, t(0));
}

#[test]
fn expire_drops_old_pending() {
    let mut h = Harness::new();
    h.tracker
        .on_listing(REPO, &[pr(111, 11, &["loom:pr"], -60)], t(0), 300);
    h.estimate(t(0));
    assert_eq!(h.tracker.expire(t(3600)).dropped, 0);
    assert_eq!(h.tracker.expire(t(0) + Duration::days(31)).dropped, 11);
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

// ------------------------------------------------------ features (#10201)

use crate::eta::explanation::{Explanation, Features};
use crate::eta::journal::JournalEntry;
use crate::eta::queue_features::{
    queue_features, reason, EventLog, QueueSubject, RosterEntry, NAMES as QUEUE_NAMES,
};
use crate::eta::tracker::{events_from_journal, ListedPr, ReadyPlan, ReadyRow, NOT_LISTED_YET};
use crate::types::{DispatchPlanContext, PlanSlots, PlanState, QueueDisposition, RowPlan};

const REVIEW: &str = "loom:review-requested";
const PLAN_NAMES: [&str; 5] = [
    "queue_ready",
    "queue_running",
    "max_concurrent",
    "active_sweeps_host",
    "repo_pr_open_skip",
];

fn listed(number: u32, label: &str, updated_secs: i64) -> ListedPr {
    ListedPr {
        number,
        labels: vec![label.to_string()],
        updated_at: Some(t(updated_secs)),
    }
}

fn plan_row(issue: u32, plan_state: PlanState, disposition: QueueDisposition) -> ReadyRow {
    ReadyRow {
        repo: REPO.to_string(),
        issue,
        plan: RowPlan {
            plan_state,
            position: (plan_state == PlanState::Next).then_some(1),
            ..RowPlan::default()
        },
        disposition,
        facts: crate::eta::tracker::IssueRow::default(),
        rank: issue as usize,
        detail: None,
    }
}

/// One ready row, one running sweep, one `pr-open-skip` row.
fn plan_rows() -> Vec<ReadyRow> {
    vec![
        plan_row(10, PlanState::Next, QueueDisposition::DeferredCapacity),
        plan_row(11, PlanState::Running, QueueDisposition::InFlight),
        plan_row(12, PlanState::Blocked, QueueDisposition::OpenPr),
    ]
}

fn plan_at(at: DateTime<Utc>, listing_failed: &[&str]) -> ReadyPlan {
    ReadyPlan {
        context: DispatchPlanContext {
            slots: PlanSlots {
                max_concurrent: 4,
                occupancy: Some(3),
                free: Some(1),
                ..PlanSlots::default()
            },
            tick_interval_secs: Some(60),
            complete: true,
            ..DispatchPlanContext::default()
        },
        at,
        listing_failed: listing_failed.iter().map(|s| (*s).to_string()).collect(),
    }
}

/// The ETA stage journal of the pass: a departure from `review_wait`, a PR
/// closed unmerged out of it, and a merge.
fn journal_rows() -> Vec<JournalEntry> {
    let at = |event: &str, pr: u32, stage: Stage, secs: i64, raw: serde_json::Value| {
        let mut row = JournalEntry::new(event, REPO, t(secs), &provenance());
        row.pr_number = Some(pr);
        row.stage = Some(stage);
        row.left_at = Some(t(secs));
        row.raw = raw;
        row
    };
    vec![
        at("label.transition", 400, Stage::ReviewWait, -1800, serde_json::json!({})),
        at(
            "pr.resolved",
            401,
            Stage::MergeWait,
            -7200,
            serde_json::json!({"state": "merged"}),
        ),
        at(
            "pr.resolved",
            402,
            Stage::ReviewWait,
            -600,
            serde_json::json!({"state": "closed"}),
        ),
    ]
}

fn primary(emitted: &[crate::eta::tracker::Emission], issue: u32, kind: Kind) -> &Explanation {
    &emitted
        .iter()
        .find(|e| e.primary && e.explanation.subject.issue == issue && e.explanation.kind == kind)
        .unwrap_or_else(|| panic!("{issue} {kind}"))
        .explanation
}

fn reason_of<'a>(e: &'a Explanation, name: &str) -> Option<&'a str> {
    e.features_omitted
        .iter()
        .find(|o| o.name == name)
        .map(|o| o.reason.as_str())
}

/// Train/serve parity: what the tracker records is exactly what
/// `queue_features` computes from the same listings, events and instant.
#[test]
fn the_tracker_records_queue_features_exactly_as_the_contract_computes_them() {
    let mut h = Harness::new();
    let prs = [
        pr(501, 50, &[REVIEW], -600),
        pr(502, 51, &[REVIEW], -1200),
        pr(503, 52, &["loom:pr"], -300),
    ];
    h.tracker.on_listing(REPO, &prs, t(0), 300);
    // #600 closes no issue: never a tracker item, still on the roster.
    let listing = vec![
        listed(501, REVIEW, -600),
        listed(502, REVIEW, -1200),
        listed(503, "loom:pr", -300),
        listed(600, REVIEW, -3000),
    ];
    let log = events_from_journal(&journal_rows(), t(-30));
    h.tracker
        .on_fleet_context(&[(REPO.to_string(), listing)], log.clone(), t(0));
    h.tracker
        .on_ready_queue(&plan_rows(), &plan_at(t(-30), &[]), t(0));
    h.tracker.on_dispatch(REPO, 70, "sweep-issue-70-1", t(10));
    let at = t(60);
    let emitted = h.estimate_all(at);

    // The same inputs, by hand.
    let entry = |pr: u32, stage: Stage, secs: i64| RosterEntry {
        repo: REPO.to_string(),
        pr,
        stage: Some(stage),
        entered_at: t(secs),
        known_at: t(0),
    };
    let roster = vec![
        entry(501, Stage::ReviewWait, -600),
        entry(502, Stage::ReviewWait, -1200),
        entry(503, Stage::MergeWait, -300),
        entry(600, Stage::ReviewWait, -3000),
    ];
    let subject = QueueSubject {
        repo: REPO.to_string(),
        pr: Some(501),
        current: Some((Stage::ReviewWait, t(-600))),
    };
    let expected = queue_features(&subject, &roster, &log, &[REPO.to_string()], at);
    let mut want = Features::default();
    let mut want_omitted = Vec::new();
    expected.write_to(&mut want, &mut want_omitted);

    let land = primary(&emitted, 50, Kind::Land);
    let features = land.features.as_ref().unwrap();
    let got = serde_json::to_value(features).unwrap();
    let want = serde_json::to_value(&want).unwrap();
    for name in QUEUE_NAMES {
        assert_eq!(got[name], want[name], "{name}");
        assert!(!got[name].is_null(), "{name}: a PR-stage item carries every feature");
    }
    // …and by hand, from the sources.
    assert_eq!(features.ahead, Some(2), "#502 and #600 entered review earlier");
    assert_eq!(features.n_stage_repo, Some(2));
    assert_eq!(features.open_prs_repo, Some(4), "#600 closes no issue but is an open PR");
    assert_eq!(features.exits_repo_1h, Some(2), "a departure and a PR closed unmerged");
    assert_eq!(features.merges_repo_24h, Some(1));
    assert_eq!(features.since_merge_sec, Some(7260));
    assert_eq!(features.fleet_scope_repos, Some(1));

    // The dispatch plan, on a started item (scope item 2).
    assert_eq!(features.queue_ready, Some(3));
    assert_eq!(features.queue_running, Some(1));
    assert_eq!(features.max_concurrent, Some(4));
    assert_eq!(features.active_sweeps_host, Some(3));
    assert_eq!(features.repo_pr_open_skip, Some(true));
    assert_eq!(features.queue_rank, None);
    assert_eq!(reason_of(land, "queue_rank"), Some(reason::NOT_APPLICABLE_STAGE));

    // A ready item: a queue position, no PR yet; every item gets the repo
    // values.
    let ready = primary(&emitted, 10, Kind::Start);
    let rf = ready.features.as_ref().unwrap();
    assert_eq!(rf.queue_rank, Some(1));
    assert_eq!((rf.merges_repo_24h, rf.open_prs_repo), (Some(1), Some(4)));
    assert_eq!(reason_of(ready, "ahead"), Some(reason::NO_PR_YET));
    // A ready row the plan holds (refused): no stage, and its own reason.
    let held = primary(&emitted, 12, Kind::Start);
    assert_eq!(reason_of(held, "ahead"), Some(reason::NO_STAGE));
    assert_eq!(reason_of(held, "queue_rank"), Some("no_dispatch_plan"));

    // An in-sweep item, on both kinds.
    for kind in [Kind::Finish, Kind::Land] {
        let sweep = primary(&emitted, 70, kind);
        let sf = sweep.features.as_ref().unwrap();
        assert_eq!(reason_of(sweep, "n_stage_fleet"), Some(reason::NO_PR_YET), "{kind}");
        assert_eq!(reason_of(sweep, "queue_rank"), Some(reason::NOT_APPLICABLE_STAGE));
        assert_eq!((sf.queue_running, sf.merges_fleet_6h), (Some(1), Some(1)), "{kind}");
    }

    // No #10201 feature ever falls back to `not_collected`.
    for e in emitted.iter().map(|e| &e.explanation) {
        for name in QUEUE_NAMES.iter().chain(&PLAN_NAMES).chain(&["queue_rank"]) {
            assert_ne!(reason_of(e, name), Some("not_collected"), "{name} on {}", e.subject.issue);
        }
    }
}

/// Nothing observed at or after an estimate's `as_of` reaches its features.
#[test]
fn a_view_or_plan_observed_after_as_of_is_not_read() {
    let mut h = Harness::new();
    h.tracker
        .on_listing(REPO, &[pr(501, 50, &[REVIEW], -600)], t(0), 300);
    let reasons = |h: &mut Harness, at: DateTime<Utc>| {
        let emitted = h.estimate(at);
        let land = primary(&emitted, 50, Kind::Land);
        let of = |names: &[&str]| {
            let mut r: Vec<String> = names
                .iter()
                .map(|n| reason_of(land, n).unwrap_or("-").to_string())
                .collect();
            r.dedup();
            r
        };
        (of(&QUEUE_NAMES), of(&PLAN_NAMES))
    };
    // Before any pass handed a view over.
    let (queue, plan) = reasons(&mut h, t(0));
    assert_eq!(
        (queue, plan),
        (vec![NOT_LISTED_YET.to_string()], vec!["no_dispatch_plan".to_string()])
    );

    // Observed after the estimate's instant: not read.
    let listing = vec![listed(501, REVIEW, -600)];
    h.tracker
        .on_fleet_context(&[(REPO.to_string(), listing)], EventLog::default(), t(500));
    h.tracker
        .on_ready_queue(&plan_rows(), &plan_at(t(500), &[]), t(500));
    let (queue, plan) = reasons(&mut h, t(400));
    assert_eq!(
        (queue, plan),
        (vec![NOT_LISTED_YET.to_string()], vec!["no_dispatch_plan".to_string()])
    );

    // Read once observed; the plan goes stale after 15 minutes.
    let (queue, plan) = reasons(&mut h, t(500 + 901));
    assert_eq!(
        queue,
        vec![
            "-".to_string(),
            reason::HISTORY_SHORTER_THAN_CAP.to_string(),
            "-".to_string()
        ]
    );
    assert_eq!(plan, vec!["stale_inputs".to_string()]);

    // A repo whose ready listing failed: its pr-open-skip state is unknown.
    h.tracker
        .on_ready_queue(&plan_rows(), &plan_at(t(1400), &[REPO]), t(1400));
    let (_, plan) = reasons(&mut h, t(1500 + 300));
    assert_eq!(plan, vec!["-".to_string(), reason::REPO_NOT_LISTED.to_string()]);
}

/// #10232: the read and stall features reach estimates through the tracker,
/// each with a specific reason while it has no value.
#[test]
fn the_tracker_records_read_and_stall_features() {
    use crate::eta::pr_features::{ReadKind, FEATURE_READ_BUDGET};
    use crate::eta::stall_features::{PoolReading, StallSnapshot};
    use serde_json::json;
    let mut h = Harness::new();
    h.tracker
        .on_listing(REPO, &[pr(501, 50, &[REVIEW], -600)], t(0), 300);
    let emitted = h.estimate(t(10));
    let land = primary(&emitted, 50, Kind::Land);
    assert_eq!(reason_of(land, "pr_additions"), Some("not_read_yet"));
    assert_eq!(reason_of(land, "breaker_state"), Some("no_stall_snapshot"));

    let reads = h
        .tracker
        .plan_feature_reads(&[REPO.to_string()], t(20), FEATURE_READ_BUDGET);
    let answers: Vec<_> = reads
        .into_iter()
        .map(|r| {
            let body = match r.kind {
                ReadKind::Pull => json!({
                    "state": "open", "merged_at": null, "additions": 12, "deletions": 4,
                    "changed_files": 3, "commits": 2, "head": {"sha": "abc"},
                    "updated_at": t(-600).to_rfc3339(),
                }),
                ReadKind::Issue => json!({
                    "body": "<!-- loom:points=3 -->", "user": {"login": "octocat"},
                    "updated_at": t(-9000).to_rfc3339(),
                }),
                ReadKind::Checks | ReadKind::Required => {
                    panic!("no head or base is known before the first PR read")
                }
            };
            (r, Some(body), t(30))
        })
        .collect();
    assert_eq!(answers.len(), 2, "one pulls/{{n}} and one issues/{{n}} read");
    h.tracker.on_feature_reads(&answers);
    h.tracker.on_stall_snapshot(StallSnapshot {
        observed_at: t(30),
        budgets: std::collections::BTreeMap::new(),
        writers: Default::default(),
        breaker: None,
        pool: PoolReading {
            usable: 2,
            total: 3,
        },
    });

    let emitted = h.estimate(t(400));
    let land = primary(&emitted, 50, Kind::Land);
    let f = land.features.as_ref().unwrap();
    assert_eq!((f.pr_additions, f.pr_commits), (Some(12), Some(2)));
    assert_eq!(f.points_marker, Some(3));
    assert_eq!(f.author.as_deref(), Some("octocat"));
    assert_eq!((f.pool_usable_accounts, f.pool_exhausted), (Some(2), Some(false)));
    assert_eq!(reason_of(land, "checks_pending"), Some("not_read_yet"));
    assert_eq!(reason_of(land, "complexity_marker"), Some("marker_absent"));
    assert_eq!(reason_of(land, "breaker_state"), Some("breaker_not_registered"));
    let names = crate::eta::pr_features::PR_SIZE_FEATURES
        .iter()
        .chain(&crate::eta::pr_features::CHECK_FEATURES)
        .chain(&crate::eta::pr_features::ALL_CHECK_FEATURES)
        .chain(&crate::eta::pr_features::ISSUE_FEATURES)
        .chain(&crate::eta::stall_features::NAMES);
    for name in names {
        assert_ne!(reason_of(land, name), Some("not_collected"), "{name}");
    }
}
