//! The queue, drain and friction features (#10201): the pure contract
//! (`eta::queue_features`), its knowability rule, and the journal adapter.

use super::{as_of, provenance, EXPLANATION_GOLDEN};
use crate::eta::explanation::{Explanation, Features};
use crate::eta::journal::JournalEntry;
use crate::eta::queue_features::{
    queue_features, reason, EventKind, EventLog, QueueFeatures, QueueSubject, RosterEntry,
    StageEvent, NAMES, PR_STAGE_FEATURES, SINCE_MERGE_CAP_SEC,
};
use crate::eta::tracker::events_from_journal;
use crate::eta::Stage;
use chrono::{DateTime, Duration, Utc};

const REPO: &str = "rjwalters/loom";
const OTHER: &str = "rjwalters/other";
const OUTSIDE: &str = "rjwalters/outside";

fn ago(secs: i64) -> DateTime<Utc> {
    as_of() - Duration::seconds(secs)
}

const H: i64 = 3600;

fn entry(repo: &str, pr: u32, stage: Option<Stage>, entered_ago: i64) -> RosterEntry {
    RosterEntry {
        repo: repo.to_string(),
        pr,
        stage,
        entered_at: ago(entered_ago),
        known_at: ago(60),
    }
}

fn event(repo: &str, pr: u32, stage: Stage, kind: EventKind, at_ago: i64) -> StageEvent {
    StageEvent {
        repo: repo.to_string(),
        pr: Some(pr),
        stage: Some(stage),
        kind,
        at: ago(at_ago),
        known_at: ago(1),
    }
}

fn subject() -> QueueSubject {
    QueueSubject {
        repo: REPO.to_string(),
        pr: Some(100),
        current: Some((Stage::ReviewWait, ago(2 * H))),
    }
}

fn scope() -> Vec<String> {
    vec![REPO.to_string(), OTHER.to_string()]
}

/// The hand-computed fixture: see the expected values in
/// [`every_contract_feature_matches_the_hand_computed_fixture`].
fn roster() -> Vec<RosterEntry> {
    use Stage::{Doctor, MergeWait, ReviewWait};
    vec![
        entry(REPO, 100, Some(ReviewWait), 2 * H), // the subject itself
        entry(REPO, 101, Some(ReviewWait), 3 * H), // ahead: entered earlier
        entry(REPO, 102, Some(ReviewWait), H),     // behind
        entry(REPO, 99, Some(ReviewWait), 2 * H),  // tie, lower PR: ahead
        entry(REPO, 103, Some(ReviewWait), 2 * H), // tie, higher PR: behind
        entry(REPO, 104, Some(MergeWait), 5 * H),  // another stage
        entry(REPO, 105, None, 4 * H),             // held: no stage
        entry(OTHER, 7, Some(ReviewWait), 10 * H), // fleet
        entry(OTHER, 8, Some(Doctor), H),          // fleet, another stage
        entry(OUTSIDE, 1, Some(ReviewWait), H),    // outside the scope
    ]
}

fn log() -> EventLog {
    use EventKind::{Exit, Merge};
    use Stage::{Doctor, MergeWait, ReviewWait};
    EventLog {
        from: Some(ago(30 * 24 * H)),
        events: vec![
            event(REPO, 90, ReviewWait, Exit, H / 2),
            event(REPO, 91, ReviewWait, Exit, 5 * H),
            // Closed unmerged: still a departure.
            event(REPO, 92, ReviewWait, Exit, 20 * H),
            event(REPO, 93, ReviewWait, Exit, 30 * H), // outside every window
            event(REPO, 94, Doctor, Exit, 600),        // another stage
            event(REPO, 80, MergeWait, Merge, 3 * H),
            event(REPO, 81, MergeWait, Merge, 23 * H),
            event(OTHER, 6, ReviewWait, Exit, 2 * H),
            event(OTHER, 5, MergeWait, Merge, H),
            event(OUTSIDE, 4, MergeWait, Merge, 600), // outside the scope
        ],
    }
}

