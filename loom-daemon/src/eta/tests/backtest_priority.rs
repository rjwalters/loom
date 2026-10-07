//! The `eta-fit/v2` priority inputs on replayed cases (#10508): the same
//! builder the fit and serving call, over a batch's own PR timelines.

use crate::eta::backtest::{
    cases_from_pr_records, cases_from_pr_records_with_roster, PrCaseRecord, ReplayCase,
};
use crate::eta::fit::features_v2::PriorityInputs;
use crate::eta::repo_priority::{FleetMember, RosterRevision};
use crate::pr_latency::history::fixtures::{labeled, merged, open, t, unlabeled};
use crate::pr_latency::history::PrEvent;
use crate::pr_latency::REVIEW_REQUESTED;

const STAR: &str = "loom:operator-priority";
const A: &str = "acme/a";
const B: &str = "acme/b";

fn roster() -> Vec<RosterRevision> {
    let member = |repo: &str, priority| FleetMember {
        repo: Some(repo.to_string()),
        priority,
    };
    vec![RosterRevision {
        committed_at: t(-100_000),
        observed_at: None,
        members: vec![member(A, 10), member(B, 50)],
    }]
}

/// The subject: repo A's PR 1, entering review at 100 and merging at 5000.
fn subject_record() -> PrCaseRecord {
    let h = merged(1, 5000, vec![labeled(REVIEW_REQUESTED, 100)]);
    let mut r = PrCaseRecord::from_history(A, &h, Some(vec![11]));
    r.linked_star = Some(Vec::new());
    r
}

/// A neighbour in repo B, in review from 60 and still open, with `events`
/// on top.
fn neighbour(
    events: Vec<PrEvent>,
    linked: Option<Vec<chrono::DateTime<chrono::Utc>>>,
) -> PrCaseRecord {
    let mut all = vec![labeled(REVIEW_REQUESTED, 60)];
    all.extend(events);
    let mut r = PrCaseRecord::from_history(B, &open(2, &[REVIEW_REQUESTED], all), None);
    r.linked_star = linked;
    r
}

fn priority_of(
    records: &[PrCaseRecord],
    history: Option<&[RosterRevision]>,
) -> Option<PriorityInputs> {
    let (cases, _) = cases_from_pr_records_with_roster(records, history);
    let case: &ReplayCase = cases
        .iter()
        .find(|c| c.subject.pr_number == Some(1) && c.as_of == t(100))
        .expect("the subject's first case");
    case.priority
}

#[test]
fn the_plain_builder_carries_no_priority_and_the_cases_are_otherwise_identical() {
    let records = [subject_record(), neighbour(Vec::new(), Some(Vec::new()))];
    let (plain, _) = cases_from_pr_records(&records);
    assert!(plain.iter().all(|c| c.priority.is_none()));
    let (rich, _) = cases_from_pr_records_with_roster(&records, Some(&roster()));
    assert!(rich.iter().all(|c| c.priority.is_some()));
    let stripped: Vec<ReplayCase> = rich
        .into_iter()
        .map(|c| ReplayCase {
            priority: None,
            ..c
        })
        .collect();
    assert_eq!(stripped, plain);
}

#[test]
fn a_star_outranks_repo_priority_in_the_dispatch_position() {
    // The neighbour (repo B, the lower-priority repo) is starred: it
    // dispatches before the unstarred subject in repo A.
    let records = [
        subject_record(),
        neighbour(vec![labeled(STAR, 50)], Some(Vec::new())),
    ];
    let p = priority_of(&records, Some(&roster())).unwrap();
    assert_eq!(p.starred_any, Some(false));
    assert_eq!(p.priority_level, Some(0));
    assert_eq!(p.repo_rank, Some(0.0));
    assert_eq!(p.ahead_dispatch_fleet, Some(1));
}

#[test]
fn without_a_star_the_higher_priority_repo_dispatches_first() {
    let records = [subject_record(), neighbour(Vec::new(), Some(Vec::new()))];
    let p = priority_of(&records, Some(&roster())).unwrap();
    assert_eq!(p.ahead_dispatch_fleet, Some(0));
}

#[test]
fn a_linked_issue_star_counts_as_a_star() {
    let records = [subject_record(), neighbour(Vec::new(), Some(vec![t(40)]))];
    let p = priority_of(&records, Some(&roster())).unwrap();
    assert_eq!(p.ahead_dispatch_fleet, Some(1));
}

