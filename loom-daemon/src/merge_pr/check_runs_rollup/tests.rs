//! Unit tests for the check-runs rollup parse. The `jq`-idiom cases below are
//! the ones a hand-rolled port gets wrong, and every one of them is also in
//! `tests/merge_pr_check_runs_rollup_differential.rs`'s shared corpus, run
//! against the frozen shell.

use super::*;

fn p(raw: &str) -> Rollup {
    parse(raw)
}

// --- the ordinary shapes -----------------------------------------------------

#[test]
fn all_green_rollup_has_nothing_failing_or_pending() {
    let r = p(r#"{"total_count":1,"check_runs":[
        {"name":"Required Build","status":"completed","conclusion":"success"}]}"#);
    assert_eq!(r.total_count, "1");
    assert!(r.failing.is_empty());
    assert!(r.pending.is_empty());
}

#[test]
fn in_progress_check_is_pending_and_not_failing() {
    let r = p(r#"{"total_count":2,"check_runs":[
        {"name":"Required Build","status":"in_progress","conclusion":null},
        {"name":"Lint","status":"completed","conclusion":"success"}]}"#);
    assert_eq!(r.failing, Vec::<String>::new());
    assert_eq!(r.pending, vec!["Required Build"]);
}

#[test]
fn mixed_rollup_populates_both_sets() {
    let r = p(r#"{"total_count":2,"check_runs":[
        {"name":"Flaky Job","status":"completed","conclusion":"failure"},
        {"name":"Required Build","status":"in_progress","conclusion":null}]}"#);
    assert_eq!(r.failing, vec!["Flaky Job"]);
    assert_eq!(r.pending, vec!["Required Build"]);
}

// --- which conclusions are TERMINAL-failing ---------------------------------

#[test]
fn every_terminal_failing_conclusion_is_recognized() {
    for c in ["failure", "timed_out", "cancelled", "action_required"] {
        let raw = format!(
            r#"{{"total_count":1,"check_runs":[{{"name":"X","status":"completed","conclusion":"{c}"}}]}}"#
        );
        assert_eq!(p(&raw).failing, vec!["X"], "conclusion {c:?} must be failing");
    }
}

#[test]
fn completed_but_not_failing_conclusions_never_refuse_a_merge() {
    // The whole set GitHub can return that is NOT in the terminal-failing
    // four. Counting any of these as failing would refuse merges that the
    // retired filter passed — `skipped` in particular is what every
    // path-filtered required check reports.
    for c in [
        "success",
        "skipped",
        "neutral",
        "stale",
        "startup_failure",
        "null",
    ] {
        let conclusion = if c == "null" {
            "null".to_string()
        } else {
            format!("\"{c}\"")
        };
        let raw = format!(
            r#"{{"total_count":1,"check_runs":[{{"name":"X","status":"completed","conclusion":{conclusion}}}]}}"#
        );
        assert!(p(&raw).failing.is_empty(), "conclusion {c:?} must NOT be failing");
    }
}

#[test]
fn failing_match_is_exact_never_substring_or_case_folded() {
    for c in [
        "FAILURE",
        "Failure",
        "failures",
        "pre-failure",
        "timedout",
        "time_out",
    ] {
        let raw = format!(
            r#"{{"total_count":1,"check_runs":[{{"name":"X","status":"completed","conclusion":"{c}"}}]}}"#
        );
        assert!(p(&raw).failing.is_empty(), "conclusion {c:?} must NOT be failing");
    }
}

// --- what counts as PENDING (a denylist of one value) -----------------------

#[test]
fn any_status_other_than_completed_is_pending() {
    for s in [
        "queued",
        "in_progress",
        "waiting",
        "pending",
        "requested",
        "COMPLETED",
        "completed ",
    ] {
        let raw = format!(
            r#"{{"total_count":1,"check_runs":[{{"name":"X","status":"{s}","conclusion":null}}]}}"#
        );
        assert_eq!(p(&raw).pending, vec!["X"], "status {s:?} must be pending");
    }
}

