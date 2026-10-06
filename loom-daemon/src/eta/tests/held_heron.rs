//! `land-2026-10-06-held-heron` (#10523): the switching rule, parity with
//! twin-otter-b everywhere else, the simulator's rates, its forward
//! solution, recomputation, and point-in-time reads.

use super::land_twin_otter::{fit_as_of, fixture_fit, review_input};
use super::{as_of, history_a, input_at};
use crate::eta::episodes::{EpisodeEnd, EpisodeNext, StageEpisode};
use crate::eta::explanation::Explanation;
use crate::eta::flag_timeline::{FlagChange, RepoFlagChange};
use crate::eta::fleet::FleetSnapshot;
use crate::eta::hazard_sim::{
    self, fit, solve, HeldHeronRecord, SideState, AGE_BOUNDS_SEC, HORIZON_SEC, STEP_SEC, SUBSTEPS,
};
use crate::eta::heuristics::{
    side_state, LandHeldHeron, LandTwinOtterB, HELD_HERON_METHOD, LAND_BOLD_LARK, LAND_HELD_HERON,
    LAND_KEEN_WREN, LAND_LOOP_KITE, LAND_TANDEM_WREN, LAND_TWIN_OTTER, LAND_TWIN_OTTER_B,
};
use crate::eta::labels::FLAG_SEQUENCED;
use crate::eta::simulate::run_explanation;
use crate::eta::{
    CurrentState, EstimateInput, Heuristic, Kind, NoEstimateReason, Registry, Stage, StageSamples,
    Tier,
};
use chrono::{DateTime, Duration, Utc};
use std::sync::Arc;

const REPO: &str = "rjwalters/loom";
const OTHER: &str = "rjwalters/other";
const HOUR: i64 = 3_600;

fn h(hours: i64) -> DateTime<Utc> {
    as_of() - Duration::hours(hours)
}

fn episode(
    repo: &str,
    pr: u32,
    stage: Stage,
    entered: DateTime<Utc>,
    end: EpisodeEnd,
) -> StageEpisode {
    StageEpisode {
        repo: repo.to_string(),
        pr_number: pr,
        stage,
        entered_at: entered,
        end,
    }
}

fn left(at: DateTime<Utc>, next: EpisodeNext) -> EpisodeEnd {
    EpisodeEnd::Left { at, next }
}

fn flags(repo: &str, pr: u32, at: DateTime<Utc>, flags: u8) -> RepoFlagChange {
    RepoFlagChange {
        repo: repo.to_string(),
        change: FlagChange {
            pr_number: pr,
            at,
            flags,
        },
    }
}

/// A repo history with enough of everything: 10 free merges (2 h each),
/// 8 holds released after 1..8 h (each then merging after 1 h), 2 holds
/// merged directly, and 6 sequenced spells of 3 h inside a `merge_wait`.
fn hold_history(repo: &str) -> StageSamples {
    let mut history = StageSamples::default();
    for n in 0..10_u32 {
        let entered = h(200 - 12 * i64::from(n));
        history.episodes.push(episode(
            repo,
            100 + n,
            Stage::MergeWait,
            entered,
            left(entered + Duration::hours(2), EpisodeNext::Merged),
        ));
    }
    for n in 0..8_u32 {
        let entered = h(190 - 15 * i64::from(n));
        let released = entered + Duration::hours(1 + i64::from(n));
        history.episodes.push(episode(
            repo,
            200 + n,
            Stage::MergeHold,
            entered,
            left(released, EpisodeNext::Stage(Stage::MergeWait)),
        ));
        history.episodes.push(episode(
            repo,
            200 + n,
            Stage::MergeWait,
            released,
            left(released + Duration::hours(1), EpisodeNext::Merged),
        ));
    }
    for n in 0..2_u32 {
        let entered = h(100 - 20 * i64::from(n));
        history.episodes.push(episode(
            repo,
            300 + n,
            Stage::MergeHold,
            entered,
            left(entered + Duration::hours(5), EpisodeNext::Merged),
        ));
    }
    for n in 0..6_u32 {
        let entered = h(180 - 20 * i64::from(n));
        let pr = 400 + n;
        history.episodes.push(episode(
            repo,
            pr,
            Stage::MergeWait,
            entered,
            left(entered + Duration::hours(6), EpisodeNext::Merged),
        ));
        history.flag_changes.push(flags(repo, pr, entered, 0));
        history
            .flag_changes
            .push(flags(repo, pr, entered + Duration::hours(1), FLAG_SEQUENCED));
        history
            .flag_changes
            .push(flags(repo, pr, entered + Duration::hours(4), 0));
    }
    history
}

