use super::*;
use crate::merge_pr::check_runs_rollup::classify;
use serde_json::{json, Value};

const SHA: &str = "fb3be7df2";

fn row(name: &str, conclusion: &str, suite: u64) -> Value {
    json!({"name": name, "status": "completed", "conclusion": conclusion,
           "check_suite": {"id": suite}})
}

fn rollup(rows: Vec<Value>) -> Value {
    json!({"total_count": rows.len(), "check_runs": rows})
}

#[allow(clippy::too_many_arguments)]
fn wrun(
    id: u64,
    wf: u64,
    suite: u64,
    created: &str,
    status: &str,
    conclusion: Option<&str>,
) -> Value {
    json!({"id": id, "workflow_id": wf, "check_suite_id": suite, "event": "pull_request",
           "head_sha": SHA, "created_at": created, "status": status,
           "conclusion": conclusion, "pull_requests": [{"number": 10417}]})
}

fn runs(list: Vec<Value>) -> Value {
    json!({"total_count": list.len(), "workflow_runs": list})
}

/// Apply the module the way both callers do, then classify.
fn failing_after(mut doc: Value, payload: &Value) -> String {
    if needs_runs(doc["check_runs"].as_array().unwrap()) {
        if let Ok(s) = superseded_suites(payload, SHA) {
            drop_superseded(&mut doc, &s);
        }
    }
    classify(&doc.to_string()).unwrap().failing
}

/// Two suites of ONE workflow on one SHA (the #10417 shape): suite 1 is the
/// older run, suite 2 the newer.
fn twin_rows(old: &str, new: &str) -> Value {
    rollup(vec![
        row("Structural Checks", old, 1),
        row("Daemon Checks", old, 1),
        row("Structural Checks", new, 2),
        row("Daemon Checks", new, 2),
    ])
}

fn twin_runs(old: &str, new: &str) -> Value {
    runs(vec![
        wrun(100, 7, 1, "2026-10-05T13:32:00Z", "completed", Some(old)),
        wrun(101, 7, 2, "2026-10-05T13:32:00Z", "completed", Some(new)),
    ])
}

#[test]
fn older_red_twin_is_dropped_when_newer_twin_is_green() {
    let f = failing_after(twin_rows("failure", "success"), &twin_runs("failure", "success"));
    assert_eq!(f, "");
}

#[test]
fn newer_red_twin_still_fails() {
    let f = failing_after(twin_rows("success", "failure"), &twin_runs("success", "failure"));
    assert_eq!(f, "Daemon Checks\nStructural Checks");
}

#[test]
fn cancelled_twin_is_dropped_even_when_newer() {
    // The twin that sat queued and was cancelled manually: its completed jobs
    // kept their red conclusions, but a cancelled run is not a verdict.
    let f = failing_after(twin_rows("success", "failure"), &twin_runs("success", "cancelled"));
    assert_eq!(f, "");
}

#[test]
fn no_completed_authority_drops_nothing() {
    // Older twin cancelled, newer still running: nothing has concluded, so
    // the cancelled twin's red stays (today's behaviour).
    let p = runs(vec![
        wrun(100, 7, 1, "2026-10-05T13:32:00Z", "completed", Some("cancelled")),
        wrun(101, 7, 2, "2026-10-05T13:32:01Z", "in_progress", None),
    ]);
    let doc = rollup(vec![
        row("Daemon Checks", "failure", 1),
        json!({"name": "Daemon Checks", "status": "in_progress", "conclusion": null,
               "check_suite": {"id": 2}}),
    ]);
    assert_eq!(failing_after(doc, &p), "Daemon Checks");
}

#[test]
fn running_newer_twin_is_never_superseded() {
    // An older green run must not settle a newer twin that is still running.
    let p = runs(vec![
        wrun(100, 7, 1, "2026-10-05T13:32:00Z", "completed", Some("failure")),
        wrun(101, 7, 2, "2026-10-05T13:33:00Z", "in_progress", None),
    ]);
    let s = superseded_suites(&p, SHA).unwrap();
    assert!(!s.contains_key(&2), "{s:?}");
}

#[test]
fn different_workflows_sharing_a_job_name_still_fail() {
    let doc = rollup(vec![row("lint", "failure", 1), row("lint", "success", 2)]);
    let p = runs(vec![
        wrun(100, 7, 1, "2026-10-05T13:32:00Z", "completed", Some("failure")),
        wrun(101, 8, 2, "2026-10-05T13:33:00Z", "completed", Some("success")),
    ]);
    assert_eq!(failing_after(doc, &p), "lint");
}

