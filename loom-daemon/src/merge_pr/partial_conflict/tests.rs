//! Tests for the pre-merge partial-increment close-conflict decision.
//!
//! The T-numbered cases mirror `test-merge-pr-partial-increment.sh`'s own
//! conflict-guard assertions (T11-T16, T17b, T21-T26), which still run against
//! this code through the shell wrapper. They are duplicated here so a
//! Rust-only change that breaks them fails without a shell suite in the loop.

use super::*;

const OPEN: &str = r#"{"state":"open","labels":[{"name":"loom:building"}]}"#;
const CLOSED: &str = r#"{"state":"closed","labels":[{"name":"loom:building"}]}"#;
const PR: &str = r#"{"state":"open","pull_request":{"url":"x"}}"#;

fn frame(body: &str, commits: &str, graphql: &str, issues: &[(&str, &str)]) -> Frame {
    Frame {
        body: body.into(),
        commit_messages: commits.into(),
        graphql_close_refs: graphql.into(),
        issues: issues
            .iter()
            .map(|(n, j)| ((*n).to_string(), (*j).to_string()))
            .collect(),
    }
}

fn sets(steps: &[Step]) -> (Vec<u64>, Vec<u64>) {
    let mut open = Vec::new();
    let mut conflict = Vec::new();
    for s in steps {
        match s {
            Step::Open(n) => open.push(*n),
            Step::Conflict(n) => conflict.push(*n),
            Step::Warning(_) => {}
        }
    }
    (open, conflict)
}

