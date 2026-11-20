//! Tests for the check-runs rollup read.

use super::*;

fn ok(raw: &str) -> (String, String, String) {
    let r = classify(raw).unwrap_or_else(|e| panic!("refused {raw:?}: {e}"));
    (r.failing, r.pending, r.total_count)
}

fn t(f: &str, p: &str, n: &str) -> (String, String, String) {
    (f.to_string(), p.to_string(), n.to_string())
}

#[test]
fn healthy_rollup_splits_failing_pending_and_count() {
    let raw = r#"{"total_count":4,"check_runs":[
        {"name":"Lint","status":"completed","conclusion":"failure"},
        {"name":"Build","status":"in_progress","conclusion":null},
        {"name":"Docs","status":"completed","conclusion":"success"},
        {"name":"Deploy","status":"queued","conclusion":null}]}"#;
    assert_eq!(ok(raw), t("Lint", "Build\nDeploy", "4"));
}

#[test]
fn every_terminal_failure_conclusion_counts_and_nothing_else_does() {
    for c in ["failure", "timed_out", "cancelled", "action_required"] {
        let raw = format!(
            r#"{{"total_count":1,"check_runs":[{{"name":"X","status":"completed","conclusion":"{c}"}}]}}"#
        );
        assert_eq!(ok(&raw), t("X", "", "1"), "{c}");
    }
    for c in [
        "success", "neutral", "skipped", "stale", "Failure", "failure ",
    ] {
        let raw = format!(
            r#"{{"total_count":1,"check_runs":[{{"name":"X","status":"completed","conclusion":"{c}"}}]}}"#
        );
        assert_eq!(ok(&raw), t("", "", "1"), "{c}");
    }
}

#[test]
fn missing_or_non_string_status_is_pending_like_jq_inequality() {
    let raw = r#"{"total_count":3,"check_runs":[
        {"name":"A"},{"name":"B","status":null},{"name":"C","status":7}]}"#;
    assert_eq!(ok(raw), t("", "A\nB\nC", "3"));
}

#[test]
fn a_failed_but_still_running_check_is_in_both_sets() {
    let raw = r#"{"total_count":1,"check_runs":[{"name":"X","status":"in_progress","conclusion":"failure"}]}"#;
    assert_eq!(ok(raw), t("X", "X", "1"));
}

#[test]
fn names_are_sorted_bytewise_and_deduplicated() {
    let raw = r#"{"total_count":5,"check_runs":[
        {"name":"b","status":"queued"},{"name":"B","status":"queued"},
        {"name":"a","status":"queued"},{"name":"b","status":"queued"},
        {"name":"é","status":"queued"}]}"#;
    assert_eq!(ok(raw), t("", "B\na\nb\né", "5"));
}

#[test]
fn null_name_sorts_first_and_stays_distinct_from_the_string_null() {
    let raw = r#"{"total_count":3,"check_runs":[
        {"name":"null","status":"queued"},{"status":"queued"},{"name":"A","status":"queued"}]}"#;
    assert_eq!(ok(raw), t("", "null\nA\nnull", "3"));
}

#[test]
fn embedded_newline_in_a_name_is_printed_raw() {
    let raw = r#"{"total_count":1,"check_runs":[{"name":"a\nb","status":"queued"}]}"#;
    assert_eq!(ok(raw), t("", "a\nb", "1"));
}

#[test]
fn trailing_newlines_are_stripped_like_command_substitution() {
    let raw = r#"{"total_count":2,"check_runs":[{"name":"z\n\n","status":"queued"},{"name":"a","status":"queued"}]}"#;
    assert_eq!(ok(raw), t("", "a\nz", "2"));
}

#[test]
fn empty_pending_name_is_reproduced_not_fixed() {
    // The retired quirk (module docs): `$(...)` strips the lone empty line,
    // so the loop reads "nothing pending". Pinned so a fix is deliberate.
    let raw = r#"{"total_count":1,"check_runs":[{"name":"","status":"in_progress"}]}"#;
    assert_eq!(ok(raw), t("", "", "1"));
    // …but it is only empty when it is the ONLY line.
    let raw = r#"{"total_count":2,"check_runs":[{"name":"","status":"queued"},{"name":"A","status":"queued"}]}"#;
    assert_eq!(ok(raw), t("", "\nA", "2"));
}