#[test]
fn same_sha_on_two_prs_is_not_a_twin() {
    let mut a = wrun(100, 7, 1, "2026-10-05T13:32:00Z", "completed", Some("failure"));
    a["pull_requests"] = json!([{"number": 1}]);
    let mut b = wrun(101, 7, 2, "2026-10-05T13:33:00Z", "completed", Some("success"));
    b["pull_requests"] = json!([{"number": 2}]);
    assert!(superseded_suites(&runs(vec![a, b]), SHA)
        .unwrap()
        .is_empty());
}

#[test]
fn unmappable_suite_is_classified_exactly_as_before() {
    // A third-party app's check-run: its suite maps to no workflow run.
    let doc = rollup(vec![
        row("codecov", "failure", 99),
        row("Daemon Checks", "failure", 1),
        row("Daemon Checks", "success", 2),
    ]);
    let before = classify(&doc.to_string()).unwrap();
    let after = failing_after(doc, &twin_runs("failure", "success"));
    assert_eq!(after, "codecov");
    assert!(before.failing.contains("codecov"));
}

#[test]
fn rows_without_suite_ids_are_untouched() {
    // forge_get_check_runs' Gitea shape, and any older payload.
    let doc = json!({"total_count": 2, "check_runs": [
        {"name": "ci", "status": "completed", "conclusion": "failure"},
        {"name": "ci", "status": "completed", "conclusion": "success"}]});
    assert!(!needs_runs(doc["check_runs"].as_array().unwrap()));
    let mut d = doc.clone();
    assert_eq!(drop_superseded(&mut d, &BTreeMap::from([(1, 2), (2, 1)])), 0);
    assert_eq!(d, doc);
}

#[test]
fn flat_check_suite_id_projection_is_read() {
    let doc = json!({"total_count": 2, "check_runs": [
        {"name": "ci", "status": "completed", "conclusion": "failure", "check_suite_id": 1},
        {"name": "ci", "status": "completed", "conclusion": "success", "check_suite_id": 2}]});
    assert_eq!(failing_after(doc, &twin_runs("failure", "success")), "");
}

#[test]
fn single_suite_needs_no_read() {
    let doc = rollup(vec![row("a", "failure", 1), row("b", "success", 1)]);
    assert!(!needs_runs(doc["check_runs"].as_array().unwrap()));
}

#[test]
fn green_duplicates_need_no_read() {
    let doc = twin_rows("success", "success");
    assert!(!needs_runs(doc["check_runs"].as_array().unwrap()));
}

#[test]
fn drop_lowers_total_count() {
    let mut doc = twin_rows("failure", "success");
    assert_eq!(drop_superseded(&mut doc, &BTreeMap::from([(1, 2)])), 2);
    assert_eq!(doc["total_count"], 2);
    assert_eq!(doc["check_runs"].as_array().unwrap().len(), 2);
}

#[test]
fn doubtful_payloads_supersede_nothing() {
    let mut missing = twin_runs("failure", "success");
    missing["workflow_runs"][0]
        .as_object_mut()
        .unwrap()
        .remove("check_suite_id");
    assert!(superseded_suites(&missing, SHA).is_err());
    let mut truncated = twin_runs("failure", "success");
    truncated["total_count"] = json!(150);
    assert!(superseded_suites(&truncated, SHA).is_err());
    assert!(superseded_suites(&json!({"total_count": 0}), SHA).is_err());
    // Doubt keeps the failure.
    assert_eq!(
        failing_after(twin_rows("failure", "success"), &missing),
        "Daemon Checks\nStructural Checks"
    );
}

#[test]
fn runs_for_another_sha_are_ignored() {
    let mut p = twin_runs("failure", "success");
    p["workflow_runs"][1]["head_sha"] = json!("other");
    assert!(superseded_suites(&p, SHA).unwrap().is_empty());
    assert!(superseded_suites(&twin_runs("failure", "success"), "")
        .unwrap()
        .is_empty());
}

#[test]
fn check_unique_to_the_older_run_is_kept() {
    // Older run red on "Extra Job"; the newer green twin has no such row.
    // Nothing newer vouches for it, so it must still fail.
    let doc = rollup(vec![
        row("Structural Checks", "success", 1),
        row("Extra Job", "failure", 1),
        row("Structural Checks", "success", 2),
    ]);
    assert_eq!(failing_after(doc, &twin_runs("failure", "success")), "Extra Job");
}

#[test]
fn same_name_dropped_while_unique_name_kept() {
    let mut doc = rollup(vec![
        row("Structural Checks", "failure", 1),
        row("Extra Job", "failure", 1),
        row("Structural Checks", "success", 2),
    ]);
    let s = superseded_suites(&twin_runs("failure", "success"), SHA).unwrap();
    assert_eq!(drop_superseded(&mut doc, &s), 1);
    assert_eq!(doc["total_count"], 2);
}
