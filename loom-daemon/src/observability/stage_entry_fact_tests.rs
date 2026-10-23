use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use super::*;
use crate::discovery::{RepoSnapshot, RepoStatus, Source, Tier};
use crate::forge_listing::RestIssue;

fn at(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

fn item(number: u32, labels: &[&str], pr: bool, labels_at: Option<(&str, &str)>) -> RestIssue {
    RestIssue {
        comments: 0,
        number,
        title: None,
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
        created_at: None,
        updated_at: None,
        closed_at: None,
        state: "open".into(),
        body: None,
        author: None,
        author_association: None,
        is_pull_request: pr,
        open_sub_issues: None,
        labels_at: labels_at.map(|(l, t)| BTreeMap::from([(l.to_string(), t.to_string())])),
        draft: false,
    }
}

fn repo(rows: Vec<RestIssue>) -> RepoSnapshot {
    RepoSnapshot {
        slug: "Acme/App".into(),
        source: Source::Worklist,
        tier: Tier::Mirror,
        status: RepoStatus::Ok,
        rows,
        pulls: Some(Vec::new()),
        extras: BTreeMap::new(),
        watermark: None,
    }
}

#[test]
fn first_read_seeds_the_baseline_and_emits_nothing() {
    let mut t = ArrivalTracker::default();
    let now = at("2026-10-20T10:00:00Z");
    assert!(t
        .diff(&repo(vec![item(1, &["loom:issue"], false, None)]), now)
        .is_empty());
}

#[test]
fn a_label_that_appears_between_reads_is_an_arrival() {
    let mut t = ArrivalTracker::default();
    let t0 = at("2026-10-20T10:00:00Z");
    let t1 = at("2026-10-20T10:01:00Z");
    t.diff(&repo(vec![item(1, &["loom:curated"], false, None)]), t0);
    // #1 moves curated -> issue; #2 is new with loom:triage.
    let rows = vec![
        item(1, &["loom:curated", "loom:issue"], false, None),
        item(2, &["loom:triage"], false, None),
    ];
    let got = t.diff(&repo(rows), t1);
    assert_eq!(
        got,
        vec![
            Arrival {
                issue: 1,
                stage: "loom:issue",
                entered_at: t1,
                source: EnteredAtSource::Observed
            },
            Arrival {
                issue: 2,
                stage: "loom:triage",
                entered_at: t1,
                source: EnteredAtSource::Observed
            },
        ]
    );
}

#[test]
fn mirror_label_time_is_preferred_over_the_tick() {
    let mut t = ArrivalTracker::default();
    t.diff(&repo(vec![]), at("2026-10-20T10:00:00Z"));
    let rows = vec![item(
        3,
        &["loom:issue"],
        false,
        Some(("loom:issue", "2026-10-20T10:00:30Z")),
    )];
    let got = t.diff(&repo(rows), at("2026-10-20T10:01:00Z"));
    assert_eq!(got[0].entered_at, at("2026-10-20T10:00:30Z"));
    assert_eq!(got[0].source, EnteredAtSource::Mirror);
}

#[test]
fn unchanged_prs_and_non_stage_labels_emit_nothing_and_reapplication_re_emits() {
    let mut t = ArrivalTracker::default();
    let now = at("2026-10-20T10:00:00Z");
    let rows = || {
        vec![
            item(1, &["loom:issue"], false, None),
            item(9, &["loom:issue"], true, None),
        ]
    };
    t.diff(&repo(rows()), now);
    let mut next = rows();
    next.push(item(4, &["loom:building"], false, None));
    assert!(
        t.diff(&repo(next), now).is_empty(),
        "stable, PR and non-stage rows are not arrivals"
    );
    // Label removed, then applied again: a new arrival.
    assert!(t.diff(&repo(vec![]), now).is_empty());
    let got = t.diff(&repo(vec![item(1, &["loom:issue"], false, None)]), now);
    assert_eq!(got.len(), 1);
}

#[test]
fn forgetting_a_repo_re_seeds_instead_of_reporting_the_gap() {
    let mut t = ArrivalTracker::default();
    let now = at("2026-10-20T10:00:00Z");
    t.diff(&repo(vec![]), now);
    t.forget("acme/app");
    assert!(t
        .diff(&repo(vec![item(1, &["loom:issue"], false, None)]), now)
        .is_empty());
}