/// An approved PR in `stage` (`merge_hold` or `merge_wait`) with `labels`,
/// in the stage for `age_h` hours.
fn approved(stage: Stage, labels: &[&str], age_h: i64) -> EstimateInput {
    let mut input = input_at(stage, age_h * HOUR, 0);
    input.features.labels = Some(labels.iter().map(ToString::to_string).collect());
    input
}

fn held() -> EstimateInput {
    approved(Stage::MergeHold, &["loom:pr", "loom:operator"], 3)
}

fn sequenced() -> EstimateInput {
    approved(Stage::MergeWait, &["loom:pr", "loom:sequenced"], 3)
}

fn heron() -> LandHeldHeron {
    LandHeldHeron::new(Some(Arc::new(fixture_fit(fit_as_of()))))
}

fn base() -> LandTwinOtterB {
    LandTwinOtterB::new(Some(Arc::new(fixture_fit(fit_as_of()))))
}

/// twin-otter-b's explanation, re-identified as held-heron's.
fn as_heron(mut e: Explanation, input: &EstimateInput) -> Explanation {
    e.heuristic = LAND_HELD_HERON.to_string();
    e.estimate_id =
        crate::eta::estimate_id(&input.subject, Kind::Land, LAND_HELD_HERON, input.as_of);
    e
}

fn quantiles(e: &Explanation) -> (i64, i64, i64, i64) {
    e.quantiles_with_p90().expect("an answer")
}

#[test]
fn held_heron_is_a_registered_land_candidate_before_the_twin_otter_pair() {
    let fitted = Registry::with_fit(Some(Arc::new(fixture_fit(fit_as_of()))));
    for registry in [Registry::builtin(), fitted] {
        let land: Vec<&str> = registry.for_kind(Kind::Land).map(Heuristic::id).collect();
        // keen-wren (#10508), bold-lark (#10524) and loop-kite (#10521) are
        // registered between it and the twin-otter pair;
        // `land-2026-10-06-tandem-wren` (#10510) after the pair.
        assert_eq!(
            land[land.len() - 7..],
            [
                LAND_HELD_HERON,
                LAND_KEEN_WREN,
                LAND_BOLD_LARK,
                LAND_LOOP_KITE,
                LAND_TWIN_OTTER,
                LAND_TWIN_OTTER_B,
                LAND_TANDEM_WREN
            ]
        );
        let heron = registry.get(LAND_HELD_HERON).expect("registered");
        assert_eq!(heron.tier(), Tier::Candidate);
        assert!(heron.models_hold());
        assert_eq!(registry.tier_of(LAND_HELD_HERON), Some(Tier::Candidate));
        assert_ne!(registry.current(Kind::Land, None).id(), LAND_HELD_HERON);
    }
}

#[test]
fn the_switching_rule_routes_only_held_and_sequenced_prs() {
    assert_eq!(side_state(&held()).map(|(s, _)| s), Some(SideState::MergeHold));
    assert_eq!(side_state(&sequenced()).map(|(s, _)| s), Some(SideState::Sequenced));
    // A hold outranks sequencing.
    let both = approved(Stage::MergeHold, &["loom:pr", "loom:operator", "loom:sequenced"], 3);
    assert_eq!(side_state(&both).map(|(s, _)| s), Some(SideState::MergeHold));
    // Plain merge_wait, other stages, sequenced-but-not-approved, refusals.
    assert!(side_state(&approved(Stage::MergeWait, &["loom:pr"], 3)).is_none());
    assert!(side_state(&approved(
        Stage::ReviewWait,
        &["loom:review-requested", "loom:sequenced"],
        3
    ))
    .is_none());
    assert!(side_state(&approved(Stage::Doctor, &["loom:changes-requested"], 3)).is_none());
    let mut refused = held();
    refused.current = CurrentState::Refused(NoEstimateReason::Blocked);
    assert!(side_state(&refused).is_none());
}