#[test]
fn a_check_run_with_no_status_is_pending() {
    // `null != "completed"` is true in jq. Dropping the row would shrink the
    // pending set, which is the one direction that ends a wait early.
    let r = p(r#"{"total_count":1,"check_runs":[{"name":"X","conclusion":null}]}"#);
    assert_eq!(r.pending, vec!["X"]);
}

#[test]
fn a_non_string_status_is_pending_not_an_error() {
    let r = p(r#"{"total_count":1,"check_runs":[{"name":"X","status":7}]}"#);
    assert_eq!(r.pending, vec!["X"]);
}

// --- `unique`: sorts AND de-duplicates --------------------------------------

#[test]
fn unique_sorts_by_codepoint_and_dedupes() {
    let r = p(r#"{"total_count":4,"check_runs":[
        {"name":"zeta","status":"queued"},
        {"name":"Alpha","status":"queued"},
        {"name":"zeta","status":"in_progress"},
        {"name":"beta","status":"queued"}]}"#);
    // Uppercase sorts before lowercase under codepoint order, and the
    // duplicate `zeta` collapses even though the two rows differ.
    assert_eq!(r.pending, vec!["Alpha", "beta", "zeta"]);
}

#[test]
fn a_missing_name_renders_as_the_literal_null() {
    // `jq -r` unquotes STRINGS only; null prints as `null`, and jq's total
    // order puts null before every string.
    let r = p(r#"{"total_count":2,"check_runs":[
        {"name":"Lint","status":"queued"},
        {"status":"queued"}]}"#);
    assert_eq!(r.pending, vec!["null", "Lint"]);
}

#[test]
fn non_string_names_render_as_compact_json_in_jq_type_order() {
    let r = p(r#"{"total_count":4,"check_runs":[
        {"name":"CI","status":"queued"},
        {"name":7,"status":"queued"},
        {"name":true,"status":"queued"},
        {"name":null,"status":"queued"}]}"#);
    assert_eq!(r.pending, vec!["null", "true", "7", "CI"]);
}

#[test]
fn an_empty_name_is_a_name_and_sorts_first() {
    let r = p(r#"{"total_count":2,"check_runs":[
        {"name":"CI","status":"queued"},
        {"name":"","status":"queued"}]}"#);
    assert_eq!(r.pending, vec!["", "CI"]);
    // …and the joined string the shell's `-n` test measured still LOOKS
    // non-empty here only because a second name follows it.
    assert_eq!(Rollup::joined(&r.pending), "\nCI");
}

