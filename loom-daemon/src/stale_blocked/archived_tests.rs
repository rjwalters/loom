//! The archived-repository probe in front of `check-stale-blocked` (#10562),
//! against the counting [`super::batch_tests::Fake`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::batch::{gather_checked, parse_archived, Gathering, Options};
use super::batch_tests::{fleet, issue, row, state, Fake};
use super::budget::Floor;

fn checked(fake: &mut Fake) -> Gathering {
    gather_checked(
        fake,
        &fleet(),
        Options {
            limit: 1000,
            no_prs: false,
            floor: Floor::default(),
        },
    )
}

/// A fake with one stale-looking issue and one undocumented PR, so a run
/// that did gather would report rows.
fn populated() -> Fake {
    let mut fake = Fake::default();
    fake.rows.push(issue(1, "Blocked by #9"));
    fake.rows.push(row(2, "", 3, true, &["loom:blocked"]));
    fake.states.insert((None, 9), state("CLOSED"));
    fake
}

fn no_listing_or_evidence_reads(fake: &Fake) -> bool {
    fake.list_calls == 0
        && fake.comment_calls.is_empty()
        && fake.state_calls.is_empty()
        && fake.merge_calls.is_empty()
        && fake.closing_calls.is_empty()
        && fake.budget_calls == 0
}

#[test]
fn archived_repo_reads_nothing_past_the_probe_and_reports_archived() {
    let mut fake = Fake {
        archived: Some(Ok(true)),
        ..populated()
    };
    let g = checked(&mut fake);
    assert_eq!(g.archived, Some(true));
    assert!(g.items.is_empty(), "no rows on an archived repo");
    assert!(g.enumerate_error.is_none(), "archived is not a failure");
    assert_eq!(fake.archived_calls, 1, "one repo probe");
    assert!(no_listing_or_evidence_reads(&fake));
    assert_eq!(g.cost.meter.rest_requests, 1, "the probe is the only read");
}

#[test]
fn failed_probe_is_unevaluated_never_archived_never_clean() {
    let mut fake = Fake {
        archived: Some(Err("HTTP 502".to_string())),
        ..populated()
    };
    let g = checked(&mut fake);
    assert_eq!(g.archived, None, "a failed probe is not an answer");
    let why = g.enumerate_error.expect("reported as not evaluated");
    assert!(
        why.contains("archived-repository probe failed") && why.contains("HTTP 502"),
        "{why}"
    );
    assert!(no_listing_or_evidence_reads(&fake));
}

#[test]
fn live_repo_is_gathered_as_before() {
    let mut fake = populated();
    let g = checked(&mut fake);
    assert_eq!(g.archived, Some(false));
    assert!(g.enumerate_error.is_none());
    assert_eq!(g.items.len(), 2);
    assert_eq!(fake.list_calls, 1);
    assert_eq!(fake.archived_calls, 1);
}

#[test]
fn parse_archived_needs_the_flag() {
    assert_eq!(parse_archived(r#"{"archived": true}"#), Ok(true));
    assert_eq!(parse_archived(r#"{"archived": false, "name": "x"}"#), Ok(false));
    // A missing or malformed flag is never read as "not archived".
    assert!(parse_archived(r#"{"name": "x"}"#).is_err());
    assert!(parse_archived(r#"{"archived": "yes"}"#).is_err());
    assert!(parse_archived("not json").is_err());
}