#[test]
fn every_other_state_is_twin_otter_bs_explanation_byte_for_byte() {
    let mut rich = history_a();
    rich.merge(hold_history(REPO));
    let heron = heron();
    let base = base();
    let mut inputs: Vec<(String, EstimateInput)> = Vec::new();
    for stage in [Stage::ReviewWait, Stage::Doctor, Stage::MergeWait] {
        let mut input = review_input();
        if let CurrentState::At(current) = &mut input.current {
            current.stage = stage;
        }
        inputs.push((format!("{stage}"), input));
    }
    for stage in [Stage::SweepCurator, Stage::SweepBuilder] {
        inputs.push((format!("{stage}"), input_at(stage, 600, 0)));
    }
    let mut refused = review_input();
    refused.current = CurrentState::Refused(NoEstimateReason::Blocked);
    inputs.push(("blocked".to_string(), refused));
    for history in [&history_a(), &rich] {
        for (name, input) in &inputs {
            let got = heron.estimate(input, history);
            assert_eq!(got.held_heron, None, "{name}");
            assert_eq!(got, as_heron(base.estimate(input, history), input), "{name}");
        }
    }
    // Held or sequenced, but too little evidence: twin-otter-b again.
    for input in [held(), sequenced()] {
        let got = heron.estimate(&input, &history_a());
        assert_eq!(got.held_heron, None);
        assert_eq!(got, as_heron(base.estimate(&input, &history_a()), &input));
    }
}

#[test]
fn a_held_pr_is_answered_by_the_simulator_and_recomputes() {
    let history = hold_history(REPO);
    let input = held();
    let e = heron().estimate(&input, &history);
    assert_eq!(e.heuristic, LAND_HELD_HERON);
    assert_eq!(e.no_estimate_reason, None);
    assert_eq!(e.twin_otter, None);
    let combination = e.combination.as_ref().unwrap();
    assert_eq!(combination.method, HELD_HERON_METHOD);
    assert_eq!(combination.draws, 0);
    let record = e.held_heron.as_ref().expect("the simulator answered");
    assert_eq!(record.state, SideState::MergeHold);
    assert_eq!(record.level, "repo");
    assert_eq!(record.spell_age_sec, Some(3 * HOUR));
    assert_eq!(record.events.hold_release, 8);
    assert_eq!(record.events.hold_merge, 2);
    assert_eq!(e.result.as_ref().unwrap().samples_min, 10);
    let (p25, p50, p75, p90) = quantiles(&e);
    assert!(0 <= p25 && p25 <= p50 && p50 <= p75 && p75 <= p90, "{p25} {p50} {p75} {p90}");
    assert!(p90 < HORIZON_SEC);
    assert!(record.landed_by_horizon > 0.99);
    // Recomputes from the explanation alone, before and after JSON.
    assert_eq!(run_explanation(&e), Some((p25, p50, p75, p90)));
    let parsed: Explanation = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
    assert_eq!(parsed, e);
    assert_eq!(run_explanation(&parsed), Some((p25, p50, p75, p90)));
    // Deterministic: the same input gives the same explanation.
    assert_eq!(heron().estimate(&input, &history), e);
    // A hold outranks sequencing.
    let both = approved(Stage::MergeHold, &["loom:pr", "loom:operator", "loom:sequenced"], 3);
    let e = heron().estimate(&both, &history);
    assert_eq!(e.held_heron.unwrap().state, SideState::MergeHold);
}

