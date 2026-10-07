//! The SigNoz timeline reader (#10519).
//!
//! Pure: no daemon, no network, no clock. The fixture
//! (`fixtures/signoz-timeline.jsonl`) is rows in exactly the shape
//! `TIMELINE_SQL` returns as `JSONEachRow`, from both sources: the loom-ui
//! webhook export (`service = loom-ui-d1-export`, the D1 record as the body)
//! and the daemon (stage-journal label sets, `pr.resolved`, `ci.*`,
//! `queue.snapshot`).

use crate::eta::fleet_signoz_refresh::{
    FileRows, Limits, PageQuery, ReadError, SignozRead, SignozStop,
};
use crate::eta::fleet_signoz_timeline::{Family, Timeline, MATCH_SLACK_SEC};
use crate::eta::fleet_signoz_timeline_rows::{
    parse_row, reject, walk, ItemKey, Lifecycle, ParsedRow, Row, RowBody, Source, Target,
    Transition, SERVICE_WEBHOOK, TIMELINE_SQL,
};
use crate::eta::WINDOW_DAYS;
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::json;
use std::collections::BTreeSet;

const REPO: &str = "rjwalters/loom";
const FIXTURE: &str = include_str!("../fixtures/signoz-timeline.jsonl");

/// The fixture's `T0`.
fn t(sec: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 1, 10, 0, 0).unwrap() + Duration::seconds(sec)
}

fn pr(number: u32) -> ItemKey {
    ItemKey::new(REPO, Target::Pr, number)
}

fn limits(page_size: u32) -> Limits {
    Limits {
        page_size,
        max_pages: 100,
    }
}

/// Every fixture row, walked as the daemon would.
fn fixture_rows(page_size: u32) -> Vec<Row> {
    let mut reader = FileRows::parse(FIXTURE).unwrap();
    let (rows, report) = walk(REPO, &mut reader, t(-86_400), t(30 * 86_400), limits(page_size))
        .unwrap_or_else(|r| panic!("walk stopped: {r:?}"));
    assert_eq!(report.stop, SignozStop::Complete);
    rows
}

/// A label change, knowable when it happened (a webhook receipt, or the
/// daemon's observation).
fn label(source: Source, id: &str, item: &ItemKey, name: &str, tr: Transition, at: i64) -> Row {
    Row {
        record_id: id.to_string(),
        repo: REPO.to_string(),
        source,
        observed_at: Some(t(at)),
        body: RowBody::Label {
            item: item.clone(),
            label: name.to_string(),
            transition: tr,
            at: t(at),
        },
    }
}

fn label_set(id: &str, item: &ItemKey, labels: &[&str], observed: Option<i64>) -> Row {
    Row {
        record_id: id.to_string(),
        repo: REPO.to_string(),
        source: Source::Daemon,
        observed_at: observed.map(t),
        body: RowBody::LabelSet {
            item: item.clone(),
            labels: labels.iter().map(|l| (*l).to_string()).collect(),
        },
    }
}

fn lifecycle(
    source: Source,
    id: &str,
    item: &ItemKey,
    event: Lifecycle,
    at: i64,
    seen: i64,
) -> Row {
    Row {
        record_id: id.to_string(),
        repo: REPO.to_string(),
        source,
        observed_at: Some(t(seen)),
        body: RowBody::Lifecycle {
            item: item.clone(),
            event,
            at: t(at),
        },
    }
}

const RR: &str = "loom:review-requested";
const APPROVED: &str = "loom:pr";

// -- the fixture, end to end ----------------------------------------------

