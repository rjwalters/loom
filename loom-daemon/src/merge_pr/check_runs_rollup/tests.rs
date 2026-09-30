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