#[test]
fn a_sequenced_pr_reads_its_spell_age_from_the_flag_timeline() {
    let mut history = hold_history(REPO);
    let input = sequenced();
    let pr = input.subject.pr_number.unwrap();
    history.flag_changes.push(flags(REPO, pr, h(10), 0));
    history
        .flag_changes
        .push(flags(REPO, pr, h(2), FLAG_SEQUENCED));
    let e = heron().estimate(&input, &history);
    let record = e.held_heron.as_ref().expect("the simulator answered");
    assert_eq!(record.state, SideState::Sequenced);
    assert_eq!(record.spell_age_sec, Some(2 * HOUR));
    assert_eq!(record.spell_exit_by_age_per_h.len(), AGE_BOUNDS_SEC.len() + 1);
    assert_eq!(record.events.desequence, 6);
    assert_eq!(e.result.as_ref().unwrap().samples_min, 6);
    assert_eq!(run_explanation(&e), Some(quantiles(&e)));
    // No timeline for the PR: answered at the pooled rate, age unknown.
    let e = heron().estimate(&input, &hold_history(REPO));
    let record = e.held_heron.as_ref().expect("still answered");
    assert_eq!(record.spell_age_sec, None);
    assert!(record.spell_exit_by_age_per_h.is_empty());
    assert_eq!(run_explanation(&e), Some(quantiles(&e)));
}

#[test]
fn a_thin_repo_falls_back_to_the_host_and_then_to_twin_otter_b() {
    // The subject's repo has nothing; another repo has everything.
    let other = hold_history(OTHER);
    let e = heron().estimate(&held(), &other);
    assert_eq!(e.held_heron.as_ref().unwrap().level, "host");
    // No hold exits anywhere: twin-otter-b.
    let mut thin = hold_history(REPO);
    thin.episodes.retain(|e| e.stage != Stage::MergeHold);
    let e = heron().estimate(&held(), &thin);
    assert_eq!(e.held_heron, None);
    assert_eq!(e, as_heron(base().estimate(&held(), &thin), &held()));
}

#[test]
fn facts_at_or_after_as_of_never_change_the_answer() {
    let history = hold_history(REPO);
    let mut leaked = history.clone();
    let after = as_of() + Duration::minutes(1);
    // New episodes, exits and flag changes after `as_of`.
    for n in 0..20_u32 {
        leaked.episodes.push(episode(
            REPO,
            900 + n,
            Stage::MergeHold,
            as_of(),
            left(after, EpisodeNext::Merged),
        ));
        leaked
            .flag_changes
            .push(flags(REPO, 900 + n, after, FLAG_SEQUENCED));
    }
    // An episode open at `as_of` whose later end is known now.
    leaked.episodes.push(episode(
        REPO,
        950,
        Stage::MergeHold,
        h(1),
        left(after, EpisodeNext::Stage(Stage::MergeWait)),
    ));
    let mut open = history.clone();
    open.episodes.push(episode(
        REPO,
        950,
        Stage::MergeHold,
        h(1),
        EpisodeEnd::Open { at: as_of() },
    ));
    for input in [held(), sequenced()] {
        assert_eq!(
            heron().estimate(&input, &leaked),
            heron().estimate(&input, &open),
            "{:?}",
            input.current
        );
    }
}

#[test]
fn rates_are_events_over_exposure_in_the_window() {
    let mut history = hold_history(REPO);
    // Outside the 14-day window: ignored.
    history.episodes.push(episode(
        REPO,
        990,
        Stage::MergeWait,
        h(24 * 20),
        left(h(24 * 20 - 1), EpisodeNext::Merged),
    ));
    let record = fit(&history, REPO, as_of(), SideState::MergeHold, Some(0)).unwrap();
    let e = record.events;
    assert_eq!((e.merge, e.hold_merge, e.hold_release), (10 + 8 + 6, 2, 8));
    assert_eq!((e.desequence, e.sequenced_merge, e.sequence_entry), (6, 0, 6));
    assert_eq!(e.hold_entry, 0);
    // Free merge_wait: 10 × 2 h + 8 × 1 h + 6 × (6 − 3) h = 46 h.
    assert_eq!(record.exposure.merge_wait_h, 46.0);
    // Holds: 1 + … + 8 = 36 h, plus 2 × 5 h.
    assert_eq!(record.exposure.merge_hold_h, 46.0);
    assert_eq!(record.exposure.sequenced_h, 18.0);
    let r = record.rates_per_h;
    assert_eq!(r.merge, (24.0_f64 / 46.0 * 1e6).round() / 1e6);
    assert_eq!(r.hold_release, (8.0_f64 / 46.0 * 1e6).round() / 1e6);
    assert_eq!(r.desequence, (6.0_f64 / 18.0 * 1e6).round() / 1e6);
    // Age bins: every bin positive, the shrinkage keeps an empty bin at the
    // pooled rate (no exposure past 72 h).
    assert_eq!(record.spell_exit_by_age_per_h.len(), 6);
    assert!(record.spell_exit_by_age_per_h.iter().all(|&x| x > 0.0));
    assert_eq!(record.spell_exit_by_age_per_h[5], r.hold_release);
    // Hour weights: an exposure-weighted mean of 1.
    assert_eq!(record.hold_exit_by_hour.len(), 24);
    assert!(record.hold_exit_by_hour.iter().all(|&w| w > 0.0));
}

