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

// #11330: the refusal ends in a parseable classification line so a driver can
// choose `gh run rerun --failed <id>` without scraping prose.
#[test]
fn refusal_carries_the_ci_result_failed_token() {
    let v = assess("10403", "abc", &runs("failure", "completed"), with_ci, |_| {
        Ok(json!({"jobs": [
            {"name": "Detect Changes", "conclusion": "cancelled"},
            {"name": "Rust Unit Tests (1/4)", "conclusion": "failure"},
            {"name": "Structural Checks", "conclusion": "success"}
        ]}))
    });
    let Verdict::Refuse(m) = v else {
        panic!("{v:?}")
    };
    assert!(m.starts_with("Merge blocked:"), "{m}");
    assert_eq!(
        m.lines().last(),
        Some("LOOM-CI-RESULT-FAILED run=2 jobs=Detect Changes,Rust Unit Tests (1/4)")
    );
}

#[test]
fn ci_result_failed_token_without_listable_jobs() {
    let v = assess("1", "abc", &runs("cancelled", "completed"), with_ci, |_| Err("boom".into()));
    let Verdict::Refuse(m) = v else {
        panic!("{v:?}")
    };
    assert_eq!(m.lines().last(), Some("LOOM-CI-RESULT-FAILED run=2 jobs=-"));
}

// #12039: a `CI` workflow registered only from another branch is listed by
// `actions/workflows` but is no workflow this PR runs.
fn wf_ci_at(path: &str) -> Result<Value, String> {
    Ok(json!({"total_count": 2, "workflows": [
        {"name": "CI", "state": "active", "path": path},
        {"name": "Release", "state": "active", "path": ".github/workflows/release.yml"}
    ]}))
}

fn no_runs() -> Value {
    json!({"workflow_runs": []})
}

#[test]
fn listed_ci_absent_at_base_and_head_is_no_ci_workflow() {
    let p = ".github/workflows/ci.yml";
    let v = assess_with(
        "83",
        "abc",
        &no_runs(),
        || wf_ci_at(p),
        |path| workflow_applies(path, "main", "abc", |_, _| FileAt::Absent),
        |_| panic!("no job read"),
    );
    let Verdict::NoCiWorkflow(m) = v else {
        panic!("{v:?}")
    };
    assert!(m.contains(p) && m.contains("#12039"), "{m}");
}

#[test]
fn listed_ci_present_at_base_keeps_unverified() {
    let mut reads = Vec::new();
    let v = assess_with(
        "83",
        "abc",
        &no_runs(),
        || wf_ci_at(".github/workflows/ci.yml"),
        |path| {
            workflow_applies(path, "main", "abc", |_, r| {
                reads.push(r.to_string());
                if r == "main" {
                    FileAt::Present
                } else {
                    FileAt::Absent
                }
            })
        },
        |_| panic!("no job read"),
    );
    assert!(matches!(v, Verdict::Unverified(_)), "{v:?}");
    assert_eq!(reads, ["main"], "base present short-circuits the head read");
}

#[test]
fn listed_ci_present_only_at_head_keeps_unverified() {
    let v = assess_with(
        "83",
        "abc",
        &no_runs(),
        || wf_ci_at(".github/workflows/ci.yml"),
        |path| {
            workflow_applies(path, "main", "abc", |_, r| {
                if r == "abc" {
                    FileAt::Present
                } else {
                    FileAt::Absent
                }
            })
        },
        |_| panic!("no job read"),
    );
    assert!(matches!(v, Verdict::Unverified(_)), "{v:?}");
}

#[test]
fn existence_check_error_is_unverified_not_no_ci() {
    for (base, head) in [
        (FileAt::Unknown("HTTP 502".into()), FileAt::Absent),
        (FileAt::Absent, FileAt::Unknown("HTTP 403".into())),
    ] {
        let v = assess_with(
            "83",
            "abc",
            &no_runs(),
            || wf_ci_at(".github/workflows/ci.yml"),
            |path| {
                workflow_applies(path, "main", "abc", |_, r| {
                    if r == "main" {
                        base.clone()
                    } else {
                        head.clone()
                    }
                })
            },
            |_| panic!("no job read"),
        );
        let Verdict::Unverified(m) = v else {
            panic!("{v:?}")
        };
        assert!(m.contains("could not be read"), "{m}");
    }
    // The base ref itself could not be resolved.
    let v = assess_with(
        "83",
        "abc",
        &no_runs(),
        || wf_ci_at(".github/workflows/ci.yml"),
        |_| Err("could not read PR #83's base ref".into()),
        |_| panic!("no job read"),
    );
    assert!(matches!(v, Verdict::Unverified(_)), "{v:?}");
}

#[test]
fn ci_record_without_path_always_applies() {
    let v = assess_with(
        "83",
        "abc",
        &no_runs(),
        || Ok(json!({"total_count": 1, "workflows": [{"name": "CI"}]})),
        |_| panic!("a pathless record is never existence-checked"),
        |_| panic!("no job read"),
    );
    assert!(matches!(v, Verdict::Unverified(_)), "{v:?}");
}

#[test]
fn exact_head_run_skips_existence_check() {
    let v = assess_with(
        "7",
        "abc",
        &runs("success", "completed"),
        || panic!("no wf read"),
        |_| panic!("no existence read"),
        |_| panic!("no job read"),
    );
    assert_eq!(v, Verdict::Clean);
}

#[test]
fn contents_read_classification_only_not_found_is_absent() {
    assert_eq!(classify_contents_read(&Ok("{}".into())), FileAt::Present);
    assert_eq!(
        classify_contents_read(&Err("gh api x failed: gh: Not Found (HTTP 404)".into())),
        FileAt::Absent
    );
    assert!(matches!(
        classify_contents_read(&Err(
            "gh api x failed: gh: No commit found for the ref abc (HTTP 404)".into()
        )),
        FileAt::Unknown(_)
    ));
    assert!(matches!(
        classify_contents_read(&Err("gh api x failed: HTTP 502".into())),
        FileAt::Unknown(_)
    ));
}