fn warnings(steps: &[Step]) -> String {
    steps
        .iter()
        .filter_map(|s| match s {
            Step::Warning(m) => Some(m.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// --- the retained suite's cases --------------------------------------------

#[test]
fn t11_incident_shape_body_keyword_is_a_conflict() {
    let f = frame(
        "## Operator follow-up (after merge)\n\n1. npm publish\n2. Verify, then close #123.\n\n\
Contributes to #123\n",
        "",
        "",
        &[("123", OPEN)],
    );
    let steps = plan(&f, "999", false);
    assert_eq!(sets(&steps), (vec![123], vec![123]));
    let w = warnings(&steps);
    assert!(w.contains("Partial-increment conflict (#4569)"), "{w}");
    assert!(w.contains("close #123"), "{w}");
    assert!(w.contains("its body ALSO carries"), "{w}");
}

#[test]
fn t12_clean_body_is_open_but_not_conflicted() {
    let f = frame("Implements a slice.\n\nContributes to #123", "", "", &[("123", OPEN)]);
    let steps = plan(&f, "999", false);
    assert_eq!(sets(&steps), (vec![123], vec![]));
    assert!(warnings(&steps).is_empty());
}

#[test]
fn t13_closing_a_different_issue_is_not_a_conflict() {
    let f = frame("Closes #888\n\nPart of #123", "", "", &[("123", OPEN)]);
    assert_eq!(sets(&plan(&f, "999", false)), (vec![123], vec![]));
}

#[test]
fn t14_already_closed_is_in_neither_set() {
    let f = frame("Part of #777\n\nclose #777", "", "", &[("777", CLOSED)]);
    assert!(plan(&f, "999", false).is_empty());
}

#[test]
fn t15_sidebar_only_link_is_a_conflict_attributed_to_the_sidebar() {
    let f = frame("Contributes to #123", "", "123", &[("123", OPEN)]);
    let steps = plan(&f, "999", false);
    assert_eq!(sets(&steps), (vec![123], vec![123]));
    assert!(warnings(&steps).contains("Development sidebar"));
}

#[test]
fn t16_a_pr_is_never_tracked() {
    let f = frame("Part of #321\n\nclose #321", "", "", &[("321", PR)]);
    assert!(plan(&f, "999", false).is_empty());
}

#[test]
fn t17b_dry_run_prefixes_both_lines_and_stays_conditional() {
    let f = frame("Verify, then close #123.\n\nContributes to #123", "", "", &[("123", OPEN)]);
    let steps = plan(&f, "999", true);
    assert_eq!(sets(&steps), (vec![123], vec![123]));
    let w = warnings(&steps);
    assert!(w.starts_with("[dry-run] Partial-increment conflict (#4569)"), "{w}");
    assert!(w.contains("  [dry-run] merge-pr.sh would reopen #123"), "{w}");
    assert!(!w.contains("will reopen #123"), "{w}");
}

#[test]
fn t21_commit_message_keyword_is_attributed_to_the_commit() {
    let f = frame(
        "Implements a slice.\n\nContributes to #123",
        "feat: implement the slice\n\nclose #123",
        "",
        &[("123", OPEN)],
    );
    let steps = plan(&f, "999", false);
    assert_eq!(sets(&steps), (vec![123], vec![123]));
    let w = warnings(&steps);
    assert!(w.contains("closing keyword in a commit message of this PR"), "{w}");
    assert!(w.contains("reword the offending commit message"), "{w}");
    assert!(!w.contains("its body ALSO carries"), "{w}");
}

#[test]
fn t25_non_adjacent_keyword_in_a_commit_is_not_a_reference() {
    let f = frame(
        "Part of #123",
        "chore: follow-up will close issue #123 later",
        "",
        &[("123", OPEN)],
    );
    assert_eq!(sets(&plan(&f, "999", false)), (vec![123], vec![]));
}

// --- what the port adds ----------------------------------------------------

#[test]
fn no_declaration_is_an_empty_plan_even_with_closing_refs() {
    let f = frame("Closes #123", "close #123", "123", &[]);
    assert!(plan(&f, "999", false).is_empty());
}

#[test]
fn an_issue_missing_from_the_frame_reads_as_unanswered_not_open() {
    let f = frame("Part of #123\n\nclose #123", "", "", &[]);
    assert!(plan(&f, "999", false).is_empty());
}

#[test]
fn a_failed_read_with_the_fallback_appended_is_not_open() {
    let f = frame("Part of #123", "", "", &[("123", r#"{"message":"Not Found"}{}"#)]);
    assert!(plan(&f, "999", false).is_empty());
}

#[test]
fn a_zero_padded_sidebar_line_never_matches_as_grep_qx_did() {
    let f = frame("Part of #123", "", "0123", &[("123", OPEN)]);
    assert_eq!(sets(&plan(&f, "999", false)), (vec![123], vec![]));
}

#[test]
fn sidebar_lines_that_are_not_bare_numbers_are_dropped() {
    let f = frame("Part of #123", "", "#123\n 123\n123x", &[("123", OPEN)]);
    assert_eq!(sets(&plan(&f, "999", false)), (vec![123], vec![]));
}

#[test]
fn several_declarations_are_decided_independently_in_ascending_order() {
    let f = frame(
        "Part of #456\nPart of #123\n\nfixes #456",
        "",
        "",
        &[("456", OPEN), ("123", OPEN)],
    );
    let steps = plan(&f, "999", false);
    assert_eq!(sets(&steps), (vec![123, 456], vec![456]));
}

#[test]
fn frame_parse_requires_nul_termination_and_paired_issues() {
    assert_eq!(
        Frame::parse("b\0c\0g\x00123\0{}\0"),
        Some(frame("b", "c", "g", &[("123", "{}")]))
    );
    assert_eq!(Frame::parse("b\0c\0\0"), Some(frame("b", "c", "", &[])));
    assert_eq!(Frame::parse(""), None);
    assert_eq!(Frame::parse("b\0c\0g"), None, "unterminated");
    assert_eq!(Frame::parse("b\0c\0g\x00123\0"), None, "unpaired issue");
}

#[test]
fn render_terminates_with_done_and_frames_every_line() {
    let out = render(&[
        Step::Open(1),
        Step::Conflict(1),
        Step::Warning("a\nb".into()),
    ]);
    assert_eq!(
        out,
        "OPEN\t1\nCONFLICT\t1\nWARNING\ta\nWARNING\tb\nLOOM-PARTIAL-CONFLICT-DONE\n"
    );
    assert_eq!(render(&[]), "LOOM-PARTIAL-CONFLICT-DONE\n");
}