#[test]
fn a_hold_that_ends_in_rework_is_censored_not_a_release() {
    let mut history = hold_history(REPO);
    // Holds sent back to `doctor` / `review_wait` instead of released to
    // `merge_wait`: exposure counts, but no release or exit event.
    for (n, next) in [Stage::Doctor, Stage::ReviewWait].into_iter().enumerate() {
        let entered = h(60 - 10 * n as i64);
        history.episodes.push(episode(
            REPO,
            600 + n as u32,
            Stage::MergeHold,
            entered,
            left(entered + Duration::hours(3), EpisodeNext::Stage(next)),
        ));
    }
    let record = fit(&history, REPO, as_of(), SideState::MergeHold, Some(0)).unwrap();
    assert_eq!(record.events.hold_release, 8);
    assert_eq!(record.events.hold_merge, 2);
    assert_eq!(record.exposure.merge_hold_h, 46.0 + 6.0);
}

#[test]
fn hour_weights_average_to_one_over_hold_exposure() {
    // One long hold per hour of day so the exposure is even, exits all at 09Z.
    let mut history = hold_history(REPO);
    history.episodes.retain(|e| e.stage != Stage::MergeHold);
    let day = as_of() - Duration::days(5);
    let nine = day.date_naive().and_hms_opt(9, 30, 0).unwrap().and_utc();
    for n in 0..6_u32 {
        history.episodes.push(episode(
            REPO,
            700 + n,
            Stage::MergeHold,
            nine - Duration::hours(24 + i64::from(n)),
            left(nine - Duration::days(i64::from(n) % 2), EpisodeNext::Stage(Stage::MergeWait)),
        ));
    }
    let record = fit(&history, REPO, as_of(), SideState::MergeHold, None).unwrap();
    let w = &record.hold_exit_by_hour;
    assert!(w[9] > 1.0, "{w:?}");
    assert!(w.iter().enumerate().all(|(h, &x)| h == 9 || x < w[9]));
    assert!(record.spell_exit_by_age_per_h.is_empty(), "age unknown: pooled");
}

fn record(state: SideState) -> HeldHeronRecord {
    HeldHeronRecord {
        state,
        level: "repo".to_string(),
        window_days: hazard_sim::WINDOW_DAYS,
        rates_per_h: hazard_sim::Rates::default(),
        events: hazard_sim::Counts::default(),
        exposure: hazard_sim::Exposure::default(),
        spell_age_sec: None,
        age_bounds_sec: AGE_BOUNDS_SEC.to_vec(),
        spell_exit_by_age_per_h: Vec::new(),
        hold_exit_by_hour: vec![1.0; 24],
        step_sec: STEP_SEC,
        substeps: SUBSTEPS,
        horizon_sec: HORIZON_SEC,
        landed_by_horizon: 0.0,
    }
}

