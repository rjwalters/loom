//! `land-2026-10-08-ranked-rook` (#10921): the Judge's real pick order as
//! the review queue, its point-in-time reads, serving/replay parity, and the
//! heuristic that answers from it.

use super::hold_parity::{serve, Spec, AT};
use super::{as_of, input_at};
use crate::eta::backtest::{cases_from_pr_records, parse_pr_records};
use crate::eta::explanation::Explanation;
use crate::eta::heuristics::{
    LandRankedRook, LandTwinOtterB, LAND_HELD_HERON, LAND_QUICK_TERN, LAND_RANKED_ROOK,
    LAND_SWIFT_TERN, RANKED_ROOK_ORDER,
};
use crate::eta::history::{SampleSource, StageSample, StageSamples};
use crate::eta::planner_queue::{planner_position, PlannerPosition, PREFER_HUMAN_PRS};
use crate::eta::priority_features::{PriorityEntry, PriorityState};
use crate::eta::queue_features::{EventKind, EventLog, StageEvent};
use crate::eta::shadow_fleet::is_retired;
use crate::eta::simulate::run_explanation;
use crate::eta::stage_queue::{QueueScope, StageQueue, HALF_LIFE_SEC, WINDOW_SEC};
use crate::eta::tracker::EstimateContext;
use crate::eta::{
    estimate_id, CurrentState, EstimateInput, Heuristic, Kind, NoEstimateReason, Registry, Stage,
    Tier,
};
use crate::pr_latency::REVIEW_REQUESTED;
use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeMap;

const REPO: &str = "rjwalters/loom";
const OTHER: &str = "rjwalters/other";
const STAR: &str = "loom:operator-priority";

fn ago(min: i64) -> DateTime<Utc> {
    as_of() - Duration::minutes(min)
}

/// An open PR of `repo` in `stage` since `entered_min` ago, known a minute
/// before `as_of`, at `star`.
fn entry(
    repo: &str,
    pr: u32,
    stage: Stage,
    entered_min: i64,
    star: PriorityState,
) -> PriorityEntry {
    PriorityEntry {
        repo: repo.to_string(),
        pr,
        stage: Some(stage),
        entered_at: ago(entered_min),
        known_at: ago(1),
        star,
    }
}

/// Starred (level 1) since `min` ago, unstarred before.
fn starred_since(min: i64) -> PriorityState {
    PriorityState {
        changes: vec![(ago(min), 1)],
        ..PriorityState::default()
    }
}

fn exits(repo: &str, n: i64) -> EventLog {
    EventLog {
        from: None,
        events: (0..n)
            .map(|i| StageEvent {
                repo: repo.to_string(),
                pr: Some(500 + i as u32),
                stage: Some(Stage::ReviewWait),
                kind: EventKind::Exit,
                at: ago(30 + 60 * i),
                known_at: ago(30 + 60 * i),
            })
            .collect(),
    }
}

fn scope() -> Vec<String> {
    vec![REPO.to_string(), OTHER.to_string()]
}

/// The planner position of PR `pr` (own state `star`) against `roster`.
fn position(pr: u32, star: &PriorityState, roster: &[PriorityEntry]) -> PlannerPosition {
    planner_position(REPO, pr, Stage::ReviewWait, star, roster, &exits(REPO, 6), &scope(), as_of())
        .expect("a review_wait PR in scope has a position")
}

/// Three PRs waiting for review in one repo, oldest first: #1 entered
/// 3 h ago, #2 2 h ago, #3 1 h ago. `star1` is #1's own state.
fn three(star1: PriorityState) -> Vec<PriorityEntry> {
    vec![
        entry(REPO, 1, Stage::ReviewWait, 180, star1),
        entry(REPO, 2, Stage::ReviewWait, 120, PriorityState::default()),
        entry(REPO, 3, Stage::ReviewWait, 60, PriorityState::default()),
    ]
}

fn ahead_of(roster: &[PriorityEntry]) -> Vec<u32> {
    roster
        .iter()
        .map(|e| position(e.pr, &e.star, roster).items_ahead)
        .collect()
}