#[test]
fn empty_rollup_is_answered_not_refused() {
    assert_eq!(ok(r#"{"total_count":0,"check_runs":[]}"#), t("", "", "0"));
}

#[test]
fn extra_keys_and_surrounding_whitespace_are_fine() {
    let raw = " \n{\"total_count\":1,\"x\":[1],\"check_runs\":[{\"name\":\"A\",\"status\":\"queued\",\"html_url\":null}]}\n";
    assert_eq!(ok(raw), t("", "A", "1"));
}

#[test]
fn duplicate_keys_take_the_last_value_like_jq() {
    let raw = r#"{"total_count":1,"total_count":2,"check_runs":[{"name":"A","name":"B","status":"queued"}]}"#;
    assert_eq!(ok(raw), t("", "B", "2"));
}

#[test]
fn unreadable_payloads_are_refused_never_read_as_settled() {
    let cases: &[&str] = &[
        "",
        "   ",
        "not json",
        r#"{"total_count":1,"check_runs":[]} trailing"#,
        r#"{"total_count":1,"check_runs":[]}{"total_count":1,"check_runs":[]}"#,
        "null",
        "[]",
        r#""x""#,
        r#"{"check_runs":[]}"#,
        r#"{"total_count":1}"#,
        r#"{"total_count":1,"check_runs":null}"#,
        r#"{"total_count":1,"check_runs":{"a":{"name":"A"}}}"#,
        r#"{"total_count":1,"check_runs":[null]}"#,
        r#"{"total_count":1,"check_runs":["A"]}"#,
        r#"{"total_count":1,"check_runs":[{"name":7,"status":"queued"}]}"#,
        r#"{"total_count":1,"check_runs":[{"name":["A"],"status":"queued"}]}"#,
        r#"{"total_count":1,"check_runs":[{"name":"a\u0000b","status":"queued"}]}"#,
    ];
    for raw in cases {
        assert!(classify(raw).is_err(), "should refuse {raw:?}");
    }
}

#[test]
fn a_bad_name_is_refused_even_on_a_check_neither_failing_nor_pending() {
    // Stricter than the retired filter (which never rendered it): a name
    // outside the forge contract means the payload is not what we think.
    let raw = r#"{"total_count":1,"check_runs":[{"name":7,"status":"completed","conclusion":"success"}]}"#;
    assert_eq!(classify(raw), Err(Refusal::BadName(0)));
}

#[test]
fn total_count_must_be_an_unsigned_integer_literal() {
    for bad in [
        "-1",
        "2.5",
        "5.0",
        "1e2",
        "\"5\"",
        "true",
        "null",
        "[1]",
        "18446744073709551616",
    ] {
        let raw = format!(r#"{{"total_count":{bad},"check_runs":[]}}"#);
        assert_eq!(classify(&raw), Err(Refusal::BadTotalCount), "{bad}");
    }
    let raw = r#"{"total_count":18446744073709551615,"check_runs":[]}"#;
    assert_eq!(ok(raw).2, "18446744073709551615");
}

#[test]
fn stuck_in_progress_with_conclusion_and_completed_at_is_settled() {
    let raw = r#"{"total_count":3,"check_runs":[
        {"name":"Stuck","status":"in_progress","conclusion":"success","completed_at":"2026-11-10T03:25:28Z"},
        {"name":"StuckFail","status":"in_progress","conclusion":"failure","completed_at":"2026-11-10T03:25:28Z"},
        {"name":"Done","status":"completed","conclusion":"success","completed_at":"2026-11-10T03:25:28Z"}]}"#;
    assert_eq!(ok(raw), t("StuckFail", "", "3"));
}

#[test]
fn half_written_rows_stay_pending() {
    let raw = r#"{"total_count":4,"check_runs":[
        {"name":"NoCompletedAt","status":"in_progress","conclusion":"success","completed_at":null},
        {"name":"NoConclusion","status":"in_progress","conclusion":null,"completed_at":"2026-11-10T03:25:28Z"},
        {"name":"NumConclusion","status":"in_progress","conclusion":1,"completed_at":"2026-11-10T03:25:28Z"},
        {"name":"NoStatus","conclusion":null}]}"#;
    assert_eq!(ok(raw), t("", "NoCompletedAt\nNoConclusion\nNoStatus\nNumConclusion", "4"));
}

// ---------------------------------------------------------------------------
// #14415: stranded check runs under a terminal-failed workflow run
// ---------------------------------------------------------------------------

mod stranded {
    use super::*;
    use serde_json::json;

    const SHA: &str = "5f72ee5c0926e69e278a65d5a36b1a561198591b";

    /// A workflow-runs row, the shape `actions/runs?head_sha=` returns.
    fn wrun(id: u64, suite: u64, status: &str, conclusion: Option<&str>, at: &str) -> Value {
        json!({"id": id, "workflow_id": 7, "check_suite_id": suite, "event": "push",
               "head_sha": SHA, "created_at": at, "status": status,
               "conclusion": conclusion, "pull_requests": []})
    }

    fn runs_payload(rows: Vec<Value>) -> Value {
        json!({"total_count": rows.len(), "workflow_runs": rows})
    }

    /// A check-run row in `suite`.
    fn row(name: &str, suite: u64, status: &str, conclusion: Option<&str>) -> Value {
        json!({"name": name, "status": status, "conclusion": conclusion,
               "check_suite": {"id": suite}})
    }

    fn doc(rows: Vec<Value>) -> Value {
        json!({"total_count": rows.len(), "check_runs": rows})
    }

