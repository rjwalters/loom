//! Tests for the partial-increment reset decision.
//!
//! The T-numbered cases mirror `test-merge-pr-partial-increment.sh`'s own
//! reset assertions (T1/T3/T4/T7/T18/T19/T20/T27), which still run against
//! this code through the shell wrapper. They are duplicated here so a
//! Rust-only change that breaks them fails without a shell suite in the loop.

use super::*;

const OPEN_BUILDING: &str = r#"{"state":"open","labels":[{"name":"loom:building"}]}"#;
const CLOSED_BUILDING: &str = r#"{"state":"closed","labels":[{"name":"loom:building"}]}"#;

fn plan_for(body: &str, pre: PreMerge) -> Vec<Step> {
    plan("123", "999", "owner/repo", &IssueView::from_json(body), pre)
}

fn actions(steps: &[Step]) -> Vec<&'static str> {
    steps
        .iter()
        .map(|s| match s {
            Step::Info(_) => "info",
            Step::Warning(_) => "warning",
            Step::Reopen => "reopen",
            Step::Swap => "swap",
        })
        .collect()
}

// --- the retained suite's cases --------------------------------------------

#[test]
fn t1_open_and_building_swaps() {
    let steps = plan_for(OPEN_BUILDING, PreMerge::default());
    assert_eq!(actions(&steps), ["info", "swap"]);
    assert_eq!(
        steps[0],
        Step::Info(
            "Partial-increment reset: PR #999 merged as a partial slice of #123; returning it to \
the ready queue"
                .into()
        )
    );
}

#[test]
fn t3_t20_closed_and_untracked_is_a_logged_skip() {
    let steps = plan_for(CLOSED_BUILDING, PreMerge::default());
    assert_eq!(
        steps,
        [Step::Info(
            "Partial-increment reset: issue #123 is not open (state='closed') — skipping".into()
        )]
    );
}

