//! `merge_hold` (#10218): the label → stage definition, the shipped
//! heuristics' unchanged refusal, and the stage as a first-class path start
//! for a heuristic that models it.

use super::{as_of, history_a, input_at};
use crate::eta::explanation::Explanation;
use crate::eta::grid;
use crate::eta::heuristics::{estimate_path, PathRules, LAND_TWIN_OTTER, LAND_TWIN_OTTER_B};
use crate::eta::history::{SampleSource, StageSample, StageSamples};
use crate::eta::labels::{
    hold_labels, stage_from_pr_labels, MERGE_HOLD_COMPANION_LABELS, MERGE_HOLD_LABELS,
};
use crate::eta::simulate::{reachable_path, run, run_explanation, run_marks, PathSpec};
use crate::eta::{CurrentState, Kind, NoEstimateReason, Registry, Stage, DRAWS, STAGE_COUNT};
use chrono::Duration;

fn labels(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

// -- the stage itself --------------------------------------------------------

#[test]
fn merge_hold_is_the_last_stage_in_every_but_not_in_all() {
    assert_eq!(STAGE_COUNT, 7);
    assert_eq!(Stage::MergeHold.index(), 6);
    assert_eq!(Stage::EVERY.last(), Some(&Stage::MergeHold));
    assert!(!Stage::ALL.contains(&Stage::MergeHold), "pre-#10218 outputs iterate ALL");
    assert!(Stage::EVERY.iter().all(|s| *s <= Stage::MergeHold), "declared last");
    assert!(!Stage::MergeHold.is_role_attempt());
    assert_eq!(Stage::MergeHold.as_str(), "merge_hold");
    assert_eq!(serde_json::to_string(&Stage::MergeHold).unwrap(), "\"merge_hold\"");
    // Dense indices stay a permutation of 0..STAGE_COUNT.
    let mut indices: Vec<usize> = Stage::EVERY.iter().map(|s| s.index()).collect();
    indices.sort_unstable();
    assert_eq!(indices, (0..STAGE_COUNT).collect::<Vec<_>>());
}

// -- the one label → stage definition -------------------------------------------

#[test]
fn every_operator_hold_on_an_approved_pr_is_merge_hold() {
    for hold in MERGE_HOLD_LABELS {
        assert_eq!(
            stage_from_pr_labels(&labels(&["loom:pr", hold])),
            Ok(Stage::MergeHold),
            "{hold}"
        );
    }
    // The decision sub-kind with its base, and the mechanical sub-kind as a
    // companion, are still a merge hold.
    assert_eq!(
        stage_from_pr_labels(&labels(&["loom:pr", "loom:operator-only", "loom:operator-decision"])),
        Ok(Stage::MergeHold)
    );
    assert_eq!(
        stage_from_pr_labels(&labels(&[
            "loom:pr",
            "loom:operator-only",
            "loom:operator-mechanical"
        ])),
        Ok(Stage::MergeHold)
    );
}

#[test]
fn other_holds_and_holds_off_an_approved_pr_are_still_refused() {
    for hold in ["loom:blocked", "loom:needs-capability"] {
        assert_eq!(
            stage_from_pr_labels(&labels(&["loom:pr", hold])),
            Err(NoEstimateReason::Blocked),
            "{hold}"
        );
        // Together with an operator hold, still a refusal.
        assert_eq!(
            stage_from_pr_labels(&labels(&["loom:pr", "loom:operator", hold])),
            Err(NoEstimateReason::Blocked),
            "{hold} + loom:operator"
        );
    }
    for stage_label in ["loom:review-requested", "loom:changes-requested"] {
        for hold in MERGE_HOLD_LABELS {
            assert_eq!(
                stage_from_pr_labels(&labels(&[stage_label, hold])),
                Err(NoEstimateReason::Blocked),
                "{hold} on {stage_label}"
            );
        }
    }
    // A companion alone holds nothing up as a merge hold.
    assert_eq!(
        stage_from_pr_labels(&labels(&["loom:pr", "loom:operator-mechanical"])),
        Err(NoEstimateReason::Blocked)
    );
    // Contradictory verdict labels under an operator hold: a refusal.
    assert_eq!(
        stage_from_pr_labels(&labels(&["loom:pr", "loom:review-requested", "loom:operator"])),
        Err(NoEstimateReason::Blocked)
    );
}

#[test]
fn the_operator_star_alone_is_not_a_hold() {
    assert_eq!(
        stage_from_pr_labels(&labels(&["loom:pr", "loom:operator-priority"])),
        Ok(Stage::MergeWait)
    );
}

#[test]
fn every_merge_hold_label_is_a_registry_hold_label() {
    // A label-registry rename must fail here, not silently un-hold a PR.
    let holds = hold_labels();
    for label in MERGE_HOLD_LABELS.iter().chain(MERGE_HOLD_COMPANION_LABELS) {
        assert!(holds.contains(label), "{label} is not a hold label");
    }
}

// -- shipped heuristics refuse it, byte for byte as before ---------------------

/// Every path-engine heuristic. `land-2026-10-04-twin-otter` (#10243) is the
/// one exception, with its composition `-b` (#10244), which hands every PR
/// stage to it: they model `merge_hold` from the fit (see
/// `land_twin_otter::a_held_pr_is_estimated_from_the_fits_merge_hold_stage`),
/// and are exactly the heuristics that declare `models_hold` (#10284).
#[test]
fn every_shipped_heuristic_refuses_a_held_pr_exactly_as_before() {
    let registry = Registry::builtin();
    let held = input_at(Stage::MergeHold, 600, 0);
    let mut refused = held.clone();
    refused.current = CurrentState::Refused(NoEstimateReason::Blocked);
    let (models_hold, path_engine): (Vec<&str>, Vec<&str>) = registry
        .ids()
        .into_iter()
        .partition(|id| registry.get(id).unwrap().models_hold());
    assert_eq!(models_hold, vec![LAND_TWIN_OTTER, LAND_TWIN_OTTER_B]);
    assert_eq!(path_engine.len(), 7, "{path_engine:?}");
    for id in path_engine {
        let heuristic = registry.get(id).unwrap();
        let explanation = heuristic.estimate(&held, &history_a());
        assert_eq!(explanation.no_estimate_reason, Some(NoEstimateReason::Blocked), "{id}");
        assert!(explanation.current_stage.is_none(), "{id}");
        assert!(explanation.path.is_none(), "{id}");
        assert!(explanation.result.is_none(), "{id}");
        // The pre-#10218 input for a held PR was `Refused(Blocked)`.
        let before = heuristic.estimate(&refused, &history_a());
        assert_eq!(
            serde_json::to_string(&explanation).unwrap(),
            serde_json::to_string(&before).unwrap(),
            "{id}: a held PR's refusal moved"
        );
    }
}

#[test]
fn no_shipped_estimate_marks_merge_hold() {
    let registry = Registry::builtin();
    for kind in [Kind::Finish, Kind::Land] {
        for heuristic in registry.for_kind(kind) {
            for stage in Stage::ALL {
                let explanation = heuristic.estimate(&input_at(stage, 0, 0), &history_a());
                let Some(result) = explanation.result.as_ref() else {
                    continue;
                };
                assert!(
                    result
                        .stage_marks
                        .iter()
                        .all(|m| m.stage != Stage::MergeHold),
                    "{} from {stage}",
                    heuristic.id()
                );
                assert!(explanation
                    .stages
                    .iter()
                    .all(|e| e.stage != Stage::MergeHold));
            }
        }
    }
}

// -- first-class for a heuristic that models it ---------------------------------

/// 20 samples per stage, `300, 600, …` seconds.
fn hold_history() -> StageSamples {
    let mut samples = StageSamples::default();
    for (stage, base) in [(Stage::MergeHold, 1800), (Stage::MergeWait, 300)] {
        for i in 0..20_i64 {
            samples.stages.push(StageSample {
                repo: "rjwalters/loom".to_string(),
                stage,
                duration_sec: base * (i + 1),
                observed_at: as_of() - Duration::hours(i + 1),
                source: SampleSource::StageJournal,
                host: "host-a".to_string(),
                worked: None,
            });
        }
    }
    samples
}

fn hold_rules() -> PathRules {
    PathRules {
        id: "test-hold-aware",
        kind: Kind::Land,
        sources: &[SampleSource::StageJournal],
        always_merge: true,
        censoring: false,
        adjust: None,
        models_hold: true,
        half_life_sec: None,
        stall_term: false,
        residual_tail: false,
    }
}

#[test]
fn a_hold_aware_explanation_starts_at_merge_hold_and_recomputes_exactly() {
    let explanation =
        estimate_path(hold_rules(), &input_at(Stage::MergeHold, 600, 0), &hold_history());
    assert_eq!(explanation.no_estimate_reason, None);
    let path = explanation.path.as_ref().unwrap();
    assert_eq!(path.start, Stage::MergeHold);
    assert_eq!(path.terminal, Stage::MergeWait);
    assert!(explanation.branches.is_none(), "approved: no verdict ahead");
    let stages: Vec<Stage> = explanation.stages.iter().map(|e| e.stage).collect();
    assert_eq!(stages, vec![Stage::MergeHold, Stage::MergeWait]);
    assert_eq!(explanation.stages[0].conditioning.as_ref().unwrap().age_sec, 600);

    let result = explanation.result.as_ref().unwrap();
    let marked: Vec<Stage> = result.stage_marks.iter().map(|m| m.stage).collect();
    assert!(marked.contains(&Stage::MergeHold) && marked.contains(&Stage::MergeWait));
    let hold_mark = result
        .stage_marks
        .iter()
        .find(|m| m.stage == Stage::MergeHold)
        .unwrap();
    assert_eq!(hold_mark.p50_at, Some(explanation.as_of), "the start is marked at as_of");
    let wait_mark = result
        .stage_marks
        .iter()
        .find(|m| m.stage == Stage::MergeWait)
        .unwrap();
    assert_eq!(wait_mark.p50_at, Some(result.eta_p50_at), "the terminal mark is the estimate");

    // Only the serialized JSON crosses this line.
    let parsed: Explanation =
        serde_json::from_str(&serde_json::to_string(&explanation).unwrap()).unwrap();
    assert_eq!(run_explanation(&parsed), explanation.quantiles_with_p90());
    assert_eq!(run_marks(&parsed).as_ref(), Some(&result.stage_marks));
}

#[test]
fn a_merge_hold_path_spec_walks_hold_then_merge_wait() {
    assert_eq!(
        reachable_path(Stage::MergeHold, false, true, false),
        vec![Stage::MergeHold, Stage::MergeWait]
    );
    let mut grids: [Option<Vec<i64>>; STAGE_COUNT] = Default::default();
    let hold: Vec<i64> = (1..=20).map(|i| i * 1800).collect();
    let wait: Vec<i64> = (1..=20).map(|i| i * 300).collect();
    grids[Stage::MergeHold.index()] = Some(grid::grid_of(&hold));
    grids[Stage::MergeWait.index()] = Some(grid::grid_of(&wait));
    let spec = PathSpec {
        start: Stage::MergeHold,
        start_rework: 0,
        // The terminal is `merge_wait` whatever this says.
        include_merge: false,
        grids,
        ready_visits: 0,
        ready_offset_sec: 0,
        stop_at_dispatch: false,
        conditioning: None,
        p_by_attempt: Vec::new(),
        cap: 2,
        draws: DRAWS,
        seed: 7,
        residual_life: false,
        stall_offset_sec: 0,
    };
    let simulation = run(&spec).unwrap();
    assert_eq!(simulation.reached[Stage::MergeHold.index()], 1.0);
    assert_eq!(simulation.reached[Stage::MergeWait.index()], 1.0);
    assert_eq!(simulation.mean_visits[Stage::MergeHold.index()], 1.0);
    let marks = simulation.stage_marks(as_of());
    let terminal = marks.iter().find(|m| m.stage == Stage::MergeWait).unwrap();
    assert_eq!(terminal.p50_at, Some(as_of() + Duration::seconds(simulation.quantiles.1)));
    assert!(marks
        .iter()
        .any(|m| m.stage == Stage::MergeHold && m.p50_at.is_some()));
    assert!(simulation
        .contributions
        .p50_share
        .contains_key("merge_hold"));
}
