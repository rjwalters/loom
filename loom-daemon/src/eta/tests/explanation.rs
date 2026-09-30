//! The explanation record: recomputable, golden, capped, provenance-bearing.

use super::{history_a, input_at, EXPLANATION_GOLDEN};
use crate::eta::explanation::{
    Explanation, MAX_BYTES, TARGET_BYTES, TRUNCATED_DETAIL, TRUNCATED_FEATURES, TRUNCATED_GRIDS,
    TRUNCATED_STAGE_MARKS,
};
use crate::eta::heuristics::{FinishV1, LandV1};
use crate::eta::simulate::{run_explanation, run_marks};
use crate::eta::{Heuristic, Stage, EXPLANATION_SCHEMA};

fn golden_explanation() -> Explanation {
    LandV1.estimate(&input_at(Stage::ReviewWait, 0, 0), &history_a())
}

#[test]
fn explanation_recomputes_p50() {
    for (stage, age, rework) in [
        (Stage::ReviewWait, 0, 0),
        (Stage::ReviewWait, 900, 0),
        (Stage::Doctor, 300, 1),
        (Stage::SweepBuilder, 1800, 0),
        (Stage::MergeWait, 60, 0),
    ] {
        let explanation = LandV1.estimate(&input_at(stage, age, rework), &history_a());
        let expected = explanation.quantiles().expect("fixture estimates");
        // Only the serialized JSON crosses this line.
        let json = serde_json::to_string(&explanation).unwrap();
        let parsed: Explanation = serde_json::from_str(&json).unwrap();
        let recomputed = run_explanation(&parsed).expect("explanation is self-sufficient");
        assert_eq!(recomputed.1, expected.1, "p50 at {stage} age {age}");
        assert_eq!(recomputed, expected, "all quartiles at {stage} age {age}");
    }
}

#[test]
fn explanation_recomputes_stage_marks() {
    for (stage, age, rework) in [
        (Stage::ReviewWait, 0, 0),
        (Stage::ReviewWait, 900, 0),
        (Stage::Doctor, 300, 1),
        (Stage::SweepBuilder, 1800, 0),
        (Stage::MergeWait, 60, 0),
    ] {
        let explanation = LandV1.estimate(&input_at(stage, age, rework), &history_a());
        let expected = explanation.result.as_ref().expect("fixture estimates");
        // Only the serialized JSON crosses this line.
        let json = serde_json::to_string(&explanation).unwrap();
        let parsed: Explanation = serde_json::from_str(&json).unwrap();
        let recomputed = run_marks(&parsed).expect("explanation is self-sufficient");
        assert_eq!(recomputed, expected.stage_marks, "marks at {stage} age {age}");
        // Marks cover every stage, in stage order.
        assert_eq!(
            recomputed.iter().map(|m| m.stage).collect::<Vec<_>>(),
            Stage::ALL.to_vec(),
            "one mark per stage, in stage order, at {stage}"
        );
    }
}

#[test]
fn terminal_mark_p50_is_the_estimate() {
    for estimate in [
        LandV1.estimate(&input_at(Stage::ReviewWait, 0, 0), &history_a()),
        FinishV1.estimate(&input_at(Stage::SweepBuilder, 0, 0), &history_a()),
    ] {
        let result = estimate.result.as_ref().expect("fixture estimates");
        let terminal = estimate.path.as_ref().expect("path").terminal;
        let marks = &result.stage_marks;
        let terminal_mark = marks
            .iter()
            .find(|m| m.stage == terminal)
            .unwrap_or_else(|| panic!("a mark for the terminal stage {terminal}"));
        assert_eq!(
            terminal_mark.p50_at,
            Some(result.eta_p50_at),
            "the terminal mark's p50 is the estimate ({terminal})"
        );
        // The mark's own percentiles are ordered, and the other marks are
        // internally consistent where present.
        for mark in marks {
            if let (Some(p25), Some(p50), Some(p75)) = (mark.p25_at, mark.p50_at, mark.p75_at) {
                assert!(p25 <= p50 && p50 <= p75, "ordered at {}", mark.stage);
            } else {
                assert!(
                    mark.p25_at.is_none() && mark.p50_at.is_none() && mark.p75_at.is_none(),
                    "all-or-nothing times at {}",
                    mark.stage
                );
                assert_eq!(mark.mean_visits, None, "no visits at {}", mark.stage);
            }
        }
    }
}