#[test]
fn the_forward_solution_matches_the_closed_form() {
    // A hold merged directly at rate ln 2 per hour: the median is 1 h, the
    // p90 log2(10) h.
    let mut r = record(SideState::MergeHold);
    r.rates_per_h.hold_merge = std::f64::consts::LN_2;
    let s = solve(&r, as_of()).unwrap();
    let (p25, p50, p75, p90) = s.quantiles;
    let close = |got: i64, hours: f64| (got - (hours * 3600.0).round() as i64).abs() <= 5;
    assert!(close(p25, (4.0_f64 / 3.0).log2()), "{p25}");
    assert!(close(p50, 1.0), "{p50}");
    assert!(close(p75, 2.0), "{p75}");
    assert!(close(p90, 10.0_f64.log2()), "{p90}");
    // Release then merge: two exponential stages, each with mean 1 h, so the
    // median is the Erlang-2 median (1.678 h).
    let mut r = record(SideState::Sequenced);
    r.rates_per_h.desequence = 1.0;
    r.rates_per_h.merge = 1.0;
    let (_, p50, _, _) = solve(&r, as_of()).unwrap().quantiles;
    assert!((p50 - 6_041).abs() <= 30, "{p50}");
}

#[test]
fn a_chain_that_never_lands_answers_the_horizon_and_a_malformed_one_nothing() {
    let r = record(SideState::MergeHold);
    let s = solve(&r, as_of()).unwrap();
    assert_eq!(s.quantiles, (HORIZON_SEC, HORIZON_SEC, HORIZON_SEC, HORIZON_SEC));
    assert_eq!(s.landed_by_horizon, 0.0);
    for broken in [
        HeldHeronRecord {
            hold_exit_by_hour: vec![1.0; 23],
            ..record(SideState::MergeHold)
        },
        HeldHeronRecord {
            step_sec: 0,
            ..record(SideState::MergeHold)
        },
        HeldHeronRecord {
            substeps: 0,
            ..record(SideState::MergeHold)
        },
        HeldHeronRecord {
            spell_exit_by_age_per_h: vec![1.0; 2],
            ..record(SideState::MergeHold)
        },
    ] {
        assert_eq!(solve(&broken, as_of()), None);
    }
    let mut negative = record(SideState::MergeHold);
    negative.rates_per_h.merge = -1.0;
    assert_eq!(solve(&negative, as_of()), None);
}

#[test]
fn the_current_spell_is_released_at_its_own_ages_rate() {
    let history = hold_history(REPO);
    let young = approved(Stage::MergeHold, &["loom:pr", "loom:operator"], 0);
    let old = approved(Stage::MergeHold, &["loom:pr", "loom:operator"], 7);
    let e_young = heron().estimate(&young, &history);
    let e_old = heron().estimate(&old, &history);
    // Same hazards, different spell ages.
    let (a, b) = (e_young.held_heron.as_ref().unwrap(), e_old.held_heron.as_ref().unwrap());
    assert_eq!(a.spell_exit_by_age_per_h, b.spell_exit_by_age_per_h);
    assert_eq!((a.spell_age_sec, b.spell_age_sec), (Some(0), Some(7 * HOUR)));
    // Releases in this history come at 1..8 h, so a fresh hold's first hour
    // (no releases) is slower than the 4..12 h bin a 7-h hold is in: the
    // older hold is answered sooner.
    let bins = &a.spell_exit_by_age_per_h;
    assert!(bins[0] < bins[2], "{bins:?}");
    assert!(quantiles(&e_old).1 < quantiles(&e_young).1);
}

#[test]
fn the_fleet_snapshot_hands_its_flag_timeline_to_the_estimator() {
    let mut snapshot = FleetSnapshot::empty(REPO);
    let change = FlagChange {
        pr_number: 7,
        at: h(1),
        flags: FLAG_SEQUENCED,
    };
    snapshot.flag_changes.push(change);
    let history = snapshot.stage_samples();
    assert_eq!(
        history.flag_changes,
        vec![RepoFlagChange {
            repo: REPO.to_string(),
            change,
        }]
    );
    let mut merged = StageSamples::default();
    merged.merge(history.clone());
    merged.merge(history);
    assert_eq!(merged.flag_changes.len(), 2);
    assert_eq!(hazard_sim::sequenced_since(&merged, REPO, 7, as_of()), Some(h(1)));
    assert_eq!(hazard_sim::sequenced_since(&merged, REPO, 7, h(2)), None);
}
