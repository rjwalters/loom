//! Tests for the synchronous-path required-context conclusion gate (#10688).

use super::*;
use serde_json::json;

fn run(name: &str, status: &str, conclusion: Option<&str>, started: Option<&str>) -> CheckRun {
    CheckRun {
        name: name.into(),
        status: status.into(),
        conclusion: conclusion.map(String::from),
        started_at: started.map(|t| t.parse().unwrap()),
        actions_run_id: None,
        actions_job_id: None,
    }
}

fn done(name: &str, conclusion: &str) -> CheckRun {
    run(name, "completed", Some(conclusion), Some("2026-10-11T10:00:00Z"))
}

fn status(context: &str, state: &str) -> Status {
    Status {
        context: context.into(),
        state: state.into(),
    }
}

fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

fn named(items: &[(&str, &str)]) -> Named {
    items
        .iter()
        .map(|(a, b)| ((*a).into(), (*b).into()))
        .collect()
}

#[test]
fn nothing_required_is_clean() {
    assert_eq!(assess(&[], &[done("Lint", "failure")], &[], &[]), Verdict::Clean);
}

#[test]
fn all_required_green_is_clean_and_informational_red_is_ignored() {
    let runs = [
        done("CI Result", "success"),
        done("Lint", "failure"),
        done("Docs", "skipped"),
    ];
    assert_eq!(assess(&v(&["CI Result", "Docs"]), &runs, &[], &[]), Verdict::Clean);
}

#[test]
fn red_required_check_outside_ci_refuses_naming_it() {
    // 2am#3112: a required check from another app concluded failure.
    let runs = [
        done("CI Result", "success"),
        done("GitHub Bypass Check", "failure"),
    ];
    let verdict = assess(&v(&["CI Result", "GitHub Bypass Check"]), &runs, &[], &[]);
    assert_eq!(
        verdict,
        Verdict::Refuse {
            red: named(&[("GitHub Bypass Check", "failure")]),
            bypassed: vec![]
        }
    );
}

#[test]
fn every_bad_conclusion_is_red() {
    for c in [
        "failure",
        "cancelled",
        "timed_out",
        "action_required",
        "startup_failure",
        "stale",
        "weird",
    ] {
        let v1 = assess(&v(&["X"]), &[done("X", c)], &[], &[]);
        assert!(matches!(v1, Verdict::Refuse { .. }), "{c} must refuse, got {v1:?}");
    }
}

#[test]
fn missing_required_context_holds() {
    let verdict = assess(&v(&["CI Result", "Bypass"]), &[done("CI Result", "success")], &[], &[]);
    assert_eq!(
        verdict,
        Verdict::Hold {
            waiting: named(&[("Bypass", "not reported")]),
            bypassed: vec![]
        }
    );
}

#[test]
fn pending_required_context_holds() {
    let runs = [run(
        "Bypass",
        "in_progress",
        None,
        Some("2026-10-11T10:00:00Z"),
    )];
    assert_eq!(
        assess(&v(&["Bypass"]), &runs, &[], &[]),
        Verdict::Hold {
            waiting: named(&[("Bypass", "in_progress")]),
            bypassed: vec![]
        }
    );
}

#[test]
fn red_outranks_pending_elsewhere() {
    let runs = [done("A", "failure"), run("B", "queued", None, None)];
    assert!(matches!(assess(&v(&["A", "B"]), &runs, &[], &[]), Verdict::Refuse { .. }));
}

#[test]
fn rerun_in_flight_supersedes_the_old_red_run() {
    let runs = [done("A", "failure"), run("A", "queued", None, None)];
    assert!(matches!(assess(&v(&["A"]), &runs, &[], &[]), Verdict::Hold { .. }));
}