#[test]
fn the_judge_reviews_newest_first_and_a_starred_pr_jumps_the_queue() {
    // Unstarred: the listing order, newest first. FIFO would be 0, 1, 2.
    assert_eq!(ahead_of(&three(PriorityState::default())), [2, 1, 0]);
    // Star the oldest on the next pass (no refit): it goes to the head and
    // the other two each move back one.
    let starred = three(starred_since(10));
    assert_eq!(ahead_of(&starred), [0, 2, 1]);
    let head = position(1, &starred[0].star, &starred);
    assert_eq!((head.operator_level, head.queue_len), (1, 3));
    // A level-2 PR outranks a plain star.
    let mut high = starred.clone();
    high[2].star = PriorityState::at_level(2);
    assert_eq!(ahead_of(&high), [1, 2, 0]);
    // Other repos and other stages do not queue for this repo's Judge.
    let mut mixed = three(PriorityState::default());
    mixed.push(entry(OTHER, 9, Stage::ReviewWait, 30, PriorityState::default()));
    mixed.push(entry(REPO, 8, Stage::MergeWait, 30, PriorityState::default()));
    assert_eq!(ahead_of(&mixed[..3]), [2, 1, 0]);
    assert_eq!(position(1, &PriorityState::default(), &mixed).items_ahead, 2);
}

#[test]
fn the_fifo_queue_is_unchanged_beside_it() {
    use crate::eta::queue_features::RosterEntry;
    use crate::eta::stage_queue::stage_queue;
    let roster: Vec<RosterEntry> = three(starred_since(10))
        .into_iter()
        .map(|e| RosterEntry {
            repo: e.repo,
            pr: e.pr,
            stage: e.stage,
            entered_at: e.entered_at,
            known_at: e.known_at,
        })
        .collect();
    let fifo = |pr: u32, entered: i64| {
        stage_queue(
            REPO,
            pr,
            Stage::ReviewWait,
            ago(entered),
            &roster,
            &exits(REPO, 6),
            &[REPO.to_string()],
            as_of(),
        )
        .unwrap()
    };
    let got: Vec<u32> = [(1, 180), (2, 120), (3, 60)]
        .into_iter()
        .map(|(pr, entered)| fifo(pr, entered).items_ahead)
        .collect();
    assert_eq!(got, [0, 1, 2]);
    // `stage_queue` itself never fills the planner view.
    assert!(fifo(1, 180).planner.is_none());
}

#[test]
fn prefer_human_prs_is_pinned_off_in_serving_and_replay() {
    // Origin cannot be reconstructed point-in-time, so both sides order
    // with `preferHumanPrs` off (#10921, curator Q1); see `eta.md`.
    const { assert!(!PREFER_HUMAN_PRS) };
    let roster = three(PriorityState::default());
    assert!(!position(1, &PriorityState::default(), &roster).prefer_human_prs);
    let doc = include_str!("../../../../defaults/docs/eta.md");
    assert!(doc.contains("`preferHumanPrs` is pinned off"), "eta.md documents the pin");
}

#[test]
fn only_review_wait_in_scope_has_a_position_and_the_drain_is_per_repo() {
    let roster = three(PriorityState::default());
    let at = |repo: &str, stage: Stage| {
        planner_position(
            repo,
            1,
            stage,
            &PriorityState::default(),
            &roster,
            &exits(REPO, 6),
            &scope(),
            as_of(),
        )
    };
    assert!(at(REPO, Stage::Doctor).is_none());
    assert!(at(REPO, Stage::MergeWait).is_none());
    assert!(at("someone/else", Stage::ReviewWait).is_none());
    // Exits of another repo do not drain this one's queue.
    let mut log = exits(REPO, 6);
    log.events.extend(exits(OTHER, 20).events);
    let p = planner_position(
        REPO,
        1,
        Stage::ReviewWait,
        &PriorityState::default(),
        &roster,
        &log,
        &scope(),
        as_of(),
    )
    .unwrap();
    assert_eq!(p.exits, 6);
    assert!(p.drain_rate_per_hr > 0.0);
}