fn compute() -> QueueFeatures {
    queue_features(&subject(), &roster(), &log(), &scope(), as_of())
}

#[test]
fn every_contract_feature_matches_the_hand_computed_fixture() {
    let f = compute();
    assert_eq!(f.ahead, Some(2), "#101 entered earlier; #99 ties and has the lower number");
    assert_eq!(f.n_stage_repo, Some(4), "#99, #101, #102, #103: the subject is not 'other'");
    assert_eq!(f.n_stage_fleet, Some(5), "plus other#7; outside#1 is out of scope");
    assert_eq!(f.exits_repo_1h, Some(1));
    assert_eq!(f.exits_repo_6h, Some(2));
    assert_eq!(f.exits_repo_24h, Some(3), "the PR closed unmerged departed too");
    assert_eq!(f.exits_fleet_1h, Some(1));
    assert_eq!(f.exits_fleet_6h, Some(3));
    assert_eq!(f.exits_fleet_24h, Some(4));
    assert_eq!(f.merges_repo_24h, Some(2));
    assert_eq!(f.merges_fleet_6h, Some(2), "repo -3h and other -1h; outside is out of scope");
    assert_eq!(f.since_merge_sec, Some(3 * H));
    assert_eq!(
        f.open_prs_repo,
        Some(7),
        "every review-label PR, the held one and the subject included"
    );
    assert_eq!(f.fleet_scope_repos, Some(2));
    assert!(f.omitted.is_empty(), "{:?}", f.omitted);
}

#[test]
fn ahead_ties_break_on_the_lower_pr_number() {
    let roster = vec![
        entry(REPO, 100, Some(Stage::ReviewWait), 2 * H),
        entry(REPO, 50, Some(Stage::ReviewWait), 2 * H),
        entry(REPO, 150, Some(Stage::ReviewWait), 2 * H),
    ];
    let at = |pr: u32| {
        let mut subject = subject();
        subject.pr = Some(pr);
        queue_features(&subject, &roster, &EventLog::default(), &scope(), as_of()).ahead
    };
    assert_eq!((at(50), at(100), at(150)), (Some(0), Some(1), Some(2)));
}

#[test]
fn windows_are_half_open_at_as_of() {
    let mut log = EventLog::default();
    let edge = |at: DateTime<Utc>| StageEvent {
        repo: REPO.to_string(),
        pr: Some(1),
        stage: Some(Stage::ReviewWait),
        kind: EventKind::Merge,
        at,
        known_at: ago(1),
    };
    log.events.push(edge(ago(6 * H))); // exactly as_of - 6h: counts
    log.events.push(edge(as_of())); // exactly as_of: does not
    log.events.push(edge(ago(6 * H + 1))); // just outside 6h
    let f = queue_features(&subject(), &roster(), &log, &scope(), as_of());
    assert_eq!(f.exits_repo_6h, Some(1));
    assert_eq!(f.exits_repo_24h, Some(2));
    assert_eq!(f.merges_fleet_6h, Some(1));
    assert_eq!(f.merges_repo_24h, Some(2));
    assert_eq!(f.since_merge_sec, Some(6 * H), "the event at as_of is not the last merge");
}

#[test]
fn since_merge_is_capped_at_168_hours() {
    let merge_at = |secs: i64| EventLog {
        from: Some(ago(400 * H)),
        events: vec![event(REPO, 1, Stage::MergeWait, EventKind::Merge, secs)],
    };
    let since = |log: &EventLog| {
        queue_features(&subject(), &roster(), log, &scope(), as_of()).since_merge_sec
    };
    assert_eq!(since(&merge_at(200 * H)), Some(SINCE_MERGE_CAP_SEC));
    assert_eq!(since(&merge_at(167 * H)), Some(167 * H));
    assert_eq!(SINCE_MERGE_CAP_SEC, 604_800);
}