#[test]
fn fixture_rows_from_both_sources_give_labels_merge_close_ci_and_queue() {
    let rows = fixture_rows(500);
    let timeline = Timeline::build(&rows, t(30 * 86_400));

    // PR 900: both sources; every change dated by the webhook.
    let p900 = timeline.item(&pr(900)).unwrap();
    let changes: Vec<(&str, Transition, DateTime<Utc>, Source, bool)> = p900
        .labels
        .iter()
        .map(|e| (e.label.as_str(), e.transition, e.at, e.source, e.corroborated))
        .collect();
    assert_eq!(
        changes,
        vec![
            (RR, Transition::Added, t(0), Source::Webhook, false),
            (RR, Transition::Removed, t(3610), Source::Webhook, true),
            (APPROVED, Transition::Added, t(3612), Source::Webhook, true),
        ]
    );
    assert_eq!(p900.merged_at(), Some(t(7205)), "the webhook's receipt time wins");
    assert_eq!(p900.closed_at(), None);
    let merge = p900.resolution().unwrap();
    assert!(merge.corroborated, "the daemon's pr.resolved saw it too");
    assert_eq!(merge.observed_at, t(7205));
    assert_eq!(p900.labels_at(t(3611)), BTreeSet::from([] as [String; 0]));
    assert_eq!(p900.labels_at(t(3612)), BTreeSet::from([APPROVED.to_string()]));

    // PR 901: the daemon only; its polling times stand.
    let p901 = timeline.item(&pr(901)).unwrap();
    let changes: Vec<(&str, Transition, DateTime<Utc>, Source)> = p901
        .labels
        .iter()
        .map(|e| (e.label.as_str(), e.transition, e.at, e.source))
        .collect();
    assert_eq!(
        changes,
        vec![
            ("loom:changes-requested", Transition::Added, t(1000), Source::Daemon),
            (RR, Transition::Removed, t(1000), Source::Daemon),
        ]
    );
    assert_eq!(p901.closed_at(), Some(t(2000)));
    assert_eq!(p901.merged_at(), None);

    // Issue 899: a webhook-only issue label.
    let issue = timeline
        .item(&ItemKey::new(REPO, Target::Issue, 899))
        .unwrap();
    assert_eq!(issue.labels.len(), 1);
    assert_eq!(issue.labels[0].at, t(-600));

    // CI: the latest attempt of the workflow on the PR's branch, with its job.
    let ci = timeline.ci_for_ref(REPO, "feature/issue-899").unwrap();
    let latest = &ci.latest["CI"];
    assert_eq!((latest.run.run_id, latest.run.run_attempt), (55, 2));
    assert_eq!(latest.run.conclusion.as_deref(), Some("success"));
    assert_eq!(latest.observed_at, t(1510), "the earliest knowable copy");
    assert_eq!(latest.jobs.len(), 1);
    assert_eq!(latest.jobs[0].job, "test");
    assert!(ci.all_green());

    // Queue: the latest snapshot, this repo's rows only.
    let queue = timeline.queue.as_ref().unwrap();
    assert_eq!(queue.tick_at, t(1200));
    assert_eq!(queue.entries.len(), 1);
    assert_eq!(timeline.queue_entry(899).unwrap().state.as_deref(), Some("running"));

    // One redelivered record, one re-exported merge and one re-exported CI
    // run were dropped; three daemon changes found their webhook partner.
    assert_eq!(timeline.stats.duplicate_records, 1);
    assert_eq!(timeline.stats.duplicate_events, 2);
    assert_eq!(timeline.stats.corroborated, 3);
    assert_eq!(timeline.stats.not_knowable, 0);
}

#[test]
fn the_walk_rejects_foreign_kinds_and_pages_to_the_same_rows() {
    let mut reader = FileRows::parse(FIXTURE).unwrap();
    let (one_page, report) =
        walk(REPO, &mut reader, t(-86_400), t(30 * 86_400), limits(500)).unwrap();
    assert_eq!(report.pages, 1);
    assert_eq!(report.rejected.get(reject::UNKNOWN_KIND), Some(&1), "sweep.outcome");
    let mut reader = FileRows::parse(FIXTURE).unwrap();
    let (paged, report) = walk(REPO, &mut reader, t(-86_400), t(30 * 86_400), limits(3)).unwrap();
    assert!(report.pages > 3);
    // The redelivered webhook row repeats its cursor exactly, so a page
    // boundary between the two copies keeps one; the timeline is the same.
    assert_eq!(
        Timeline::build(&paged, t(30 * 86_400)).items,
        Timeline::build(&one_page, t(30 * 86_400)).items
    );
}

#[test]
fn an_unavailable_backend_stops_the_walk_with_nothing() {
    struct Down;
    impl SignozRead for Down {
        fn page(&mut self, _: &PageQuery) -> Result<String, ReadError> {
            Err(ReadError::Unavailable("connection refused".to_string()))
        }
    }
    let report = walk(REPO, &mut Down, t(0), t(1), limits(10)).unwrap_err();
    assert_eq!(report.stop, SignozStop::Unavailable);
}