#[test]
fn nothing_known_at_or_after_as_of_moves_the_position() {
    let roster = three(PriorityState::default());
    let before = position(2, &PriorityState::default(), &roster);
    let mut later = roster.clone();
    // A PR first known at `as_of`, a star applied at `as_of`, and an exit
    // at `as_of`: none of them is knowable yet.
    later.push(PriorityEntry {
        known_at: as_of(),
        ..entry(REPO, 4, Stage::ReviewWait, 0, PriorityState::default())
    });
    later[0].star = PriorityState {
        changes: vec![(as_of(), 1)],
        ..PriorityState::default()
    };
    let mut log = exits(REPO, 6);
    log.events.push(StageEvent {
        repo: REPO.to_string(),
        pr: Some(1),
        stage: Some(Stage::ReviewWait),
        kind: EventKind::Exit,
        at: as_of(),
        known_at: as_of(),
    });
    let own_later = PriorityState {
        changes: vec![(as_of() + Duration::minutes(5), 2)],
        ..PriorityState::default()
    };
    let after =
        planner_position(REPO, 2, Stage::ReviewWait, &own_later, &later, &log, &scope(), as_of())
            .unwrap();
    assert_eq!(after, before);
    // A linked issue's star is not a PR label: the Judge does not see it.
    let mut linked = roster.clone();
    linked[0].star.linked_since = Some(ago(30));
    assert_eq!(position(2, &PriorityState::default(), &linked), before);
}

// -- serving and replay ---------------------------------------------------

const RR: &str = REVIEW_REQUESTED;
const APPROVED: &str = crate::pr_latency::APPROVED;

/// [`REPO`], fixture hours (see `hold_parity`):
/// - 60: review 10 h, approved 11 h (a review exit in the window);
/// - 61: review 12 h, starred 14 h;
/// - 62: review 13 h;
/// - 63: review 15 h.
///
/// Judge order at [`AT`]: 61 (starred), 63 (newest), 62.
fn specs() -> Vec<Spec> {
    let spec = |pr, steps| Spec {
        repo: super::fit_rows::REPO,
        pr,
        steps,
        merged: None,
        touched: None,
    };
    vec![
        spec(60, &[(10.0, &[RR]), (11.0, &[APPROVED])]),
        spec(61, &[(12.0, &[RR]), (14.0, &[RR, STAR])]),
        spec(62, &[(13.0, &[RR])]),
        spec(63, &[(15.0, &[RR])]),
    ]
}

/// Ten `merge_wait` samples of the fixture repo, all before [`AT`].
fn merge_history() -> StageSamples {
    let mut h = StageSamples::default();
    for i in 0..10_i64 {
        h.stages.push(StageSample {
            repo: super::fit_rows::REPO.to_string(),
            stage: Stage::MergeWait,
            duration_sec: 600 + i * 60,
            observed_at: super::fit_rows::h(AT) - Duration::hours(i + 30),
            source: SampleSource::SweepOutcome,
            host: "host-a".to_string(),
            worked: None,
        });
    }
    h
}

#[test]
fn serving_orders_the_review_queue_as_the_judge_picks() {
    let specs = specs();
    let mut tracker = serve(&specs);
    let registry = Registry::builtin();
    let history = merge_history();
    let repo_ids = BTreeMap::new();
    let ctx = EstimateContext {
        registry: &registry,
        current_start: None,
        current_finish: None,
        current_land: None,
        history: &history,
        refresh_secs: 300,
        host_id: Some("host-test"),
        repo_ids: &repo_ids,
        stalls: &super::NO_STALLS,
    };
    let keys: Vec<_> = specs[1..].iter().map(Spec::key).collect();
    let emissions = tracker.estimate(Some(&keys), &ctx, super::fit_rows::h(AT));
    let served = |pr: u32| {
        let e = emissions
            .iter()
            .find(|e| {
                e.explanation.heuristic == LAND_RANKED_ROOK
                    && e.explanation.subject.pr_number == Some(pr)
            })
            .unwrap_or_else(|| panic!("no ranked-rook estimate for PR {pr}"));
        let q =
            e.explanation.queue.clone().unwrap_or_else(|| {
                panic!("PR {pr} not answered from the queue: {:?}", e.explanation)
            });
        (q.items_ahead, q.fifo_items_ahead, q.operator_level, e.explanation.clone())
    };
    let (a61, f61, l61, e61) = served(61);
    let (a62, f62, _, e62) = served(62);
    let (a63, f63, _, e63) = served(63);
    assert_eq!((a61, a63, a62), (0, 1, 2), "starred first, then newest first");
    assert_eq!((f61, f62, f63), (Some(0), Some(1), Some(2)), "FIFO beside it");
    assert_eq!(l61, Some(1));
    let p50 = |e: &Explanation| e.result.as_ref().unwrap().p50_sec;
    assert!(p50(&e61) < p50(&e63) && p50(&e63) < p50(&e62));
}