#[test]
fn t4_open_without_building_is_an_idempotent_skip() {
    let steps =
        plan_for(r#"{"state":"open","labels":[{"name":"loom:issue"}]}"#, PreMerge::default());
    assert_eq!(
        steps,
        [Step::Info(
            "Partial-increment reset: issue #123 is not loom:building — skipping (idempotent)"
                .into()
        )]
    );
}

#[test]
fn t7_a_pull_request_is_skipped_silently() {
    let body = r#"{"state":"open","pull_request":{"url":"x"},"labels":[{"name":"loom:building"}]}"#;
    assert!(plan_for(
        body,
        PreMerge {
            conflicted: true,
            open_before_merge: true
        }
    )
    .is_empty());
}

#[test]
fn t18_t27_conflicted_and_closed_reopens_then_swaps() {
    let pre = PreMerge {
        conflicted: true,
        open_before_merge: true,
    };
    let steps = plan_for(CLOSED_BUILDING, pre);
    assert_eq!(actions(&steps), ["warning", "reopen", "info", "swap"]);
    let Step::Warning(w) = &steps[0] else {
        unreachable!()
    };
    assert!(w.contains("was auto-closed by PR #999's merge"));
    assert!(w.contains("— reopening (#4569)"));
}

#[test]
fn a_reopened_issue_without_building_reopens_then_skips() {
    let pre = PreMerge {
        conflicted: true,
        open_before_merge: false,
    };
    let steps = plan_for(r#"{"state":"closed","labels":[]}"#, pre);
    assert_eq!(actions(&steps), ["warning", "reopen", "info"]);
}

#[test]
fn t19_open_before_but_unattributed_warns_and_does_not_reopen() {
    let pre = PreMerge {
        conflicted: false,
        open_before_merge: true,
    };
    let steps = plan_for(CLOSED_BUILDING, pre);
    assert_eq!(actions(&steps), ["warning"]);
    let Step::Warning(w) = &steps[0] else {
        unreachable!()
    };
    assert!(w.contains("NOT reopening automatically"));
    assert!(w.contains("(state='closed')"));
    assert!(w.ends_with("gh issue reopen 123 --repo owner/repo"));
}

#[test]
fn conflicted_outranks_open_before_merge() {
    // Both facts true is the normal #4569 shape; the reopen arm wins.
    let pre = PreMerge {
        conflicted: true,
        open_before_merge: true,
    };
    assert_eq!(actions(&plan_for(CLOSED_BUILDING, pre))[1], "reopen");
}

#[test]
fn pre_merge_facts_are_ignored_for_an_open_issue() {
    let pre = PreMerge {
        conflicted: true,
        open_before_merge: true,
    };
    assert_eq!(actions(&plan_for(OPEN_BUILDING, pre)), ["info", "swap"]);
}

// --- the jq model ----------------------------------------------------------

#[test]
fn a_failed_read_reads_as_an_empty_unknown_state() {
    for body in ["{}", "", "   \n", "not json", r#"{"message":"Not Found"}"#] {
        let view = IssueView::from_json(body);
        assert_eq!(
            view,
            IssueView {
                is_pr: false,
                state: String::new(),
                building: false
            },
            "{body:?}"
        );
        let steps = plan("1", "2", "o/r", &view, PreMerge::default());
        assert_eq!(
            steps,
            [Step::Info(
                "Partial-increment reset: issue #1 is not open (state='unknown') — skipping".into()
            )],
            "{body:?}"
        );
    }
}

#[test]
fn pull_request_null_still_counts_as_a_pr() {
    // `has` tests key presence, not truthiness.
    assert!(IssueView::from_json(r#"{"pull_request":null}"#).is_pr);
}

#[test]
fn state_null_and_false_become_empty() {
    assert_eq!(IssueView::from_json(r#"{"state":null}"#).state, "");
    assert_eq!(IssueView::from_json(r#"{"state":false}"#).state, "");
}

#[test]
fn an_error_body_followed_by_the_fallback_is_two_documents() {
    // `gh api` prints the error body on stdout and exits 1, so the shell's
    // `|| echo '{}'` APPENDS a document. Two `has` outputs are never exactly
    // `true`, and two state lines are never exactly `open`.
    let view = IssueView::from_json("{\"message\":\"Not Found\"}\n{}");
    assert!(!view.is_pr);
    assert_eq!(view.state, "");
    let twice_open = IssueView::from_json(r#"{"state":"open"}{"state":"open"}"#);
    assert_eq!(twice_open.state, "open\nopen");
    assert_ne!(twice_open.state, "open");
}

#[test]
fn a_non_object_document_contributes_nothing_but_does_not_stop_the_stream() {
    let view = IssueView::from_json(r#"[1]{"pull_request":1,"state":"open"}"#);
    assert!(view.is_pr);
    assert_eq!(view.state, "open");
}

#[test]
fn a_parse_error_ends_the_stream() {
    let view = IssueView::from_json(r#"{"state":"closed"} garbage {"state":"open"}"#);
    assert_eq!(view.state, "closed");
}

#[test]
fn a_non_object_label_ends_that_documents_label_output() {
    assert!(IssueView::from_json(r#"{"labels":[{"name":"loom:building"},"x"]}"#).building);
    assert!(!IssueView::from_json(r#"{"labels":["x",{"name":"loom:building"}]}"#).building);
    assert!(IssueView::from_json(r#"{"labels":[null,{"name":"loom:building"}]}"#).building);
}

#[test]
fn an_object_labels_iterates_its_values() {
    assert!(IssueView::from_json(r#"{"labels":{"k":{"name":"loom:building"}}}"#).building);
}

#[test]
fn building_is_an_exact_line_match() {
    assert!(!IssueView::from_json(r#"{"labels":[{"name":"loom:building-x"}]}"#).building);
    assert!(!IssueView::from_json(r#"{"labels":[{"name":"Loom:Building"}]}"#).building);
    // grep -qx over the captured lines: an embedded newline splits a name.
    assert!(IssueView::from_json(r#"{"labels":[{"name":"a\nloom:building"}]}"#).building);
}

#[test]
fn state_is_compared_verbatim() {
    for s in ["OPEN", "Open", " open", "open "] {
        let body = format!(r#"{{"state":"{s}","labels":[{{"name":"loom:building"}}]}}"#);
        assert_eq!(actions(&plan_for(&body, PreMerge::default())), ["info"], "{s:?}");
    }
}

// --- the line protocol -----------------------------------------------------

#[test]
fn render_emits_one_record_per_step() {
    let steps = [
        Step::Warning("w".into()),
        Step::Reopen,
        Step::Info("i".into()),
        Step::Swap,
    ];
    assert_eq!(render(&steps), "WARNING\tw\nREOPEN\nINFO\ti\nSWAP\n");
}

#[test]
fn render_gives_every_continuation_line_its_level() {
    let out = render(&[Step::Info("a\nREOPEN\nb".into())]);
    assert_eq!(out, "INFO\ta\nINFO\tREOPEN\nINFO\tb\n");
    assert!(
        !out.lines().any(|l| l == "REOPEN"),
        "a message line must never read as an action"
    );
}

#[test]
fn render_of_nothing_is_empty() {
    assert_eq!(render(&[]), "");
}
