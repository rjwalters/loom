//! Tests for the closed-issue `loom:building` cleanup decision (#6199).
//!
//! The T-numbered cases mirror `test-merge-pr-closed-issue-cleanup.sh`'s own
//! per-issue assertions (T1-T5), which still run against this code through the
//! shell wrapper. They are duplicated here so a Rust-only change that breaks
//! them fails without a shell suite in the loop.

use super::*;

fn plan_for(body: &str) -> Plan {
    plan(&IssueView::from_json(body))
}

// --- the retained suite's cases --------------------------------------------

#[test]
fn t1_closed_and_building_strips() {
    assert_eq!(
        plan_for(r#"{"state":"closed","labels":[{"name":"loom:building"}]}"#),
        Plan::Strip
    );
}

#[test]
fn t2_an_open_issue_keeps_its_live_claim() {
    assert_eq!(
        plan_for(r#"{"state":"open","labels":[{"name":"loom:building"}]}"#),
        Plan::Skip(Skip::NotClosed)
    );
}

#[test]
fn t3_closed_without_the_label_is_idempotent() {
    assert_eq!(
        plan_for(r#"{"state":"closed","labels":[{"name":"loom:issue"}]}"#),
        Plan::Skip(Skip::NotBuilding)
    );
}

#[test]
fn t4_a_pr_served_by_the_issues_endpoint_is_never_mutated() {
    // A merged PR is BOTH `closed` and, in principle, `loom:building`-labelled.
    // The `has("pull_request")` test must come first or this strips a PR.
    assert_eq!(
        plan_for(
            r#"{"state":"closed","pull_request":{"url":"x"},"labels":[{"name":"loom:building"}]}"#
        ),
        Plan::Skip(Skip::IsPullRequest)
    );
}

#[test]
fn t5_other_labels_do_not_change_the_decision() {
    // The shell names `loom:building` explicitly in `--remove-label`, so the
    // decision is all this side owns: it must still be Strip.
    assert_eq!(
        plan_for(
            r#"{"state":"closed","labels":[{"name":"loom:building"},{"name":"tier:maintenance"}]}"#
        ),
        Plan::Strip
    );
}

// --- the degenerate reads the shell suite cannot reach ---------------------

#[test]
fn a_failed_read_is_not_closed_so_nothing_is_stripped() {
    // `gh api` prints its error body to stdout and exits non-zero, so the
    // wrapper's `|| echo '{}'` leaves TWO documents. Neither carries a
    // `state`, and the decision must be a skip, not a strip.
    for body in [
        "{}",
        "",
        "\n\n",
        r#"{"message":"Not Found","documentation_url":"https://docs.github.com","status":"404"}{}"#,
        "not json at all",
        "null",
    ] {
        assert_eq!(plan_for(body), Plan::Skip(Skip::NotClosed), "body: {body:?}");
    }
}

#[test]
fn state_is_matched_exactly_and_case_sensitively() {
    for state in ["CLOSED", "Closed", "closed ", "open"] {
        let body = format!(r#"{{"state":"{state}","labels":[{{"name":"loom:building"}}]}}"#);
        assert_eq!(plan_for(&body), Plan::Skip(Skip::NotClosed), "state: {state:?}");
    }
}

#[test]
fn the_label_match_is_whole_line_and_case_sensitive() {
    // `grep -qx 'loom:building'` — a prefix, a suffix and a case variant all
    // fail to match, so the issue keeps whatever it has.
    assert_eq!(
        plan_for(
            r#"{"state":"closed","labels":[{"name":"loom:building-x"},{"name":"Loom:Building"},
              {"name":"x-loom:building"}]}"#
        ),
        Plan::Skip(Skip::NotBuilding)
    );
}

#[test]
fn pull_request_null_still_counts_as_a_pr() {
    // jq's `has` answers key PRESENCE, so `"pull_request": null` is `true` —
    // a reading that "deserialize it as an issue" would get backwards.
    assert_eq!(
        plan_for(r#"{"pull_request":null,"state":"closed","labels":[{"name":"loom:building"}]}"#),
        Plan::Skip(Skip::IsPullRequest)
    );
}

// --- the wire protocol ------------------------------------------------------

#[test]
fn render_is_always_exactly_one_token_led_line() {
    for (plan, want) in [
        (Plan::Strip, "STRIP\n"),
        (Plan::Skip(Skip::IsPullRequest), "SKIP\tis-pull-request\n"),
        (Plan::Skip(Skip::NotClosed), "SKIP\tnot-closed\n"),
        (Plan::Skip(Skip::NotBuilding), "SKIP\tnot-building\n"),
    ] {
        let rendered = render(plan);
        assert_eq!(rendered, want);
        assert_eq!(rendered.lines().count(), 1);
    }
}
