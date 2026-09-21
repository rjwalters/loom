#![allow(clippy::unwrap_used)]
use super::*;
use std::collections::BTreeSet;

fn fixture(run: &str) -> FixtureBundle {
    build(run, "2026-09-21T12:00:00Z".parse().unwrap()).unwrap()
}

#[test]
fn replay_is_byte_stable_but_new_trials_do_not_collide() {
    let a = fixture("trial-a");
    let b = fixture("trial-a");
    assert_eq!(
        serde_json::to_vec(&a.envelopes).unwrap(),
        serde_json::to_vec(&b.envelopes).unwrap()
    );
    assert_eq!(a.manifest, b.manifest);
    let c = fixture("trial-b");
    assert_ne!(a.manifest["spans"][0]["trace_id"], c.manifest["spans"][0]["trace_id"]);
    assert_eq!(
        a.manifest["expected_distinct"],
        json!({"spans":35,"logs":14,"metric_data_points":3})
    );
    assert!(build("../unsafe", "2026-09-21T12:00:00Z".parse().unwrap()).is_err());
}

#[test]
fn graph_separates_repair_attempts_repos_and_intentionally_missing_root() {
    let bundle = fixture("trial");
    let scenarios = bundle.manifest["scenarios"].as_array().unwrap();
    let repair = scenarios.iter().find(|s| s["name"] == "repair").unwrap();
    assert_eq!(
        repair["phases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["phase"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["builder", "judge", "doctor", "judge", "merge"]
    );
    assert_ne!(repair["phases"][1]["span_id"], repair["phases"][3]["span_id"]);
    let crash = scenarios
        .iter()
        .find(|s| s["name"] == "crash_incomplete")
        .unwrap();
    assert_eq!(crash["root_exported"], false);
    let spans = bundle.manifest["spans"].as_array().unwrap();
    assert!(!spans.iter().any(|s| s["span_id"] == crash["root_span_id"]));
    let ids: BTreeSet<_> = spans
        .iter()
        .map(|s| s["span_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 35);
    for span in spans {
        if let Some(parent) = span["parent_span_id"].as_str() {
            assert!(ids.contains(parent) || parent == crash["root_span_id"].as_str().unwrap());
        }
    }
    let alpha = scenarios
        .iter()
        .find(|s| s["name"] == "concurrent_alpha")
        .unwrap();
    let beta = scenarios
        .iter()
        .find(|s| s["name"] == "concurrent_beta")
        .unwrap();
    assert_eq!(alpha["issue"], beta["issue"]);
    assert_ne!(alpha["repo"], beta["repo"]);
    assert_ne!(alpha["trace_id"], beta["trace_id"]);
    let reject = scenarios
        .iter()
        .find(|s| s["name"] == "preflight_rejection")
        .unwrap();
    assert_eq!(reject["model_launch_expected"], false);
    assert!(spans
        .iter()
        .filter(|s| s["trace_id"] == reject["trace_id"])
        .all(|s| s["attributes"].get("loom.model").is_none()));
}

#[test]
fn missing_usage_is_not_zero_and_privacy_probe_is_only_expected_in_input() {
    let bundle = fixture("trial");
    let TelemetryRecord::TokensSnapshot(snapshot) = &bundle.envelopes.last().unwrap().record else {
        panic!("expected metrics")
    };
    assert_eq!(snapshot.accounts[0].usage_fraction, Some(0.0));
    assert_eq!(snapshot.accounts[1].usage_fraction, None);
    assert_eq!(bundle.manifest["metrics"][1]["present"], false);
    for span in bundle.manifest["spans"].as_array().unwrap() {
        assert!(span["attributes"].get("prompt.content").is_none());
    }
    assert!(serde_json::to_string(&bundle.envelopes)
        .unwrap()
        .contains(PRIVACY_SENTINEL));
    assert_eq!(bundle.manifest["backend_verification"], "not_performed");
}