#[test]
fn the_start_stage_is_marked_at_as_of() {
    // The path starts at the item's current stage, so its entry time is 0 on
    // every path and its mark reads `as_of` for all three percentiles — the
    // timeline's origin, "this stage began at/before now". The current
    // stage's age does NOT shift it; that is the definition, not a bug.
    let explanation = golden_explanation();
    let as_of = explanation.as_of;
    let start = explanation.path.as_ref().expect("path").start;
    let mark = explanation
        .result
        .as_ref()
        .expect("result")
        .stage_marks
        .iter()
        .find(|m| m.stage == start)
        .expect("a mark for the start stage");
    assert_eq!((mark.p25_at, mark.p50_at, mark.p75_at), (Some(as_of), Some(as_of), Some(as_of)));
    assert!(mark.mean_visits.is_some());
}

#[test]
fn stages_off_the_path_carry_no_times() {
    // From `merge_wait` the only reachable stage is `merge_wait` itself: the
    // other four get `None` times and no mean_visits, never a fabricated
    // value, and the terminal mark is the estimate.
    let explanation = LandV1.estimate(&input_at(Stage::MergeWait, 60, 0), &history_a());
    let result = explanation.result.as_ref().expect("fixture estimates");
    for mark in &result.stage_marks {
        if mark.stage == Stage::MergeWait {
            assert_eq!(mark.p50_at, Some(result.eta_p50_at));
            assert_eq!(mark.mean_visits, Some(1.0));
        } else {
            assert_eq!((mark.p25_at, mark.p50_at, mark.p75_at), (None, None, None));
            assert_eq!(mark.mean_visits, None, "{} is off the path", mark.stage);
        }
    }
}

#[test]
fn explanation_matches_golden_json() {
    let explanation = golden_explanation();
    let actual = serde_json::to_value(&explanation).unwrap();
    if std::env::var("LOOM_ETA_BLESS").is_ok_and(|v| v == "1") {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/eta/fixtures/explanation-golden.json");
        let mut text = serde_json::to_string_pretty(&actual).unwrap();
        text.push('\n');
        std::fs::write(path, text).unwrap();
    }
    let golden: serde_json::Value = serde_json::from_str(EXPLANATION_GOLDEN).unwrap();
    assert!(
        actual == golden,
        "land-v1 explanation drifted from fixtures/explanation-golden.json \
         (a shipped heuristic id is immutable; re-bless only for a schema change)"
    );
    assert_eq!(golden["schema"], EXPLANATION_SCHEMA);
    // Round trip: the golden text parses back into the same struct.
    let parsed: Explanation = serde_json::from_value(golden).unwrap();
    assert_eq!(parsed, explanation);
}

