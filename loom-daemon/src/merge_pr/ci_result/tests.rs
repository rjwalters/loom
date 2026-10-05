use super::*;
use serde_json::json;

fn runs(conclusion: &str, status: &str) -> Value {
    json!({"workflow_runs": [
        {"id": 1, "name": "CI", "status": "completed", "conclusion": "failure",
         "created_at": "2026-10-04T20:00:00Z", "html_url": "u1"},
        {"id": 2, "name": "CI", "status": status, "conclusion": conclusion,
         "created_at": "2026-10-04T21:00:00Z", "html_url": "u2"},
        {"id": 3, "name": "Other", "status": "completed", "conclusion": "success",
         "created_at": "2026-10-04T22:00:00Z", "html_url": "u3"}
    ]})
}

fn jobs_detect_cancelled() -> Value {
    json!({"jobs": [
        {"name": "Detect Changes", "conclusion": "cancelled"},
        {"name": "Repo Hygiene Checks", "conclusion": "cancelled"},
        {"name": "Rust Unit Tests (1/4)", "conclusion": "skipped"},
        {"name": "Structural Checks", "conclusion": "success"},
        {"name": "Daemon Checks", "conclusion": "success"},
        {"name": "Shell Syntax (macos-latest)", "conclusion": "success"}
    ]})
}

#[test]
fn latest_ci_run_only_ignores_other_workflows_and_older_runs() {
    let (id, ..) = latest_run(&runs("success", "completed")).unwrap();
    assert_eq!(id, 2);
}

#[test]
fn success_is_clean_without_listing_jobs() {
    let v = assess("7", "abc", &runs("success", "completed"), |_| panic!("no job read"));
    assert_eq!(v, Verdict::Clean);
}

// The #10403 fixture: Detect Changes cancelled, the three required contexts green.
#[test]
fn cancelled_detect_changes_refuses_naming_job_and_rerun_hint() {
    let v = assess("10403", "abc", &runs("failure", "completed"), |id| {
        assert_eq!(id, 2);
        Ok(jobs_detect_cancelled())
    });
    let Verdict::Refuse(m) = v else {
        panic!("{v:?}")
    };
    assert!(m.contains("Detect Changes (cancelled)"), "{m}");
    assert!(m.contains("Repo Hygiene Checks (cancelled)"), "{m}");
    assert!(!m.contains("Structural Checks"), "{m}");
    assert!(m.contains("gh run rerun --failed 2"), "{m}");
}

#[test]
fn cancelled_run_conclusion_refuses_even_if_jobs_unlistable() {
    let v = assess("1", "abc", &runs("cancelled", "completed"), |_| Err("boom".into()));
    let Verdict::Refuse(m) = v else {
        panic!("{v:?}")
    };
    assert!(m.contains("no individual failed/cancelled job could be listed"), "{m}");
}

#[test]
fn unanswerable_states_are_unverified_not_refused() {
    assert!(matches!(
        assess("1", "abc", &json!({"workflow_runs": []}), |_| unreachable!()),
        Verdict::Unverified(_)
    ));
    assert!(matches!(
        assess("1", "abc", &runs("", "in_progress"), |_| unreachable!()),
        Verdict::Unverified(_)
    ));
    assert!(matches!(
        assess("1", "abc", &json!({}), |_| unreachable!()),
        Verdict::Unverified(_)
    ));
}