#[test]
fn latest_completed_run_decides() {
    let old_red = run("A", "completed", Some("failure"), Some("2026-10-11T09:00:00Z"));
    let new_green = run("A", "completed", Some("success"), Some("2026-10-11T10:00:00Z"));
    assert_eq!(
        assess(&v(&["A"]), &[new_green.clone(), old_red.clone()], &[], &[]),
        Verdict::Clean
    );
    let old_green = run("A", "completed", Some("success"), Some("2026-10-11T09:00:00Z"));
    let new_red = run("A", "completed", Some("failure"), Some("2026-10-11T10:00:00Z"));
    assert!(matches!(
        assess(&v(&["A"]), &[old_green, new_red], &[], &[]),
        Verdict::Refuse { .. }
    ));
}

#[test]
fn commit_statuses_count_as_required_evidence() {
    assert_eq!(
        assess(&v(&["ext/ci"]), &[], &[status("ext/ci", "success")], &[]),
        Verdict::Clean
    );
    assert!(matches!(
        assess(&v(&["ext/ci"]), &[], &[status("ext/ci", "error")], &[]),
        Verdict::Refuse { .. }
    ));
    assert!(matches!(
        assess(&v(&["ext/ci"]), &[], &[status("ext/ci", "pending")], &[]),
        Verdict::Hold { .. }
    ));
}

#[test]
fn worst_source_wins() {
    let verdict = assess(&v(&["A"]), &[done("A", "success")], &[status("A", "failure")], &[]);
    assert!(matches!(verdict, Verdict::Refuse { .. }));
}

#[test]
fn allow_red_required_overrides_exactly_the_named_context() {
    let runs = [done("A", "failure"), done("B", "success")];
    assert_eq!(
        assess(&v(&["A", "B"]), &runs, &[], &v(&["A"])),
        Verdict::Bypassed(named(&[("A", "failure")]))
    );
}

#[test]
fn allowing_one_context_does_not_admit_another_red_one() {
    let runs = [done("A", "failure"), done("B", "cancelled")];
    assert_eq!(
        assess(&v(&["A", "B"]), &runs, &[], &v(&["A"])),
        Verdict::Refuse {
            red: named(&[("B", "cancelled")]),
            bypassed: named(&[("A", "failure")])
        }
    );
}

#[test]
fn allow_red_required_never_passes_a_missing_or_pending_context() {
    let runs = [run("A", "in_progress", None, None)];
    assert!(matches!(
        assess(&v(&["A", "B"]), &runs, &[], &v(&["A", "B"])),
        Verdict::Hold { .. }
    ));
}

#[test]
fn messages_name_the_contexts() {
    let red = named(&[("GitHub Bypass Check", "failure")]);
    let msg = refusal("7", "deadbeef", &red);
    assert!(msg.starts_with("Merge blocked: PR #7"));
    assert!(msg.contains("`GitHub Bypass Check` (failure)"));
    assert!(msg.contains("--allow-red-required"));
    assert!(
        hold_reason("deadbeef", &named(&[("B", "not reported")])).contains("`B` (not reported)")
    );
    let body = bypass_comment("7", "deadbeef", &red);
    assert!(body.contains("--allow-red-required"));
    assert!(body.contains("`GitHub Bypass Check` (failure)"));
}

#[test]
fn parses_combined_status_and_fails_closed_on_truncation() {
    let ok = json!({"total_count": 1, "statuses": [{"context": "ext/ci", "state": "success"}]});
    assert_eq!(parse_statuses(&ok).unwrap(), vec![status("ext/ci", "success")]);
    let short = json!({"total_count": 2, "statuses": [{"context": "ext/ci", "state": "success"}]});
    assert!(parse_statuses(&short).is_err());
    assert!(parse_statuses(&json!({})).is_err());
}

#[test]
fn parses_check_run_rows() {
    let rows = json!([
        {"name": "A", "status": "completed", "conclusion": "failure", "started_at": "2026-10-11T10:00:00Z"},
        {"name": "B", "status": "queued", "conclusion": null}
    ]);
    let runs = parse_check_runs(&rows).unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1].conclusion, None);
    assert!(parse_check_runs(&json!([{"status": "completed"}])).is_err());
}