#[test]
fn a_lone_empty_name_joins_to_the_empty_string_the_shell_read_as_nothing() {
    // The one case where "the list is non-empty" and "the shell saw
    // something" disagree: `printf` of a single empty line, stripped by
    // command substitution, made `[[ -n "$pending" ]]` false.
    let r = p(r#"{"total_count":1,"check_runs":[{"name":"","status":"queued"}]}"#);
    assert_eq!(r.pending.len(), 1);
    assert_eq!(Rollup::joined(&r.pending), "");
    assert_eq!(Rollup::line_count(&r.pending), 1);
}

#[test]
fn line_count_matches_printf_piped_to_wc_l() {
    assert_eq!(Rollup::line_count(&[]), 1);
    assert_eq!(Rollup::line_count(&["A".to_string()]), 1);
    assert_eq!(Rollup::line_count(&["A".to_string(), "B".to_string()]), 2);
}

// --- the shapes `jq` could not walk (silent empties, fail-CLOSED) -----------

#[test]
fn null_check_runs_yields_the_all_empty_answer() {
    let r = p(r#"{"total_count":3,"check_runs":null}"#);
    assert!(r.failing.is_empty());
    assert!(r.pending.is_empty());
    // `.total_count` itself was still readable — only the two iterations
    // errored, and they errored independently of it.
    assert_eq!(r.total_count, "3");
}

#[test]
fn missing_check_runs_yields_the_all_empty_answer() {
    let r = p(r#"{"total_count":0}"#);
    assert!(r.failing.is_empty());
    assert!(r.pending.is_empty());
    assert_eq!(r.total_count, "0");
}

#[test]
fn a_non_object_row_aborts_the_whole_filter_not_just_that_row() {
    // `jq`'s `.name` on a number is a fatal type error, so the names already
    // collected are lost with it. A port that skipped the bad row would
    // report a SMALLER pending set than the shell did — and a smaller
    // pending set can end the wait early.
    let r = p(r#"{"total_count":2,"check_runs":[{"name":"Lint","status":"queued"},7]}"#);
    assert!(r.pending.is_empty());
    assert!(r.failing.is_empty());
}

#[test]
fn a_string_row_also_aborts_the_filter() {
    let r = p(r#"{"total_count":1,"check_runs":["Lint"]}"#);
    assert!(r.pending.is_empty());
}

#[test]
fn a_null_row_is_indexable_and_contributes_a_null_name() {
    // `null | .status` is `null` in jq (not an error), `null != "completed"`,
    // and `null | .name` is `null`.
    let r = p(r#"{"total_count":1,"check_runs":[null]}"#);
    assert_eq!(r.pending, vec!["null"]);
    assert!(r.failing.is_empty());
}

#[test]
fn check_runs_as_an_object_iterates_its_values() {
    // `jq`'s `.[]` iterates object VALUES, so this was never empty.
    let r = p(r#"{"total_count":1,"check_runs":{"a":{"name":"CI","status":"queued"}}}"#);
    assert_eq!(r.pending, vec!["CI"]);
}

#[test]
fn malformed_and_empty_input_are_the_all_empty_answer() {
    for raw in ["", "   ", "not json", "{\"check_runs\":[", "{} {}"] {
        let r = p(raw);
        assert_eq!(r.total_count, "0", "input {raw:?}");
        assert!(r.failing.is_empty(), "input {raw:?}");
        assert!(r.pending.is_empty(), "input {raw:?}");
    }
}

#[test]
fn a_top_level_array_is_the_all_empty_answer() {
    let r = p(r#"[{"name":"CI","status":"queued"}]"#);
    assert_eq!(r.total_count, "0");
    assert!(r.pending.is_empty());
}

// --- `.total_count // 0` plus bash's `^[0-9]+$` gate ------------------------

#[test]
fn total_count_alternative_fires_on_null_and_false_only() {
    assert_eq!(p(r#"{"total_count":null,"check_runs":[]}"#).total_count, "0");
    assert_eq!(p(r#"{"total_count":false,"check_runs":[]}"#).total_count, "0");
    // A literal 0 is truthy to jq's `//`, so it survives as itself — the same
    // "0" either way, which is exactly why the distinction never showed up in
    // a bug report and must not be "simplified" into an is-zero test.
    assert_eq!(p(r#"{"total_count":0,"check_runs":[]}"#).total_count, "0");
    assert_eq!(p(r#"{"check_runs":[]}"#).total_count, "0");
    assert_eq!(p(r#"{"total_count":true,"check_runs":[]}"#).total_count, "0");
}

#[test]
fn total_count_as_a_json_string_of_digits_passes_the_gate() {
    // `jq -r` already unquoted it, so bash's regex saw `7`.
    assert_eq!(p(r#"{"total_count":"7","check_runs":[]}"#).total_count, "7");
}

#[test]
fn total_count_rejected_by_the_bash_regex_becomes_zero() {
    for v in [
        "7.0",
        "-1",
        "1e3",
        "\"abc\"",
        "\"\"",
        "\" 7\"",
        "\"7 \"",
        "[]",
        "{}",
        "\"7\\n8\"",
    ] {
        let raw = format!(r#"{{"total_count":{v},"check_runs":[]}}"#);
        assert_eq!(p(&raw).total_count, "0", "total_count {v} must be gated to 0");
    }
}

#[test]
fn total_count_is_carried_as_digits_not_reparsed_into_a_narrower_type() {
    // The shell passed the digit string straight into `[[ -gt ]]`, so the
    // port keeps it as digits rather than round-tripping it through an
    // integer type that might not hold it.
    assert_eq!(
        p(r#"{"total_count":18446744073709551615,"check_runs":[]}"#).total_count,
        "18446744073709551615"
    );
    // Past `u64::MAX` the documented bound takes over: exponent form, which
    // the `^[0-9]+$` gate rejects, so 0 — the direction that keeps #6169's
    // zero-row guard engaged. See the module docs for why the literal digits
    // are not carried for a field that counts check runs on one commit.
    assert_eq!(
        p(r#"{"total_count":123456789012345678901234567890,"check_runs":[]}"#).total_count,
        "0"
    );
}

#[test]
fn total_count_is_independent_of_the_name_lists() {
    // Each retired filter had its OWN `|| true`, so one erroring never
    // silenced the others.
    let r = p(r#"{"total_count":5,"check_runs":[7]}"#);
    assert_eq!(r.total_count, "5");
    assert!(r.failing.is_empty());
    assert!(r.pending.is_empty());
}
