//! The explanation record: recomputable, golden, capped, provenance-bearing.

use super::{history_a, input_at, EXPLANATION_GOLDEN};
use crate::eta::explanation::{
    Explanation, MAX_BYTES, TARGET_BYTES, TRUNCATED_FEATURES, TRUNCATED_GRIDS,
};
use crate::eta::heuristics::LandV1;
use crate::eta::simulate::run_explanation;
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
    assert_eq!(
        big.quantiles(),
        golden_explanation().quantiles(),
        "features never move the estimate"
    );

    // … and the grids second, when dropping features is not enough.
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
    assert_eq!(
        run_explanation(&huge),
        None,
        "a truncated grid is not recomputable, and says so"
    );
}

#[test]
fn explanation_without_provenance_does_not_parse() {
    let golden: serde_json::Value = serde_json::from_str(EXPLANATION_GOLDEN).unwrap();
    for field in ["version", "revision", "tree_state"] {
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
