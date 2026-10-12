//! #8876 Part B: a run whose every issue lookup fails is its own condition.

use std::collections::HashMap;
use std::path::Path;

use super::{IssueStateSummary, RecordingLookup};
use crate::stash_retirement::{
    plan_and_execute_retirement, IssueStateLookup, QuarantineStashEntry, RetireVerdict,
    RetirementReport,
};

/// Answers from a table; an issue missing from it is unknown, like a `gh`
/// that 404s on a private repo.
struct TableLookup(HashMap<u64, bool>);

impl IssueStateLookup for TableLookup {
    fn is_closed(&mut self, issue: u64) -> Option<bool> {
        self.0.get(&issue).copied()
    }
}

fn entry(index: usize, issue: Option<u64>) -> QuarantineStashEntry {
    QuarantineStashEntry {
        stash_ref: format!("stash@{{{index}}}"),
        commit: format!("{index:040x}"),
        age: "1 hour ago".to_string(),
        label: "loom-quarantine: fixture".to_string(),
        issue,
        run_id: None,
    }
}

/// Classify `issues` (one stash each) against `open`, the issues the forge
/// can answer for. Every answerable issue is open, so no verdict reaches the
/// content check and no `git` runs.
fn run(
    issues: &[Option<u64>],
    open: &[u64],
    execute: bool,
) -> (Vec<RetirementReport>, IssueStateSummary) {
    let entries: Vec<QuarantineStashEntry> = issues
        .iter()
        .enumerate()
        .map(|(i, issue)| entry(i, *issue))
        .collect();
    let mut table = TableLookup(open.iter().map(|n| (*n, false)).collect());
    let mut lookup = RecordingLookup::new(&mut table);
    let reports =
        plan_and_execute_retirement(Path::new("/nonexistent"), &entries, &mut lookup, execute);
    let summary = IssueStateSummary::new(&reports, lookup.seen());
    (reports, summary)
}

#[test]
fn every_lookup_unknown_is_reported_once_as_a_blind_run() {
    // The fleet incident: nine stashes, eight distinct issues, all unreadable.
    let issues = [969, 992, 992, 1001, 1002, 1003, 1004, 1005, 1006].map(Some);
    let (reports, summary) = run(&issues, &[], false);

    assert!(summary.unreadable_for_all);
    assert_eq!((summary.lookups_attempted, summary.lookups_unknown), (8, 8));
    assert_eq!(summary.stashes_unknown, 9);
    let message = summary.blind_message().unwrap();
    assert!(message.starts_with("issue state unreadable for all 9 stash(es)"), "{message}");
    assert!(message.contains("ambient gh login"), "{message}");
    assert_eq!(message.lines().count(), 1, "one line, not one per stash");
    // Fail-closed is untouched: every stash is still kept.
    assert!(reports
        .iter()
        .all(|r| matches!(r.verdict, RetireVerdict::Keep { .. })));
}

#[test]
fn a_blind_run_drops_nothing_even_with_execute() {
    let (reports, summary) = run(&[Some(1), Some(2)], &[], true);
    assert!(summary.unreadable_for_all);
    assert!(reports
        .iter()
        .all(|r| r.outcome.is_none() && matches!(r.verdict, RetireVerdict::Keep { .. })));
}

#[test]
fn one_unreadable_issue_among_readable_ones_is_not_a_blind_run() {
    let (reports, summary) = run(&[Some(1), Some(2), Some(3)], &[1, 3], false);

    assert!(!summary.unreadable_for_all);
    assert_eq!((summary.lookups_attempted, summary.lookups_unknown), (3, 1));
    assert_eq!(summary.stashes_unknown, 1);
    assert_eq!(summary.blind_message(), None);
    let RetireVerdict::Keep { reason, .. } = &reports[1].verdict else {
        panic!("an unknown issue state must keep its stash");
    };
    assert_eq!(reason, "could not determine the state of issue #2");
}

#[test]
fn stashes_without_an_issue_token_are_not_attempted_lookups() {
    let (_, summary) = run(&[None, None], &[], false);
    assert_eq!(summary.lookups_attempted, 0);
    assert!(!summary.unreadable_for_all);
    assert_eq!(summary.blind_message(), None);
}

#[test]
fn tokenless_stashes_do_not_dilute_or_inflate_a_blind_run() {
    let (_, summary) = run(&[None, Some(7), None], &[], false);
    assert!(summary.unreadable_for_all);
    assert_eq!(summary.stashes_unknown, 1, "only the stash that named an issue");
}

#[test]
fn a_single_scoped_stash_whose_lookup_fails_is_blind() {
    // `--issue N` with one stash: the only lookup failed, so nothing in this
    // run was readable.
    let (_, summary) = run(&[Some(42)], &[], false);
    assert!(summary.unreadable_for_all);
    assert_eq!((summary.lookups_attempted, summary.stashes_unknown), (1, 1));
}

#[test]
fn no_stashes_is_not_a_blind_run() {
    let (_, summary) = run(&[], &[], false);
    assert!(!summary.unreadable_for_all);
}

#[test]
fn the_json_shape_names_the_condition_explicitly() {
    let (_, summary) = run(&[Some(1)], &[], false);
    let value = serde_json::to_value(&summary).unwrap();
    assert_eq!(value["unreadable_for_all"], true);
    assert_eq!(value["lookups_attempted"], 1);
    assert_eq!(value["lookups_unknown"], 1);
    assert_eq!(value["stashes_unknown"], 1);
    assert!(value.get("credential").is_none(), "no credential label ships with Part B");
}