#[test]
fn nothing_after_the_case_reaches_its_priority_inputs() {
    let base = [subject_record(), neighbour(Vec::new(), Some(Vec::new()))];
    let later = [
        subject_record(),
        // Starred by label at 200 and by its issue at 300, after the
        // subject's case at 100.
        neighbour(vec![labeled(STAR, 200)], Some(vec![t(300)])),
    ];
    assert_eq!(priority_of(&base, Some(&roster())), priority_of(&later, Some(&roster())));
    // A star removed again before the case never counts either.
    let removed = [
        subject_record(),
        neighbour(vec![labeled(STAR, 10), unlabeled(STAR, 20)], Some(Vec::new())),
    ];
    assert_eq!(priority_of(&removed, Some(&roster())), priority_of(&base, Some(&roster())));
}

#[test]
fn a_roster_revision_not_yet_knowable_is_unknown_not_todays_file() {
    let mut late = roster();
    late[0].committed_at = t(50); // inside the 120 s lag before the case
    let records = [subject_record(), neighbour(Vec::new(), Some(Vec::new()))];
    for history in [None, Some(late.as_slice())] {
        let p = priority_of(&records, history).unwrap();
        assert_eq!(p.repo_rank, None);
        assert_eq!(p.ahead_dispatch_fleet, None);
        // The star inputs do not depend on the roster.
        assert_eq!(p.starred_any, Some(false));
    }
}

#[test]
fn an_unread_linked_star_is_unknown_never_unstarred() {
    let mut subject = subject_record();
    subject.linked_star = None;
    let p = priority_of(&[subject, neighbour(Vec::new(), None)], Some(&roster())).unwrap();
    assert_eq!(p.starred_any, None);
    assert_eq!(p.priority_level, None);
    assert_eq!(p.ahead_dispatch_fleet, None);
    assert_eq!(p.repo_rank, Some(0.0));
    // A PR starred by its own labels is known even so.
    let h = merged(1, 5000, vec![labeled(REVIEW_REQUESTED, 100), labeled(STAR, 90)]);
    let own = PrCaseRecord::from_history(A, &h, Some(vec![11]));
    let p = priority_of(&[own], Some(&roster())).unwrap();
    assert_eq!(p.starred_any, Some(true));
    assert_eq!(p.priority_level, Some(1));
}

#[test]
fn a_record_without_linked_star_round_trips_unchanged() {
    let r = PrCaseRecord::from_history(A, &merged(1, 5000, vec![]), Some(vec![11]));
    let json = serde_json::to_string(&r).unwrap();
    assert!(!json.contains("linked_star"));
    let back: PrCaseRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(back, r);
    let with = PrCaseRecord {
        linked_star: Some(vec![t(5)]),
        ..r
    };
    let back: PrCaseRecord = serde_json::from_str(&serde_json::to_string(&with).unwrap()).unwrap();
    assert_eq!(back, with);
}

mod fill_linked {
    use super::*;
    use crate::eta::backtest::fill_linked_stars;
    use crate::eta::star::{IssueStarChange, RepoStar, StarInputs, StarLink};

    fn stars(synced_through: Option<chrono::DateTime<chrono::Utc>>) -> StarInputs {
        let mut star = RepoStar::default();
        star.links.insert(
            1,
            vec![StarLink {
                issue: 11,
                known_at: t(10),
            }],
        );
        star.issue_stars = vec![
            IssueStarChange {
                issue: 11,
                at: t(20),
                starred: true,
            },
            IssueStarChange {
                issue: 11,
                at: t(4000),
                starred: false,
            },
            // After the PR merged: never read into the record.
            IssueStarChange {
                issue: 11,
                at: t(9000),
                starred: true,
            },
        ];
        star.links_from = Some(t(-10));
        star.issue_events_from = Some(t(-10));
        star.synced_through = synced_through;
        let mut inputs = StarInputs::default();
        inputs.repos.insert(A.to_string(), star);
        inputs
    }

    fn unread(repo: &str) -> PrCaseRecord {
        let mut r = subject_record();
        r.repo = repo.to_string();
        r.linked_star = None;
        r
    }

    #[test]
    fn a_covered_record_gets_the_flips_before_its_end() {
        let mut records = [unread(A)];
        assert_eq!(fill_linked_stars(&mut records, &stars(None), t(99_999)), 1);
        assert_eq!(records[0].linked_star, Some(vec![t(20), t(4000)]));
    }

    #[test]
    fn an_uncovered_record_stays_unread_never_unstarred() {
        // Another repo (no cache), and a repo whose cache ends before the PR did.
        let mut records = [unread(B), unread(A)];
        assert_eq!(fill_linked_stars(&mut records, &stars(Some(t(100))), t(99_999)), 0);
        assert_eq!(records[0].linked_star, None);
        assert_eq!(records[1].linked_star, None);
    }

    #[test]
    fn a_record_that_already_carries_a_read_is_untouched() {
        let mut records = [subject_record()];
        assert_eq!(fill_linked_stars(&mut records, &stars(None), t(99_999)), 0);
        assert_eq!(records[0].linked_star, Some(Vec::new()));
    }
}