    fn rollup(doc: &Value) -> Rollup {
        classify(&doc.to_string()).unwrap()
    }

    #[test]
    fn a_stranded_row_under_a_failed_run_reads_failing_not_pending() {
        let mut d = doc(vec![
            row("build", 9, "queued", None),
            row("lint", 9, "queued", None),
            row("docs", 4, "completed", Some("success")),
        ]);
        // Before: both stranded rows hold the rollup pending.
        assert_eq!(rollup(&d).pending, "build\nlint");
        let payload = runs_payload(vec![wrun(
            1,
            9,
            "completed",
            Some("startup_failure"),
            "2026-08-26T15:36:37Z",
        )]);
        assert_eq!(fail_stranded_rows(&mut d, &payload, SHA), 2);
        let r = rollup(&d);
        assert_eq!(r.pending, "");
        // `startup_failure` is not in the retired failing filter's conclusion
        // list, so assert on the rewritten row itself and on settledness.
        assert_eq!(d["check_runs"][0]["status"], "completed");
        assert_eq!(d["check_runs"][0]["conclusion"], "startup_failure");
        assert!(is_settled(&d["check_runs"][0]));
    }

    #[test]
    fn a_failed_run_with_a_listed_failing_conclusion_reads_failing() {
        let mut d = doc(vec![row("build", 9, "queued", None)]);
        let payload = runs_payload(vec![wrun(
            1,
            9,
            "completed",
            Some("failure"),
            "2026-11-18T10:00:00Z",
        )]);
        assert_eq!(fail_stranded_rows(&mut d, &payload, SHA), 1);
        let r = rollup(&d);
        assert_eq!((r.failing.as_str(), r.pending.as_str()), ("build", ""));
    }

    #[test]
    fn a_run_that_is_rerunning_or_in_progress_strands_nothing() {
        for (status, conclusion) in [
            ("in_progress", None),
            ("queued", None),
            ("completed", Some("success")),
        ] {
            let mut d = doc(vec![row("build", 9, "queued", None)]);
            let payload =
                runs_payload(vec![wrun(1, 9, status, conclusion, "2026-11-18T10:00:00Z")]);
            assert_eq!(fail_stranded_rows(&mut d, &payload, SHA), 0, "{status}/{conclusion:?}");
            assert_eq!(rollup(&d).pending, "build");
        }
    }

    #[test]
    fn a_superseded_twin_is_not_attributed() {
        // An older failed run and a newer green run of the same workflow: the
        // older verdict was replaced, so its stranded rows are not a failure.
        let mut d = doc(vec![row("build", 9, "queued", None)]);
        let payload = runs_payload(vec![
            wrun(1, 9, "completed", Some("startup_failure"), "2026-11-18T10:00:00Z"),
            wrun(2, 10, "completed", Some("success"), "2026-11-18T11:00:00Z"),
        ]);
        assert_eq!(fail_stranded_rows(&mut d, &payload, SHA), 0);
        assert_eq!(rollup(&d).pending, "build");
    }

    #[test]
    fn an_unreadable_or_truncated_runs_payload_leaves_rows_pending() {
        let failed = wrun(1, 9, "completed", Some("startup_failure"), "2026-11-18T10:00:00Z");
        let truncated = json!({"total_count": 5, "workflow_runs": [failed.clone()]});
        for payload in [
            truncated,
            json!({}),
            json!({"workflow_runs": [failed]}),
            json!(null),
        ] {
            let mut d = doc(vec![row("build", 9, "queued", None)]);
            assert_eq!(fail_stranded_rows(&mut d, &payload, SHA), 0, "{payload}");
            assert_eq!(rollup(&d).pending, "build");
        }
    }

    #[test]
    fn a_row_without_a_suite_id_or_in_another_suite_stays_pending() {
        let mut d = doc(vec![
            json!({"name": "no-suite", "status": "queued", "conclusion": null}),
            row("other-suite", 4, "queued", None),
        ]);
        let payload = runs_payload(vec![wrun(
            1,
            9,
            "completed",
            Some("startup_failure"),
            "2026-11-18T10:00:00Z",
        )]);
        assert_eq!(fail_stranded_rows(&mut d, &payload, SHA), 0);
        assert_eq!(rollup(&d).pending, "no-suite\nother-suite");
        assert!(!is_stranded_candidate(&d["check_runs"][0]));
        assert!(is_stranded_candidate(&d["check_runs"][1]));
    }

    #[test]
    fn a_run_on_another_head_attributes_nothing() {
        let mut d = doc(vec![row("build", 9, "queued", None)]);
        let mut other = wrun(1, 9, "completed", Some("startup_failure"), "2026-11-18T10:00:00Z");
        other["head_sha"] = json!("0000000000000000000000000000000000000000");
        assert_eq!(fail_stranded_rows(&mut d, &runs_payload(vec![other]), SHA), 0);
    }
}