#[test]
fn the_query_selects_every_column_the_parser_reads() {
    for column in [
        "AS record_id",
        "AS kind",
        "AS service",
        "AS repo",
        "AS attrs",
        "AS nums",
        "body,",
        "AS event_time_ns",
        "AS knowable_time_ns",
    ] {
        assert!(TIMELINE_SQL.contains(column), "TIMELINE_SQL lacks `{column}`");
    }
    for kind in crate::eta::fleet_signoz_timeline_rows::kind::ALL {
        assert!(TIMELINE_SQL.contains(&format!("'{kind}'")), "TIMELINE_SQL skips {kind}");
    }
}

// -- rule 2: webhook time wins ----------------------------------------------

#[test]
fn webhook_time_wins_when_both_sources_saw_the_change() {
    let item = pr(10);
    let rows = vec![
        label(Source::Webhook, "w", &item, RR, Transition::Added, 100),
        // The daemon's listing diff saw it one poll later.
        label(Source::Daemon, "d", &item, RR, Transition::Added, 400),
    ];
    let timeline = Timeline::build(&rows, t(1_000));
    let events = &timeline.item(&item).unwrap().labels;
    assert_eq!(events.len(), 1, "one change, not two");
    assert_eq!(events[0].at, t(100));
    assert_eq!(events[0].source, Source::Webhook);
    assert!(events[0].corroborated);
    assert_eq!(timeline.stats.corroborated, 1);
}

#[test]
fn a_webhook_receipt_a_little_after_a_fast_poll_is_still_the_same_change() {
    let item = pr(11);
    let rows = vec![
        label(Source::Daemon, "d", &item, RR, Transition::Added, 400),
        label(Source::Webhook, "w", &item, RR, Transition::Added, 400 + MATCH_SLACK_SEC),
    ];
    let events = &Timeline::build(&rows, t(1_000)).items[&item].labels;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].at, t(400 + MATCH_SLACK_SEC));
}

#[test]
fn a_merge_is_dated_by_the_webhook_when_it_has_one() {
    let item = pr(12);
    let rows = vec![
        lifecycle(Source::Daemon, "d", &item, Lifecycle::Merged, 1_000, 1_300),
        lifecycle(Source::Webhook, "w", &item, Lifecycle::Merged, 1_004, 1_004),
    ];
    let timeline = Timeline::build(&rows, t(2_000));
    assert_eq!(timeline.items[&item].merged_at(), Some(t(1_004)));
}

// -- rule 3: daemon time otherwise -------------------------------------------

#[test]
fn daemon_time_is_kept_when_no_webhook_row_matches() {
    let item = pr(20);
    let rows = vec![
        label_set("d1", &item, &[RR], Some(100)),
        label_set("d2", &item, &[APPROVED], Some(400)),
        // A webhook row for another label is no partner.
        label(Source::Webhook, "w", &item, "loom:building", Transition::Added, 390),
    ];
    let timeline = Timeline::build(&rows, t(1_000));
    let daemon: Vec<_> = timeline.items[&item]
        .labels
        .iter()
        .filter(|e| e.source == Source::Daemon)
        .map(|e| (e.label.as_str(), e.transition, e.at, e.corroborated))
        .collect();
    assert_eq!(
        daemon,
        vec![
            (APPROVED, Transition::Added, t(400), false),
            (RR, Transition::Removed, t(400), false),
        ]
    );
}

#[test]
fn a_daemon_items_first_label_set_is_a_baseline_and_dates_nothing() {
    let item = pr(21);
    let rows = vec![label_set("d1", &item, &[RR, "bug"], Some(100))];
    let timeline = Timeline::build(&rows, t(1_000));
    let i = timeline
        .items
        .get(&item)
        .expect("a baseline-only item is still an item (#10520)");
    assert!(i.labels.is_empty(), "the baseline dates nothing");
    let set = BTreeSet::from([RR.to_string(), "bug".to_string()]);
    assert_eq!(i.label_sets, vec![(t(100), set.clone())]);
    assert_eq!(i.labels_at(t(1_000)), BTreeSet::new(), "no recorded change");
    assert_eq!(i.current_labels(t(1_000)), set, "the baseline is what it carried");
    assert_eq!(i.current_labels(t(99)), BTreeSet::new(), "nothing knowable before it");
    assert_eq!(i.undated_baseline_labels(), set);
    assert_eq!(i.seen_at().collect::<Vec<_>>(), vec![t(100)]);
}