#[test]
fn no_merge_is_the_cap_only_when_history_reaches_back_that_far() {
    let with_from = |from: Option<DateTime<Utc>>| {
        let log = EventLog {
            from,
            events: Vec::new(),
        };
        queue_features(&subject(), &roster(), &log, &scope(), as_of())
    };
    assert_eq!(
        with_from(Some(ago(SINCE_MERGE_CAP_SEC))).since_merge_sec,
        Some(SINCE_MERGE_CAP_SEC)
    );
    for short in [Some(ago(SINCE_MERGE_CAP_SEC - 1)), None] {
        let f = with_from(short);
        assert_eq!(f.since_merge_sec, None);
        let named: Vec<_> = f
            .omitted
            .iter()
            .map(|o| (o.name.as_str(), o.reason.as_str()))
            .collect();
        assert_eq!(named, vec![("since_merge_sec", reason::HISTORY_SHORTER_THAN_CAP)]);
    }
}

/// The leak test: nothing known at or after `as_of` can move a feature,
/// whatever it is and wherever it sits.
#[test]
fn nothing_known_at_or_after_as_of_changes_a_feature() {
    let base = compute();
    let mut roster = roster();
    let mut log = log();
    for (i, known_at) in [as_of(), as_of() + Duration::hours(1)]
        .into_iter()
        .enumerate()
    {
        let pr = 900 + u32::try_from(i).unwrap();
        roster.push(RosterEntry {
            known_at,
            ..entry(REPO, pr, Some(Stage::ReviewWait), 9 * H)
        });
        // The subject's own PR, re-observed late in another stage.
        roster.push(RosterEntry {
            known_at,
            ..entry(REPO, 100, Some(Stage::MergeWait), H)
        });
        // Happened before `as_of`, but known only at or after it.
        log.events.push(StageEvent {
            known_at,
            ..event(REPO, pr, Stage::ReviewWait, EventKind::Merge, 600)
        });
        log.events.push(StageEvent {
            known_at,
            ..event(OTHER, pr, Stage::ReviewWait, EventKind::Exit, 60)
        });
    }
    assert_eq!(queue_features(&subject(), &roster, &log, &scope(), as_of()), base);

    // Altering any of them changes nothing either.
    for r in roster.iter_mut().filter(|r| r.known_at >= as_of()) {
        r.stage = Some(Stage::ReviewWait);
        r.entered_at = ago(100 * H);
    }
    for e in log.events.iter_mut().filter(|e| e.known_at >= as_of()) {
        e.at = ago(10);
        e.kind = EventKind::Merge;
    }
    assert_eq!(queue_features(&subject(), &roster, &log, &scope(), as_of()), base);

    // Order does not matter.
    roster.reverse();
    log.events.reverse();
    assert_eq!(queue_features(&subject(), &roster, &log, &scope(), as_of()), base);
}

#[test]
fn null_pr_stage_features_carry_the_items_own_reason() {
    let reasons_for = |pr: Option<u32>, current: Option<(Stage, DateTime<Utc>)>| {
        let subject = QueueSubject {
            repo: REPO.to_string(),
            pr,
            current,
        };
        let f = queue_features(&subject, &roster(), &log(), &scope(), as_of());
        assert_eq!(f.ahead, None);
        assert!(f.merges_repo_24h.is_some() && f.open_prs_repo.is_some(), "every item");
        assert!(f.merges_fleet_6h.is_some() && f.since_merge_sec.is_some(), "every item");
        let mut reasons: Vec<String> = f.omitted.iter().map(|o| o.reason.clone()).collect();
        let names: Vec<&str> = f.omitted.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, PR_STAGE_FEATURES.to_vec());
        reasons.dedup();
        reasons
    };
    assert_eq!(reasons_for(Some(100), None), vec![reason::NO_STAGE]);
    let building = Some((Stage::SweepBuilder, ago(H)));
    assert_eq!(reasons_for(None, building), vec![reason::NO_PR_YET]);
    assert_eq!(reasons_for(Some(100), building), vec![reason::NOT_APPLICABLE_STAGE]);
    let ready = Some((Stage::ReadyWait, ago(H)));
    assert_eq!(reasons_for(None, ready), vec![reason::NO_PR_YET]);
    assert_eq!(reasons_for(None, Some((Stage::ReviewWait, ago(H)))), vec![reason::NO_PR_YET]);
}