/// One merged PR record of [`REPO`] on 2026-09-01: `labels` are
/// `(added, label, "HH:MM")`; merged at `merge`.
fn record(n: u32, merge: &str, labels: &[(bool, &str, &str)]) -> String {
    let events: Vec<String> = labels
        .iter()
        .map(|(added, label, at)| {
            let kind = if *added { "labeled" } else { "unlabeled" };
            format!(r#"{{"event":"{kind}","label":"{label}","at":"2026-09-01T{at}:00Z"}}"#)
        })
        .chain([format!(
            r#"{{"event":"merged","at":"2026-09-01T{merge}:00Z"}}"#
        )])
        .collect();
    format!(
        r#"{{"schema":"eta-pr-case/v1","repo":"{REPO}","number":{n},"created_at":"2026-08-31T23:00:00Z","state":"merged","merged_at":"2026-09-01T{merge}:00Z","closing_issues":[{}],"timeline_complete":true,"events":[{}]}}"#,
        n - 100,
        events.join(",")
    )
}

#[test]
fn replay_reconstructs_the_judge_order_through_the_serving_function() {
    let rr = "loom:review-requested";
    let jsonl = [
        // 300 enters review at 00:00 and is starred at 00:01.
        record(
            300,
            "02:00",
            &[
                (true, rr, "00:00"),
                (true, STAR, "00:01"),
                (false, rr, "01:00"),
                (true, "loom:pr", "01:00"),
            ],
        ),
        record(
            301,
            "02:00",
            &[
                (true, rr, "00:05"),
                (false, rr, "01:10"),
                (true, "loom:pr", "01:10"),
            ],
        ),
        record(
            302,
            "02:00",
            &[
                (true, rr, "00:10"),
                (false, rr, "01:20"),
                (true, "loom:pr", "01:20"),
            ],
        ),
        record(
            303,
            "02:00",
            &[
                (true, rr, "00:15"),
                (false, rr, "01:30"),
                (true, "loom:pr", "01:30"),
            ],
        ),
    ]
    .join("\n");
    let (cases, _) = cases_from_pr_records(&parse_pr_records(&jsonl).unwrap());
    let review = |pr: u32| {
        cases
            .iter()
            .find(|c| c.subject.pr_number == Some(pr) && c.stage == Stage::ReviewWait)
            .unwrap_or_else(|| panic!("no review case for {pr}"))
    };
    // 303 enters behind 300 (starred), 301 and 302: the Judge takes 300,
    // then 303 itself (newest). FIFO puts it last.
    let case = review(303);
    let queue = &case.queue[0];
    let planner = queue
        .planner
        .as_ref()
        .expect("replay fills the planner view");
    assert_eq!((planner.items_ahead, queue.items_ahead), (1, 3));
    assert_eq!(planner.queue_len, 4);
    // Parity: the same roster through the one function serving calls.
    let t = |hm: &str| -> DateTime<Utc> { format!("2026-09-01T{hm}:00Z").parse().unwrap() };
    let flags_star = PriorityState {
        changes: vec![(t("00:01"), 1)],
        ..PriorityState::default()
    };
    let roster: Vec<PriorityEntry> = [
        (300, "00:00", flags_star),
        (301, "00:05", PriorityState::default()),
        (302, "00:10", PriorityState::default()),
    ]
    .into_iter()
    .map(|(pr, at, star)| PriorityEntry {
        repo: REPO.to_string(),
        pr,
        stage: Some(Stage::ReviewWait),
        entered_at: t(at),
        known_at: t(at),
        star,
    })
    .collect();
    let direct = planner_position(
        REPO,
        303,
        Stage::ReviewWait,
        &PriorityState::default(),
        &roster,
        &EventLog::default(),
        &[REPO.to_string()],
        case.as_of,
    )
    .unwrap();
    assert_eq!(&direct, planner);
    // 301 entered behind starred 300 only.
    assert_eq!(review(301).queue[0].planner.as_ref().unwrap().items_ahead, 1);
}

#[test]
fn serving_and_replay_share_the_one_ordering() {
    // Both call sites go through `planner_position`, and nothing else in
    // the ETA calls the planner's ordering directly.
    let serving = include_str!("../tracker_features.rs");
    let replay = include_str!("../backtest/pr_cases.rs");
    for (name, source) in [("tracker_features.rs", serving), ("pr_cases.rs", replay)] {
        assert!(source.contains("planner_position("), "{name}");
        assert!(!source.contains("ordered_queue"), "{name} must not order on its own");
    }
    assert!(include_str!("../planner_queue.rs").contains("ordered_queue("));
}

// -- the heuristic -----------------------------------------------------------

fn history() -> StageSamples {
    let mut h = StageSamples::default();
    for i in 0..10_i64 {
        h.stages.push(StageSample {
            repo: REPO.to_string(),
            stage: Stage::MergeWait,
            duration_sec: 600 + i * 60,
            observed_at: ago(60 * (i + 1)),
            source: SampleSource::SweepOutcome,
            host: "host-a".to_string(),
            worked: None,
        });
    }
    h
}

fn review_queue(planner: Option<PlannerPosition>, fifo_ahead: u32) -> StageQueue {
    StageQueue {
        stage: Stage::ReviewWait,
        scope: QueueScope::Fleet,
        items_ahead: fifo_ahead,
        drain_rate_per_hr: 3.0,
        half_life_sec: HALF_LIFE_SEC,
        window_sec: WINDOW_SEC,
        exits: 40,
        planner,
    }
}

fn planned(items_ahead: u32, level: u8, rate: f64) -> PlannerPosition {
    PlannerPosition {
        role: "judge".to_string(),
        items_ahead,
        queue_len: 6,
        operator_level: level,
        prefer_human_prs: false,
        drain_rate_per_hr: rate,
        exits: 12,
    }
}

fn input(planner: Option<PlannerPosition>) -> EstimateInput {
    let mut i = input_at(Stage::ReviewWait, 0, 0);
    i.queue = vec![review_queue(planner, 2)];
    i
}

fn p50(e: &Explanation) -> i64 {
    e.result.as_ref().expect("an answer").p50_sec
}

#[test]
fn an_earlier_planner_position_is_an_earlier_eta() {
    let rook = LandRankedRook::default();
    let answers: Vec<Explanation> = [0, 1, 3]
        .into_iter()
        .map(|ahead| rook.estimate(&input(Some(planned(ahead, 0, 2.0))), &history()))
        .collect();
    assert!(p50(&answers[0]) < p50(&answers[1]) && p50(&answers[1]) < p50(&answers[2]));
    // (ahead + 1) departures at 2/h, plus the merge_wait service.
    let q = answers[1].queue.as_ref().unwrap();
    assert_eq!(q.wait_sec, 3600);
    assert_eq!(p50(&answers[1]), q.wait_sec + q.service_total_sec);
    assert_eq!(q.order.as_deref(), Some(RANKED_ROOK_ORDER));
    assert_eq!((q.items_ahead, q.fifo_items_ahead, q.scope.as_str()), (1, Some(2), "repo"));
    // Two PRs with the same FIFO count but different Judge positions differ.
    assert_ne!(p50(&answers[0]), p50(&answers[2]));
}

/// loom-experiments#21: every current heuristic gives a PR further back in
/// its repo's review queue a *shorter* ETA (Spearman -0.3 to -0.47), because
/// it conditions on the time already waited. Here, at identical elapsed
/// time, a later Judge position never gets an earlier p50 (or any earlier
/// quantile), and the elapsed time itself moves nothing: the position is
/// the state, so the wait is not conditioned on it a second time.
#[test]
fn a_later_position_never_gets_an_earlier_eta_and_elapsed_time_is_not_conditioned_on() {
    let rook = LandRankedRook::default();
    let at = |ahead: u32, age_sec: i64| {
        let mut i = input_at(Stage::ReviewWait, age_sec, 0);
        i.queue = vec![review_queue(Some(planned(ahead, 0, 1.7)), 0)];
        rook.estimate(&i, &history())
            .quantiles_with_p90()
            .expect("a queue answer")
    };
    for age_sec in [0, 600, 3 * 3600, 30 * 3600] {
        let mut last = at(0, age_sec);
        for ahead in 1..=12 {
            let q = at(ahead, age_sec);
            assert!(q.1 > last.1, "p50 at {ahead} ahead, age {age_sec}");
            assert!(q.0 >= last.0 && q.2 >= last.2 && q.3 >= last.3, "{q:?} < {last:?}");
            last = q;
        }
    }
    // No credit for time already waited: the same position answers the
    // same remaining time whatever the age.
    for ahead in [0, 3, 9] {
        assert_eq!(at(ahead, 0), at(ahead, 30 * 3600), "{ahead} ahead");
    }
}

#[test]
fn a_starred_pr_gets_the_earliest_review_eta() {
    let roster = three(starred_since(10));
    let rook = LandRankedRook::default();
    let eta = |e: &PriorityEntry| {
        let mut i = input(Some(position(e.pr, &e.star, &roster)));
        i.subject.pr_number = Some(e.pr);
        p50(&rook.estimate(&i, &history()))
    };
    let etas: Vec<i64> = roster.iter().map(eta).collect();
    // #1 is the oldest but starred; then #3 (newest), then #2.
    assert!(etas[0] < etas[2] && etas[2] < etas[1], "{etas:?}");
}

#[test]
fn deterministic_and_recomputed_from_the_explanation_alone() {
    let rook = LandRankedRook::default();
    let a = rook.estimate(&input(Some(planned(4, 1, 1.5))), &history());
    let b = rook.estimate(&input(Some(planned(4, 1, 1.5))), &history());
    assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap());
    let r = a.result.as_ref().unwrap();
    assert!(r.p25_sec <= r.p50_sec && r.p50_sec <= r.p75_sec && Some(r.p75_sec) <= r.p90_sec);
    let json: Explanation = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
    assert_eq!(run_explanation(&json), a.quantiles_with_p90());
    assert_eq!(a.queue.as_ref().unwrap().operator_level, Some(1));
}