/// Repeated identical sets diff to nothing, yet the item and every sighting
/// are kept; a set observed after the cutoff is not.
#[test]
fn repeated_identical_label_sets_keep_the_item_and_every_sighting() {
    let item = pr(24);
    let rows = vec![
        label_set("d1", &item, &[RR], Some(100)),
        label_set("d2", &item, &[RR], Some(400)),
        label_set("d3", &item, &[RR], Some(700)),
        label_set("d4", &item, &[RR, APPROVED], Some(2_000)),
    ];
    let timeline = Timeline::build(&rows, t(1_000));
    let i = &timeline.items[&item];
    assert!(i.labels.is_empty());
    let seen: Vec<_> = i.label_sets.iter().map(|(at, _)| *at).collect();
    assert_eq!(seen, vec![t(100), t(400), t(700)], "point-in-time: d4 is not knowable");
    assert_eq!(i.current_labels(t(1_000)), BTreeSet::from([RR.to_string()]));
}

/// A baseline label a recorded change explains is dated; one only the set
/// carries is not, and the latest set plus later changes is the current set.
#[test]
fn a_baseline_label_is_dated_only_by_a_recorded_change() {
    let item = pr(25);
    let rows = vec![
        label(Source::Webhook, "w1", &item, RR, Transition::Added, 50),
        label_set("d1", &item, &[RR, "bug"], Some(100)),
        label(Source::Webhook, "w2", &item, RR, Transition::Removed, 300),
    ];
    let timeline = Timeline::build(&rows, t(1_000));
    let i = &timeline.items[&item];
    assert_eq!(i.undated_baseline_labels(), BTreeSet::from(["bug".to_string()]));
    assert_eq!(i.current_labels(t(1_000)), BTreeSet::from(["bug".to_string()]));
    assert_eq!(i.current_labels(t(200)), BTreeSet::from([RR.to_string(), "bug".to_string()]));
}

#[test]
fn a_daemon_only_merge_keeps_the_forge_instant_it_carried() {
    let item = pr(22);
    let rows = vec![lifecycle(
        Source::Daemon,
        "d",
        &item,
        Lifecycle::Merged,
        1_000,
        1_300,
    )];
    let timeline = Timeline::build(&rows, t(2_000));
    let merge = timeline.items[&item].resolution().unwrap();
    assert_eq!((merge.at, merge.source), (t(1_000), Source::Daemon));
}

#[test]
fn a_webhook_change_before_the_daemons_previous_sighting_is_not_its_partner() {
    let item = pr(23);
    // Added, removed, re-added: the webhook saw both adds, the daemon only
    // the second (its previous sighting of an add was the first one).
    let rows = vec![
        label(Source::Webhook, "w1", &item, RR, Transition::Added, 100),
        label(Source::Daemon, "d1", &item, RR, Transition::Added, 300),
        label(Source::Webhook, "w2", &item, RR, Transition::Removed, 500),
        label(Source::Webhook, "w3", &item, RR, Transition::Added, 700),
        label(Source::Daemon, "d3", &item, RR, Transition::Added, 900),
    ];
    let timeline = Timeline::build(&rows, t(2_000));
    let adds: Vec<_> = timeline.items[&item]
        .labels
        .iter()
        .filter(|e| e.transition == Transition::Added)
        .map(|e| (e.at, e.source, e.corroborated))
        .collect();
    assert_eq!(
        adds,
        vec![
            (t(100), Source::Webhook, true),
            (t(700), Source::Webhook, true)
        ],
        "each daemon sighting pairs with the change it saw"
    );
}

// -- rules 1 and 4: dedupe ----------------------------------------------------

#[test]
fn duplicates_collapse_per_repo_number_label_and_transition() {
    let item = pr(30);
    let mut redelivered = label(Source::Webhook, "w1", &item, RR, Transition::Added, 100);
    redelivered.observed_at = Some(t(150));
    let rows = vec![
        label(Source::Webhook, "w1", &item, RR, Transition::Added, 100),
        // The same record id again (at-least-once delivery).
        redelivered,
        // The same change under a new record id (a re-export).
        label(Source::Webhook, "w2", &item, RR, Transition::Added, 100),
        // The same change from the daemon.
        label(Source::Daemon, "d1", &item, RR, Transition::Added, 300),
        // The same label on another PR, and the opposite transition: kept.
        label(Source::Webhook, "w3", &pr(31), RR, Transition::Added, 100),
        label(Source::Webhook, "w4", &item, RR, Transition::Removed, 200),
    ];
    let timeline = Timeline::build(&rows, t(1_000));
    assert_eq!(timeline.items[&item].labels.len(), 2, "one add, one remove");
    assert_eq!(timeline.items[&pr(31)].labels.len(), 1);
    assert_eq!(timeline.stats.duplicate_records, 1);
    assert_eq!(timeline.stats.duplicate_events, 1);
    assert_eq!(timeline.stats.corroborated, 1);
}

