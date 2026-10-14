//! Static checks for the step-dispatch alert rules shipped with the cutover
//! (#11909): `alerts/step-dispatch-stalled.json`,
//! `alerts/step-draw-pass-rate.json` and `alerts/step-slots-idle.json`.
//!
//! The step tick is the only item dispatch path, rolled out fleet-wide and
//! fixed forward, so these watch it from the first minute: no step spawned
//! while work waits (15 min), draws mostly refused by the pool gate or the
//! claim (30 min), and slots mostly idle while work waits (30 min). The
//! checks read the committed JSON only; no rule evaluator has run.
#![allow(clippy::unwrap_used, clippy::expect_used)]

const TEMPLATE: &str =
    include_str!("../../defaults/observability/signoz/alerts/queue-starvation.json");
const STALLED: &str =
    include_str!("../../defaults/observability/signoz/alerts/step-dispatch-stalled.json");
const PASS_RATE: &str =
    include_str!("../../defaults/observability/signoz/alerts/step-draw-pass-rate.json");
const IDLE: &str = include_str!("../../defaults/observability/signoz/alerts/step-slots-idle.json");

fn parse(s: &str) -> serde_json::Value {
    serde_json::from_str(s).expect("alert JSON must parse")
}

fn query(rule: &serde_json::Value) -> String {
    rule["condition"]["compositeQuery"]["chQueries"]["A"]["query"]
        .as_str()
        .expect("chQueries.A.query")
        .to_owned()
}

#[test]
fn step_alerts_share_the_template_schema_and_are_per_host() {
    let template = parse(TEMPLATE);
    let keys =
        |v: &serde_json::Value| -> Vec<String> { v.as_object().unwrap().keys().cloned().collect() };
    for (name, raw) in [
        ("step-dispatch-stalled", STALLED),
        ("step-draw-pass-rate", PASS_RATE),
        ("step-slots-idle", IDLE),
    ] {
        let rule = parse(raw);
        assert_eq!(keys(&rule), keys(&template), "{name}: top-level keys");
        assert_eq!(keys(&rule["condition"]), keys(&template["condition"]), "{name}");
        assert_eq!(rule["ruleType"], "threshold_rule", "{name}");
        assert_eq!(rule["alertType"], "METRIC_BASED_ALERT", "{name}");
        assert_eq!(rule["disabled"], false, "{name}: ships enabled");
        assert_eq!(rule["labels"]["loom_signal"], name.replace('-', "_"), "{name}");
        let q = query(&rule);
        for p in ["{{.start_timestamp_ms}}", "{{.end_timestamp_ms}}"] {
            assert!(q.contains(p), "{name}: query lacks {p}");
        }
        assert!(q.contains("GROUP BY host"), "{name}: attributable per host");
        assert!(!q.contains("\"loom:"), "{name}: no label literals");
    }
}

#[test]
fn the_thresholds_are_the_ruled_ones() {
    let stalled = parse(STALLED);
    assert_eq!(stalled["evalWindow"], "15m0s");
    assert_eq!(
        (stalled["condition"]["op"].as_str(), stalled["condition"]["target"].as_f64()),
        (Some("1"), Some(0.0))
    );
    let q = query(&stalled);
    assert!(q.contains("name = 'loom.dispatch.step'"));
    assert!(q.contains("'loom.dispatch.admission_result'] = 'spawned'"));
    assert!(q.contains("metric = 'loom.forge.stage_items'"));
    assert!(q.contains("max_concurrent > 0"), "dispatch hosts only");

    let pass = parse(PASS_RATE);
    assert_eq!(pass["evalWindow"], "30m0s");
    assert_eq!(pass["condition"]["op"], "1");
    assert_eq!(pass["condition"]["target"].as_f64(), Some(0.8));
    let q = query(&pass);
    for reason in [
        "pool_exhausted",
        "token_selection_failed",
        "'claimed'",
        "claim_lost",
    ] {
        assert!(q.contains(reason), "pass reason {reason}");
    }

    let idle = parse(IDLE);
    assert_eq!(idle["evalWindow"], "30m0s");
    assert_eq!(idle["condition"]["op"], "2", "below");
    assert_eq!(idle["condition"]["target"].as_f64(), Some(0.5));
    let q = query(&idle);
    assert!(
        q.contains("role = 'sweep' AND pool = ''"),
        "the slot total, not the per-role split"
    );
    assert!(q.contains("min_depth > 0"), "work waited the whole window");
}