#[test]
fn every_other_state_is_twin_otter_bs_answer_reidentified() {
    let rook = LandRankedRook::default();
    let base = LandTwinOtterB::default();
    let same = |input: &EstimateInput, name: &str| {
        let got = rook.estimate(input, &history());
        let mut want = base.estimate(input, &history());
        want.heuristic = LAND_RANKED_ROOK.to_string();
        want.estimate_id = estimate_id(&input.subject, Kind::Land, LAND_RANKED_ROOK, input.as_of);
        assert_eq!(got, want, "{name}");
        assert!(got.queue.is_none(), "{name}");
    };
    same(&input(None), "no planner view");
    same(&input(Some(planned(2, 0, 0.0))), "no review exit in the window");
    for stage in [Stage::Doctor, Stage::MergeWait, Stage::SweepBuilder] {
        let mut i = input_at(stage, 600, 0);
        i.queue = vec![review_queue(Some(planned(0, 0, 2.0)), 0)];
        same(&i, &format!("{stage}"));
    }
    let mut refused = input(Some(planned(0, 0, 2.0)));
    refused.current = CurrentState::Refused(NoEstimateReason::Blocked);
    same(&refused, "refused");
    // A later stage with no history: twin-otter-b too, never a guess.
    let got = rook.estimate(&input(Some(planned(0, 0, 2.0))), &StageSamples::default());
    assert!(got.queue.is_none());
}

#[test]
fn registered_as_a_land_candidate_in_swift_terns_retired_slot() {
    let registry = Registry::builtin();
    let land: Vec<&str> = registry.for_kind(Kind::Land).map(Heuristic::id).collect();
    let at = |id: &str| land.iter().position(|x| *x == id).unwrap();
    assert_eq!(at(LAND_RANKED_ROOK), at(LAND_QUICK_TERN) + 1);
    assert_eq!(at(LAND_HELD_HERON), at(LAND_RANKED_ROOK) + 1);
    let rook = registry.get(LAND_RANKED_ROOK).expect("registered");
    assert_eq!(rook.tier(), Tier::Candidate);
    assert!(rook.models_hold());
    assert_ne!(registry.current(Kind::Land, None).id(), LAND_RANKED_ROOK);
    assert!(!land.contains(&LAND_SWIFT_TERN));
    assert!(is_retired(LAND_SWIFT_TERN));
    assert!(registry
        .check_budget(crate::eta::shadow_fleet::DEFAULT_MAX_ACTIVE)
        .is_ok());
}