#[test]
fn input_order_does_not_change_the_timeline() {
    let mut rows = fixture_rows(500);
    let forward = Timeline::build(&rows, t(30 * 86_400));
    rows.reverse();
    assert_eq!(Timeline::build(&rows, t(30 * 86_400)), forward);
}

// -- point in time --------------------------------------------------------------

#[test]
fn the_cutoff_keeps_a_row_observed_at_it_and_drops_later_and_unobserved_rows() {
    let item = pr(40);
    let rows = vec![
        label_set("d1", &item, &[RR], Some(100)),
        // Observed exactly at the cutoff: knowable.
        label_set("d2", &item, &[APPROVED], Some(500)),
        // Observed after it: not knowable, so the removal it implies is not
        // either.
        label_set("d3", &item, &[], Some(501)),
        // Never observed (a legacy row): never knowable.
        label_set("d4", &item, &["loom:building"], None),
        lifecycle(Source::Webhook, "w", &item, Lifecycle::Merged, 900, 900),
    ];
    let timeline = Timeline::build(&rows, t(500));
    let events = &timeline.items[&item];
    assert_eq!(events.labels_at(t(500)), BTreeSet::from([APPROVED.to_string()]));
    assert!(events.labels.iter().all(|e| e.label != "loom:building"));
    assert_eq!(events.merged_at(), None, "merged after the cutoff");
    assert_eq!(timeline.stats.not_knowable, 3);
}

#[test]
fn a_row_after_the_cutoff_cannot_change_a_dedupe_decision() {
    let item = pr(41);
    let rows = vec![
        label(Source::Daemon, "d", &item, RR, Transition::Added, 400),
        // The webhook row is later knowable than the cutoff.
        label(Source::Webhook, "w", &item, RR, Transition::Added, 600),
    ];
    let early = Timeline::build(&rows, t(500));
    assert_eq!(early.items[&item].labels[0].at, t(400), "the daemon's time, for now");
    assert_eq!(early.items[&item].labels[0].source, Source::Daemon);
}

#[test]
fn the_cutoff_picks_the_queue_and_ci_that_were_knowable_then() {
    let rows = fixture_rows(500);
    // The second snapshot ticked at 1200 but was knowable at 1205.
    let timeline = Timeline::build(&rows, t(1_204));
    assert_eq!(timeline.queue.as_ref().unwrap().tick_at, t(600));
    assert_eq!(timeline.queue_entry(899).unwrap().state.as_deref(), Some("ready"));
    let ci = timeline.ci_for_ref(REPO, "feature/issue-899").unwrap();
    assert_eq!(ci.latest["CI"].run.run_attempt, 1, "the rerun was not yet observed");
    assert!(!ci.all_green());
}

fn parse(row: serde_json::Value) -> ParsedRow {
    parse_row(&row.to_string(), REPO).unwrap().1
}

fn admitted(row: serde_json::Value) -> Row {
    match parse(row) {
        ParsedRow::Admitted(row) => *row,
        other => panic!("not admitted: {other:?}"),
    }
}

fn ns(at: DateTime<Utc>) -> String {
    at.timestamp_nanos_opt().unwrap().to_string()
}

#[test]
fn a_webhook_row_is_knowable_at_its_receipt_and_a_daemon_row_at_its_observation() {
    let payload = json!({"target": "pr", "number": 7, "action": "labeled", "label": RR,
                         "at": "2026-10-01T10:00:00Z"});
    let webhook = admitted(json!({
        "record_id": "h:1", "kind": "label.transition", "service": SERVICE_WEBHOOK,
        "repo": REPO, "attrs": "{}", "nums": "{}",
        "body": json!({"id": 1, "payload": payload.to_string()}).to_string(),
        "event_time_ns": ns(t(0)), "knowable_time_ns": ns(t(4 * 86_400)),
    }));
    assert_eq!(webhook.source, Source::Webhook);
    assert_eq!(webhook.observed_at, Some(t(0)), "receipt, not the later export");

    let daemon = |body: serde_json::Value, knowable: i64| {
        admitted(json!({
            "record_id": "d:1", "kind": "label.transition", "service": "loom",
            "repo": REPO, "attrs": "{}", "nums": "{}", "body": body.to_string(),
            "event_time_ns": ns(t(0)), "knowable_time_ns": knowable.to_string(),
        }))
    };
    let journal = json!({"observed_at": "2026-10-01T10:05:00Z", "pr_number": 7,
                         "raw": {"labels": [RR]}});
    assert_eq!(daemon(journal.clone(), 0).observed_at, Some(t(300)), "its own field");
    let mut bare = journal;
    bare.as_object_mut().unwrap().remove("observed_at");
    let ingest = t(360).timestamp_nanos_opt().unwrap();
    assert_eq!(daemon(bare.clone(), ingest).observed_at, Some(t(360)), "else ingest");
    assert_eq!(daemon(bare, 0).observed_at, None, "else never knowable");
}