#[test]
fn a_repo_outside_the_scope_keeps_its_fleet_values_only() {
    let scope = vec![OTHER.to_string()];
    let f = queue_features(&subject(), &roster(), &log(), &scope, as_of());
    assert_eq!(f.fleet_scope_repos, Some(1));
    assert_eq!(f.n_stage_fleet, Some(1), "other#7");
    assert_eq!(f.exits_fleet_24h, Some(1));
    assert_eq!(f.merges_fleet_6h, Some(1));
    for o in &f.omitted {
        assert_eq!(o.reason, reason::REPO_NOT_LISTED, "{}", o.name);
    }
    let names: Vec<&str> = f.omitted.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "ahead",
            "n_stage_repo",
            "exits_repo_1h",
            "exits_repo_6h",
            "exits_repo_24h",
            "merges_repo_24h",
            "since_merge_sec",
            "open_prs_repo"
        ]
    );

    // No repo listed at all: only the scope size is known.
    let empty = queue_features(&subject(), &roster(), &log(), &[], as_of());
    assert_eq!(empty.fleet_scope_repos, Some(0));
    assert_eq!(empty.omitted.len(), NAMES.len() - 1);
}

#[test]
fn every_null_has_exactly_one_reason_and_no_value_has_one() {
    let subjects = [
        subject(),
        QueueSubject {
            pr: None,
            ..subject()
        },
        QueueSubject {
            current: None,
            ..subject()
        },
        QueueSubject {
            repo: OUTSIDE.to_string(),
            ..subject()
        },
    ];
    for subject in &subjects {
        for log in [log(), EventLog::default()] {
            let f = queue_features(subject, &roster(), &log, &scope(), as_of());
            let mut features = Features::default();
            let mut omitted = Vec::new();
            f.write_to(&mut features, &mut omitted);
            let value = serde_json::to_value(&features).unwrap();
            for name in NAMES {
                let named = omitted.iter().filter(|o| o.name == name).count();
                assert_eq!(usize::from(value[name].is_null()), named, "{name} for {subject:?}");
            }
        }
    }
    let unavailable = QueueFeatures::unavailable("not_listed_yet");
    assert_eq!(unavailable.omitted.len(), NAMES.len());
}

#[test]
fn every_contract_feature_is_a_declared_feature() {
    for name in NAMES.iter().chain(["repo_pr_open_skip"].iter()) {
        assert!(Features::NAMES.contains(name), "{name}");
    }
}

/// An explanation recorded before #10201 has none of the new keys: it still
/// parses, with every new feature `None`.
#[test]
fn a_pre_change_explanation_still_parses() {
    let mut golden: serde_json::Value = serde_json::from_str(EXPLANATION_GOLDEN).unwrap();
    let new_names: Vec<&str> = NAMES.iter().copied().chain(["repo_pr_open_skip"]).collect();
    let features = golden["features"].as_object_mut().unwrap();
    for name in &new_names {
        assert!(features.remove(*name).is_some(), "{name} is in the blessed golden");
    }
    let omitted = golden["features_omitted"].as_array_mut().unwrap();
    omitted.retain(|o| !new_names.contains(&o["name"].as_str().unwrap()));
    let parsed: Explanation = serde_json::from_value(golden).unwrap();
    let features = parsed.features.unwrap();
    assert_eq!(features.ahead, None);
    assert_eq!(features.since_merge_sec, None);
    assert_eq!(features.repo_pr_open_skip, None);
    assert!(features.labels.is_some(), "the old fields are untouched");
}

// ---------------------------------------------------------------- purity

