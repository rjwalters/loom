use super::*;
use serde_json::json;

fn runs(conclusion: &str, status: &str) -> Value {
    json!({"workflow_runs": [
        {"id": 1, "name": "CI", "status": "completed", "conclusion": "failure",
         "head_sha": "abc", "created_at": "2026-10-04T20:00:00Z", "html_url": "u1"},
        {"id": 2, "name": "CI", "status": status, "conclusion": conclusion,
         "head_sha": "abc", "created_at": "2026-10-04T21:00:00Z", "html_url": "u2"},
        {"id": 3, "name": "Other", "status": "completed", "conclusion": "success",
         "head_sha": "abc", "created_at": "2026-10-04T22:00:00Z", "html_url": "u3"}
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

fn wf(names: &[&str]) -> Value {
    let list: Vec<Value> = names
        .iter()
        .map(|n| json!({"name": n, "state": "active"}))
        .collect();
    json!({"total_count": list.len(), "workflows": list})
}

fn with_ci() -> Result<Value, String> {
    Ok(wf(&["CI", "Release"]))
}

#[test]
fn latest_ci_run_only_ignores_other_workflows_and_older_runs() {
    let (id, ..) = latest_run(&runs("success", "completed"), "abc").unwrap();
    assert_eq!(id, 2);
}

#[test]
fn success_is_clean_without_listing_jobs() {
    let v = assess(
        "7",
        "abc",
        &runs("success", "completed"),
        || panic!("no wf read"),
        |_| panic!("no job read"),
    );
    assert_eq!(v, Verdict::Clean);
}

// The #10403 fixture: Detect Changes cancelled, the three required contexts green.
#[test]
fn cancelled_detect_changes_refuses_naming_job_and_rerun_hint() {
    let v = assess("10403", "abc", &runs("failure", "completed"), with_ci, |id| {
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
    let v = assess("1", "abc", &runs("cancelled", "completed"), with_ci, |_| Err("boom".into()));
    let Verdict::Refuse(m) = v else {
        panic!("{v:?}")
    };
    assert!(m.contains("no individual failed/cancelled job could be listed"), "{m}");
}

#[test]
fn unanswerable_states_are_unverified_not_refused() {
    // A `CI` workflow exists but has no run for this head, or it is still running.
    assert!(matches!(
        assess("1", "abc", &json!({"workflow_runs": []}), with_ci, |_| unreachable!()),
        Verdict::Unverified(_)
    ));
    assert!(matches!(
        assess("1", "abc", &runs("", "in_progress"), || unreachable!(), |_| unreachable!()),
        Verdict::Unverified(_)
    ));
    assert!(matches!(
        assess("1", "abc", &json!({}), with_ci, |_| unreachable!()),
        Verdict::Unverified(_)
    ));
}

// #10567: a run for a DIFFERENT head (the branch moved after CI ran, or the
// forge ignored the head_sha filter) is not evidence about this head.
#[test]
fn run_for_another_head_is_not_evidence() {
    let v = assess("1", "moved", &runs("success", "completed"), with_ci, |_| unreachable!());
    assert!(matches!(v, Verdict::Unverified(_)), "{v:?}");
    let no_sha = json!({"workflow_runs": [{"id": 4, "name": "CI", "status": "completed",
        "conclusion": "success", "created_at": "2026-10-04T23:00:00Z", "html_url": "u4"}]});
    let v = assess("1", "abc", &no_sha, with_ci, |_| unreachable!());
    assert!(
        matches!(v, Verdict::Unverified(_)),
        "a run without head_sha must not pass: {v:?}"
    );
    assert!(
        latest_run(&runs("success", "completed"), "").is_none(),
        "empty sha matches nothing"
    );
}

// #10567: "the repository has no CI workflow" is the explicit no-CI policy and
// passes; "could not tell" is Unreadable and holds.
#[test]
fn no_ci_workflow_is_a_separate_policy_from_cannot_inspect() {
    let empty = json!({"workflow_runs": []});
    let v = assess("1", "abc", &empty, || Ok(wf(&["Release", "Docs"])), |_| unreachable!());
    assert!(matches!(v, Verdict::NoCiWorkflow(_)), "{v:?}");
    let v = assess("1", "abc", &empty, || Err("HTTP 502".into()), |_| unreachable!());
    let Verdict::Unreadable(m) = v else {
        panic!("{v:?}")
    };
    assert!(m.contains("HTTP 502"), "{m}");
    // A truncated list cannot prove absence.
    let truncated = json!({"total_count": 150, "workflows": [{"name": "Release"}]});
    let v = assess("1", "abc", &empty, || Ok(truncated), |_| unreachable!());
    assert!(matches!(v, Verdict::Unreadable(_)), "{v:?}");
    // Unexpected payload shape is unreadable, never no-CI.
    let v = assess("1", "abc", &empty, || Ok(json!({"message": "Not Found"})), |_| unreachable!());
    assert!(matches!(v, Verdict::Unreadable(_)), "{v:?}");
    // A record with a missing or non-string name cannot prove `CI` is absent,
    // alone or mixed with valid records.
    for list in [
        json!([{}]),
        json!([{"name": 7}]),
        json!([{"name": null}]),
        json!([{"name": "Release"}, {}]),
        json!([{}, {"name": "Release"}]),
    ] {
        let payload = json!({"total_count": list.as_array().unwrap().len(), "workflows": list});
        let v = assess("1", "abc", &empty, || Ok(payload), |_| unreachable!());
        let Verdict::Unreadable(m) = v else {
            panic!("{list}: {v:?}")
        };
        assert!(m.contains("no string `name`"), "{m}");
    }
    // A found `CI` still wins over a malformed sibling record.
    let mixed = json!({"total_count": 2, "workflows": [{}, {"name": "CI"}]});
    let v = assess("1", "abc", &empty, || Ok(mixed), |_| unreachable!());
    assert!(matches!(v, Verdict::Unverified(_)), "{v:?}");
    // A disabled `CI` workflow is still a declared gate.
    let disabled =
        json!({"total_count": 1, "workflows": [{"name": "CI", "state": "disabled_manually"}]});
    let v = assess("1", "abc", &empty, || Ok(disabled), |_| unreachable!());
    assert!(matches!(v, Verdict::Unverified(_)), "{v:?}");
}