#[test]
fn a_row_observed_before_its_own_event_is_rejected() {
    let row = parse(json!({
        "record_id": "d:r", "kind": "pr.resolved", "service": "loom-daemon", "repo": REPO,
        "attrs": json!({"loom.eta.pr.state": "merged",
                        "loom.eta.pr.resolved_at": "2026-10-01T10:10:00Z",
                        "loom.eta.pr.observed_at": "2026-10-01T10:00:00Z"}).to_string(),
        "nums": json!({"loom.pr_number": 9}).to_string(), "body": "pr.resolved",
        "event_time_ns": ns(t(600)), "knowable_time_ns": ns(t(0)),
    }));
    assert_eq!(row, ParsedRow::Rejected(reject::KNOWABLE_BEFORE_EVENT));
}

#[test]
fn a_reopen_after_a_close_leaves_the_item_open() {
    let item = pr(50);
    let rows = vec![
        lifecycle(Source::Webhook, "w1", &item, Lifecycle::Closed, 100, 100),
        lifecycle(Source::Webhook, "w2", &item, Lifecycle::Reopened, 200, 200),
    ];
    let timeline = Timeline::build(&rows, t(300));
    assert_eq!(timeline.items[&item].closed_at(), None);
    assert_eq!(Timeline::build(&rows, t(150)).items[&item].closed_at(), Some(t(100)));
}

// -- coverage, parameterised on the fit window ----------------------------------

/// `ci.run` / `queue.snapshot` reach SigNoz from 2026-09-28; the window is
/// [`WINDOW_DAYS`] (60 on main), not 14.
#[test]
fn ci_and_queue_cover_a_fit_window_only_once_it_lies_wholly_after_the_first_row() {
    let first = Utc.with_ymd_and_hms(2026, 9, 28, 0, 0, 0).unwrap();
    let rows = vec![Row {
        record_id: "c".to_string(),
        repo: REPO.to_string(),
        source: Source::Daemon,
        observed_at: Some(first),
        body: RowBody::Queue {
            tick_at: first,
            entries: Vec::new(),
        },
    }];
    let covered_from = first + Duration::days(WINDOW_DAYS);
    let coverage = |cutoff| Timeline::build(&rows, cutoff).coverage;
    assert!(coverage(covered_from).covers(Family::Queue, covered_from, WINDOW_DAYS));
    let before = covered_from - Duration::seconds(1);
    assert!(!coverage(before).covers(Family::Queue, before, WINDOW_DAYS));
    // The 14-day figure in #10511's table would claim coverage 46 days early
    // (this fails if WINDOW_DAYS ever drops to 14 or below).
    let fourteen = first + Duration::days(14);
    assert!(!coverage(fourteen).covers(Family::Queue, fourteen, WINDOW_DAYS));
    assert!(!coverage(covered_from).covers(Family::Ci, covered_from, WINDOW_DAYS));
}

#[test]
fn coverage_is_per_family_and_source_and_only_counts_knowable_rows() {
    let rows = fixture_rows(500);
    let cutoff = t(30 * 86_400);
    let timeline = Timeline::build(&rows, cutoff);
    let coverage = &timeline.coverage;
    assert_eq!(coverage.earliest[&(Family::Labels, Source::Webhook)], t(-600));
    assert_eq!(coverage.earliest[&(Family::Labels, Source::Daemon)], t(100));
    assert_eq!(coverage.first(Family::Lifecycle), Some(t(2_000)));
    assert_eq!(coverage.first(Family::Ci), Some(t(520)));
    assert!(!coverage.covers(Family::Ci, cutoff, WINDOW_DAYS), "30 days of 60");
    // Before anything was knowable there is no coverage at all.
    assert_eq!(
        Timeline::build(&rows, t(-601))
            .coverage
            .first(Family::Labels),
        None
    );
}