/// Like the heuristics' scan in `tests/fleet.rs`: the contract function must
/// not reach anything but its arguments, or train and serve could diverge.
#[test]
fn the_contract_reads_nothing_but_its_arguments() {
    const SOURCE: &str = include_str!("../queue_features.rs");
    const FORBIDDEN: &[&str] = &[
        "std::fs",
        "fs::",
        "File::",
        "read_to_string",
        "Command",
        "reqwest",
        "std::env",
        "env::var",
        "Utc::now",
        "SystemTime",
        "Instant::now",
        "tokio",
        "pub static",
        "\nstatic ",
        "static mut",
        "thread_local",
        "OnceLock",
        "Mutex",
    ];
    for needle in FORBIDDEN {
        assert!(!SOURCE.contains(needle), "queue_features.rs mentions `{needle}`");
    }
}

// ------------------------------------------------------- journal adapter

fn row(
    event: &str,
    stage: Option<Stage>,
    left_ago: Option<i64>,
    raw: serde_json::Value,
) -> JournalEntry {
    let observed = left_ago.map_or(ago(0), ago);
    let mut row = JournalEntry::new(event, "RJWalters/Loom", observed, &provenance());
    row.pr_number = Some(7);
    row.stage = stage;
    row.left_at = left_ago.map(ago);
    row.raw = raw;
    row
}

#[test]
fn journal_rows_become_departures_and_merges() {
    use serde_json::json;
    let known_at = ago(30);
    let rows = vec![
        row("label.transition", Some(Stage::ReviewWait), Some(H), json!({})),
        // Closed unmerged: a departure, not a merge.
        row("pr.resolved", Some(Stage::ReviewWait), Some(2 * H), json!({"state": "closed"})),
        row("pr.resolved", Some(Stage::MergeWait), Some(3 * H), json!({"state": "merged"})),
        // A backfilled PL3 row carries no state: a merge.
        row("pr.resolved", Some(Stage::MergeWait), Some(4 * H), json!({})),
        row("sweep.phase", Some(Stage::MergeWait), Some(5 * H), json!({"phase": "merge"})),
        // A pre-PR stage, an issue read and a first sighting: none counts.
        row("sweep.phase", Some(Stage::SweepBuilder), Some(H), json!({"phase": "builder"})),
        row("issue.resolved", None, Some(H), json!({"state": "closed_completed"})),
        row("label.first_seen", None, None, json!({})),
        // Too old for any window.
        row("label.transition", Some(Stage::Doctor), Some(25 * H), json!({})),
        row("pr.resolved", Some(Stage::MergeWait), Some(169 * H), json!({"state": "merged"})),
    ];
    let log = events_from_journal(&rows, known_at);
    assert_eq!(log.from, Some(ago(169 * H)), "the oldest row");
    let got: Vec<(Option<Stage>, EventKind, DateTime<Utc>)> =
        log.events.iter().map(|e| (e.stage, e.kind, e.at)).collect();
    let mut want = vec![
        (Some(Stage::ReviewWait), EventKind::Exit, ago(H)),
        (Some(Stage::ReviewWait), EventKind::Exit, ago(2 * H)),
        (Some(Stage::MergeWait), EventKind::Merge, ago(3 * H)),
        (Some(Stage::MergeWait), EventKind::Merge, ago(4 * H)),
        (Some(Stage::MergeWait), EventKind::Merge, ago(5 * H)),
    ];
    want.sort();
    let mut got_sorted = got.clone();
    got_sorted.sort();
    assert_eq!(got_sorted, want);
    assert!(log
        .events
        .iter()
        .all(|e| e.known_at == known_at && e.repo == "rjwalters/loom"));

    // The PR closed unmerged is counted as a departure from review_wait.
    let subject = QueueSubject {
        repo: REPO.to_string(),
        pr: Some(1),
        current: Some((Stage::ReviewWait, ago(H))),
    };
    let f = queue_features(&subject, &[], &log, &[REPO.to_string()], as_of());
    assert_eq!(f.exits_repo_6h, Some(2));
    assert_eq!(f.merges_repo_24h, Some(3));
    assert_eq!(f.since_merge_sec, Some(3 * H));

    // A row journaled twice is one event.
    let twice = events_from_journal(&[rows[0].clone(), rows[0].clone()], known_at);
    assert_eq!(twice.events.len(), 1);
}
