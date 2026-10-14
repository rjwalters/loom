//! Tests for the cited-only gathering behind `notify-cleared-blockers`
//! (#10515), against the counting fake from `batch_tests.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;

use super::batch::Options;
use super::batch_tests::{comment, fleet, issue, row, state, Fake};
use super::budget::Floor;
use super::notify::{gather_cited, has_marker, marker_for, CitedGathering};
use super::{classify, Artifact, Verdict};
use crate::dep_recheck::extract;

/// The merged PR in every case below.
const MERGED_PR: i64 = 999;

fn run(fake: &mut Fake, closed: &[i64]) -> CitedGathering {
    gather_cited(
        fake,
        &fleet(),
        Options {
            limit: 100,
            no_prs: false,
            floor: Floor::default(),
        },
        closed,
    )
}

/// Today's population (39 blocked issues, 4 blocked PRs); a merge closing
/// #7 through PR #999. Issue 1 cites #7, issue 2 cites the merged PR itself.
fn population() -> Fake {
    let mut fake = Fake::default();
    fake.rows.push(issue(1, "Blocked by #7"));
    fake.rows.push(issue(2, "Blocked by #999 landing."));
    for n in 3..=39u32 {
        // Every 5th has comments, none citing anything closed.
        let comments = u32::from(n % 5 == 0);
        fake.rows.push(row(
            n,
            &format!("Blocked by #{}", 500 + n % 3),
            comments,
            false,
            &["loom:blocked"],
        ));
        if comments > 0 {
            fake.comments
                .insert(n, vec![comment("a-human", "still waiting on it")]);
        }
    }
    for n in 401..=404u32 {
        fake.rows
            .push(row(n, "Blocked by #600", u32::from(n == 401), true, &["loom:blocked"]));
    }
    fake.comments
        .insert(401, vec![comment("a-human", "parked, see body")]);
    fake.states.insert((None, 7), state("CLOSED"));
    fake.states.insert((None, MERGED_PR), state("MERGED"));
    fake
}

#[test]
fn a_merge_reads_only_what_its_citers_need() {
    let mut fake = population();
    let out = run(&mut fake, &[7, MERGED_PR]);

    assert_eq!(fake.list_calls, 1, "one listing for issues and PRs together");
    // Comments: only rows with a non-zero count, once each.
    let mut with_comments: Vec<u32> = (3..=39).filter(|n| n % 5 == 0).collect();
    with_comments.push(401);
    assert_eq!(fake.comment_calls, with_comments);
    // Closing PRs: one batch, holding exactly the two citers.
    assert_eq!(fake.closing_calls, vec![2]);
    // Blocker states: only the closed numbers the citers name, once each.
    let states: HashSet<_> = fake.state_calls.iter().cloned().collect();
    assert_eq!(states.len(), fake.state_calls.len(), "no state read twice");
    assert_eq!(states, HashSet::from([(None, 7), (None, MERGED_PR)]));
    assert!(fake.merge_calls.is_empty(), "no parked PR cites a closed number");

    let numbers: Vec<i64> = out.gathering.items.iter().map(|g| g.number).collect();
    assert_eq!(numbers, vec![1, 2]);
    assert!(out
        .gathering
        .items
        .iter()
        .all(|g| matches!(classify(g.evidence.as_ref().unwrap()), Verdict::Stale(_))));
    assert_eq!(out.cited.get(&(Artifact::Issue, 1)), Some(&vec![7]));
    assert_eq!(out.cited.get(&(Artifact::Issue, 2)), Some(&vec![MERGED_PR]));
    assert_eq!(out.cited.len(), 2);
}

#[test]
fn nothing_cited_costs_no_closing_or_state_read() {
    let mut fake = population();
    let out = run(&mut fake, &[12345]);
    assert!(out.gathering.items.is_empty());
    assert!(out.cited.is_empty());
    assert!(fake.closing_calls.is_empty());
    assert!(fake.state_calls.is_empty());
    assert!(fake.merge_calls.is_empty());
}

#[test]
fn a_failed_text_read_is_kept_unevaluated_never_dropped_as_uncited() {
    let mut fake = Fake::default();
    // Three comments, no fixture: the comment read fails.
    fake.rows
        .push(row(1, "nothing in the body", 3, false, &["loom:blocked"]));
    fake.rows.push(issue(2, "Blocked by #500"));
    let out = run(&mut fake, &[7]);
    assert_eq!(out.gathering.items.len(), 1);
    let g = &out.gathering.items[0];
    assert_eq!(g.number, 1);
    assert!(g.evidence.is_err(), "reported unevaluated");
    assert!(!out.cited.contains_key(&(Artifact::Issue, 1)));
    assert!(fake.closing_calls.is_empty(), "a failed read is not queried further");
}

#[test]
fn an_already_notified_citer_is_skipped() {
    let mut fake = Fake::default();
    fake.rows
        .push(row(1, "Blocked by #7", 1, false, &["loom:blocked"]));
    fake.comments
        .insert(1, vec![comment(extract::DEFAULT_BOT_LOGIN, &marker_for(7))]);
    fake.states.insert((None, 7), state("CLOSED"));
    let out = run(&mut fake, &[7]);
    assert!(out.gathering.items.is_empty());
    assert!(fake.closing_calls.is_empty());
    assert!(fake.state_calls.is_empty());
}

#[test]
fn a_parked_pr_citer_gets_its_own_evidence() {
    let mut fake = Fake::default();
    fake.rows
        .push(row(301, "Blocked by #7: standing down.", 0, true, &["loom:blocked"]));
    fake.states.insert((None, 7), state("CLOSED"));
    fake.merge
        .insert(301, ("MERGEABLE".to_string(), "CLEAN".to_string()));
    let out = run(&mut fake, &[7]);
    assert_eq!(out.cited.get(&(Artifact::Pr, 301)), Some(&vec![7]));
    let g = &out.gathering.items[0];
    assert!(matches!(classify(g.evidence.as_ref().unwrap()), Verdict::Stale(_)));
    assert!(fake.closing_calls.is_empty(), "a PR has no closing-PR arm");
}

#[test]
fn marker_is_matched_per_number() {
    let input = extract::Input {
        body: marker_for(180),
        comments: Vec::new(),
    };
    assert!(has_marker(&input, 180));
    assert!(!has_marker(&input, 18));
}