#[test]
fn explanation_size_within_cap() {
    let explanation = golden_explanation();
    assert!(explanation.truncated.is_empty());
    assert!(
        explanation.size_bytes() <= TARGET_BYTES,
        "an ordinary explanation meets the target: {} bytes",
        explanation.size_bytes()
    );

    // Oversized features are dropped first …
    let mut input = input_at(Stage::ReviewWait, 0, 0);
    input.features.labels = Some((0..2000).map(|i| format!("label-{i:>20}")).collect());
    let big = LandV1.estimate(&input, &history_a());
    assert!(big.size_bytes() <= MAX_BYTES, "{} bytes", big.size_bytes());
    assert_eq!(big.truncated, vec![TRUNCATED_FEATURES.to_string()]);
    assert_eq!(big.features, None);
    assert!(
        !big.stages[0].distribution.grid_sec.is_empty(),
        "grids survive when features suffice"
    );
    assert!(
        !big.result
            .as_ref()
            .expect("estimate")
            .stage_marks
            .is_empty(),
        "marks survive when features suffice"
    );
    assert_eq!(
        big.quantiles(),
        golden_explanation().quantiles(),
        "features never move the estimate"
    );

    // … and the grids second, when dropping features is not enough. The
    // marks ride in `result`, so they survive this tier: the timeline stays
    // self-contained even when the grids were dropped.
    let mut huge = golden_explanation();
    let mut probe = huge.clone();
    probe.features = None;
    probe.features_omitted.clear();
    let pad = MAX_BYTES - probe.size_bytes() + 64;
    huge.history_window
        .as_mut()
        .unwrap()
        .sources
        .push("x".repeat(pad));
    huge.enforce_cap();
    assert_eq!(
        huge.truncated,
        vec![TRUNCATED_FEATURES.to_string(), TRUNCATED_GRIDS.to_string()]
    );
    assert!(huge.size_bytes() <= MAX_BYTES, "{} bytes", huge.size_bytes());
    assert!(huge
        .stages
        .iter()
        .all(|e| e.distribution.grid_sec.is_empty()));
    assert!(
        !huge
            .result
            .as_ref()
            .expect("result always survives")
            .stage_marks
            .is_empty(),
        "the marks survive the grids tier"
    );
    assert_eq!(
        run_explanation(&huge),
        None,
        "a truncated grid is not recomputable, and says so"
    );

    // … and the marks third, when the grids alone were not enough either:
    // dropped as one named unit, the scalars in `result` always survive.
    let mut huge = golden_explanation();
    let mut probe = huge.clone();
    probe.features = None;
    probe.features_omitted.clear();
    for entry in &mut probe.stages {
        entry.distribution.grid_pct.clear();
        entry.distribution.grid_sec.clear();
    }
    let pad = MAX_BYTES - probe.size_bytes() + 64;
    huge.history_window
        .as_mut()
        .unwrap()
        .sources
        .push("x".repeat(pad));
    huge.enforce_cap();
    assert_eq!(
        huge.truncated,
        vec![
            TRUNCATED_FEATURES.to_string(),
            TRUNCATED_GRIDS.to_string(),
            TRUNCATED_STAGE_MARKS.to_string()
        ]
    );
    assert!(huge.size_bytes() <= MAX_BYTES, "{} bytes", huge.size_bytes());
    let result = huge.result.as_ref().expect("result always survives");
    assert!(result.stage_marks.is_empty());
    assert_eq!(
        result.p50_sec,
        golden_explanation()
            .result
            .as_ref()
            .expect("golden")
            .p50_sec,
        "the terminal scalar never moves"
    );
}

#[test]
fn explanation_without_provenance_does_not_parse() {
    let golden: serde_json::Value = serde_json::from_str(EXPLANATION_GOLDEN).unwrap();
    for field in ["version", "revision", "tree_state", "complete"] {
        let mut value = golden.clone();
        value["loom"].as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<Explanation>(value).is_err(),
            "loom.{field} is required"
        );
    }
    let mut value = golden.clone();
    value.as_object_mut().unwrap().remove("loom");
    assert!(serde_json::from_value::<Explanation>(value).is_err());
    let mut value = golden;
    value.as_object_mut().unwrap().remove("heuristic");
    assert!(serde_json::from_value::<Explanation>(value).is_err(), "heuristic is required");
}

#[test]
fn every_null_feature_has_a_reason() {
    let explanation = golden_explanation();
    let features = serde_json::to_value(explanation.features.as_ref().unwrap()).unwrap();
    for name in crate::eta::explanation::Features::NAMES {
        let is_null = features[name].is_null();
        let has_reason = explanation.features_omitted.iter().any(|o| o.name == name);
        assert_eq!(is_null, has_reason, "{name}");
    }
    assert_eq!(
        features.as_object().unwrap().len(),
        crate::eta::explanation::Features::NAMES.len(),
        "NAMES lists every feature"
    );
}

#[test]
fn explanation_cap_holds_even_when_grids_are_not_enough() {
    let mut huge = golden_explanation();
    huge.history_window
        .as_mut()
        .unwrap()
        .sources
        .push("x".repeat(MAX_BYTES + 2048));
    huge.enforce_cap();
    assert!(huge.size_bytes() <= MAX_BYTES, "{} bytes", huge.size_bytes());
    assert_eq!(
        huge.truncated,
        vec![
            TRUNCATED_FEATURES.to_string(),
            TRUNCATED_GRIDS.to_string(),
            TRUNCATED_STAGE_MARKS.to_string(),
            TRUNCATED_DETAIL.to_string()
        ]
    );
    // Identity, provenance and the numbers survive the last resort.
    let golden = golden_explanation();
    assert_eq!(huge.estimate_id, golden.estimate_id);
    assert_eq!(huge.loom, golden.loom);
    assert_eq!(huge.quantiles(), golden.quantiles());
}
