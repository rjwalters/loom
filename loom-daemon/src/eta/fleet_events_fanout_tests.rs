#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::eta::fleet_events::{load_events, PageFetch};
use chrono::Duration;
use std::collections::HashMap;

const REPO: &str = "rjwalters/loom";

fn t(secs: i64) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
        + Duration::seconds(secs)
}

/// The fixed fetch time and `now`, so two runs compare byte for byte.
fn now() -> DateTime<Utc> {
    t(1_000_000)
}

fn pr_row(pr: u32, kind: EventKind, label: Option<&str>, secs: i64, seq: u64) -> RawEvent {
    RawEvent::new(
        REPO,
        pr,
        ItemKind::Pr,
        kind,
        label.map(str::to_string),
        t(secs),
        SOURCE_FORGE,
        seq,
        now(),
    )
}

fn review(pr: u32, state: &str, secs: i64, id: u64) -> RawEvent {
    pr_row(pr, EventKind::Review, Some(state), secs, id)
}

/// PRs 1, 2 and 4 closed; PR 3 open; heads recorded for 1 and 3.
fn cache() -> Vec<RawEvent> {
    vec![
        pr_row(1, EventKind::Opened, None, 0, 0),
        pr_row(1, EventKind::HeadCommit, Some("aaa"), 90, 0),
        pr_row(1, EventKind::Merged, None, 100, 0),
        pr_row(2, EventKind::Opened, None, 10, 0),
        pr_row(2, EventKind::Closed, None, 200, 0),
        pr_row(3, EventKind::Opened, None, 20, 0),
        pr_row(3, EventKind::HeadCommit, Some("ccc"), 30, 0),
        pr_row(4, EventKind::Opened, None, 30, 0),
        pr_row(4, EventKind::Closed, None, 400, 0),
    ]
}

/// Per-PR review listings, oldest first, `per_page` rows a page; fails (as a
/// rate-limit stop) once `fail_after` pages have been served in total.
struct FakeReviews {
    rows: HashMap<u32, Vec<RawEvent>>,
    per_page: usize,
    pr: u32,
    served: u64,
    fail_after: Option<u64>,
    requests: Vec<(u32, u32, Option<String>)>,
}

impl FakeReviews {
    fn new() -> Self {
        let mut rows = HashMap::new();
        rows.insert(1, vec![review(1, "approved", 50, 11)]);
        rows.insert(2, Vec::new());
        rows.insert(
            3,
            vec![
                review(3, "commented", 25, 31),
                review(3, "approved", 26, 32),
            ],
        );
        rows.insert(
            4,
            (0..5)
                .map(|i| review(4, "commented", 40 + i, 40 + i as u64))
                .collect(),
        );
        FakeReviews {
            rows,
            per_page: 2,
            pr: 0,
            served: 0,
            fail_after: None,
            requests: Vec::new(),
        }
    }

    fn etag(&self) -> String {
        format!("W/\"pr{}-n{}\"", self.pr, self.rows[&self.pr].len())
    }
}

impl RawEventSource for FakeReviews {
    fn cursor_key(&self) -> String {
        PerPrKind::Reviews.item_key(self.pr, None)
    }

    fn fetch_page(&mut self, page: u32, etag: Option<&str>) -> PageFetch {
        self.requests
            .push((self.pr, page, etag.map(str::to_string)));
        if self.fail_after.is_some_and(|n| self.served >= n) {
            return PageFetch::Stopped("rate limited".to_string());
        }
        self.served += 1;
        if page == 1 && etag == Some(self.etag().as_str()) {
            return PageFetch::NotModified;
        }
        let rows = &self.rows[&self.pr];
        let start = (page as usize - 1) * self.per_page;
        PageFetch::Page {
            events: rows
                .iter()
                .skip(start)
                .take(self.per_page)
                .cloned()
                .collect(),
            etag: (page == 1).then(|| self.etag()),
            last: start + self.per_page >= rows.len(),
        }
    }

    fn newest_first(&self) -> bool {
        false
    }
}

impl PerPrSource for FakeReviews {
    fn select(&mut self, pr: u32, _sha: Option<&str>) {
        self.pr = pr;
    }
}

struct Run {
    report: FanoutReport,
    requests: Vec<(u32, u32, Option<String>)>,
}

/// One run in `dir` as a fresh process would do it: everything re-read from
/// disk, the work list recomputed from the events file.
fn run(dir: &Path, source: &mut FakeReviews, max_pages: u64) -> Run {
    run_on(dir, source, max_pages, &cache())
}

fn run_on(dir: &Path, source: &mut FakeReviews, max_pages: u64, seed: &[RawEvent]) -> Run {
    let events = dir.join("events.jsonl");
    let cursor_file = dir.join("events.cursor.json");
    if !events.exists() {
        EventLog::open(&events).unwrap().append(seed).unwrap();
    }
    let mut log = EventLog::open(&events).unwrap();
    let mut cursor = EventsCursor::read(&cursor_file, REPO);
    let work = pr_work(&load_events(&events), REPO);
    source.requests.clear();
    let report = sync_per_pr(
        PerPrKind::Reviews,
        source,
        &work,
        &mut log,
        &mut cursor,
        &cursor_file,
        max_pages,
        now(),
    )
    .unwrap();
    Run {
        report,
        requests: source.requests.clone(),
    }
}

fn bytes(dir: &Path) -> (Vec<u8>, Vec<u8>) {
    (
        std::fs::read(dir.join("events.jsonl")).unwrap(),
        std::fs::read(dir.join("events.cursor.json")).unwrap(),
    )
}

#[test]
fn the_work_list_reads_state_heads_and_settlement_from_the_cache() {
    let mut events = cache();
    events.push(pr_row(1, EventKind::HeadCommit, Some("bbb"), 95, 0));
    events.push(settle_marker(REPO, 2, PerPrKind::Reviews, t(200), now()));
    // A marker for an earlier close does not settle a later one.
    events.push(pr_row(4, EventKind::Reopened, None, 500, 0));
    events.push(pr_row(4, EventKind::Closed, None, 600, 0));
    events.push(settle_marker(REPO, 4, PerPrKind::Reviews, t(400), now()));
    let work = pr_work(&events, REPO);
    let by = |n: u32| work.iter().find(|w| w.pr == n).unwrap();
    assert_eq!(by(1).closed_at, Some(t(100)));
    assert_eq!(by(1).head.as_deref(), Some("bbb"), "the latest head wins");
    assert!(by(2).reviews_settled && !by(2).checks_settled);
    assert!(!by(2).pending(PerPrKind::Reviews));
    assert!(!by(2).pending(PerPrKind::CheckRuns), "no head: nothing to read");
    assert_eq!(by(3).closed_at, None);
    assert!(by(3).pending(PerPrKind::Reviews));
    assert_eq!(by(4).closed_at, Some(t(600)));
    assert!(!by(4).reviews_settled);
}

#[test]
fn a_full_walk_settles_closed_prs_and_keeps_only_open_ones_in_the_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = FakeReviews::new();
    let first = run(dir.path(), &mut source, 100);
    assert_eq!(first.report.outcome, SyncOutcome::Complete);
    assert_eq!(first.report.settled, 3);
    assert_eq!(first.report.remaining, 0);
    // Open PR 3 first, then closed PRs newest-numbered first; PR 4 spans
    // three pages.
    let order: Vec<(u32, u32)> = first.requests.iter().map(|(p, n, _)| (*p, *n)).collect();
    assert_eq!(order, vec![(3, 1), (4, 1), (4, 2), (4, 3), (2, 1), (1, 1)]);
    let cursor = EventsCursor::read(&dir.path().join("events.cursor.json"), REPO);
    let keys: Vec<&str> = cursor.endpoints.keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["forge:reviews", "forge:reviews#3"]);
    assert_eq!(cursor.endpoints["forge:reviews"].pages_fetched, 6);
    let events = load_events(&dir.path().join("events.jsonl"));
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == EventKind::Review)
            .count(),
        8 + 3
    );

    // A second run reads only the open PR, conditionally: one 304.
    let second = run(dir.path(), &mut source, 100);
    assert_eq!(second.report.pages, 1);
    assert_eq!(second.report.appended, 0);
    assert_eq!(second.requests, vec![(3, 1, Some(source.etag_for(3)))]);
}

impl FakeReviews {
    fn etag_for(&mut self, pr: u32) -> String {
        self.pr = pr;
        self.etag()
    }
}

#[test]
fn a_killed_walk_resumes_without_rereading_and_is_byte_identical() {
    // With an open PR in the mix (it is re-polled, conditionally, on every
    // run by design, so only the events file is compared) and without one
    // (the cursor, call ledger included, is byte-identical too).
    let closed_only: Vec<RawEvent> = cache().into_iter().filter(|e| e.item != 3).collect();
    for (seed, compare_cursor) in [(cache(), false), (closed_only, true)] {
        let straight = tempfile::tempdir().unwrap();
        let total = run_on(straight.path(), &mut FakeReviews::new(), 100, &seed)
            .report
            .pages;
        for fail_after in 1..total {
            let interrupted = tempfile::tempdir().unwrap();
            let mut source = FakeReviews::new();
            source.fail_after = Some(fail_after);
            let first = run_on(interrupted.path(), &mut source, 100, &seed);
            assert!(matches!(first.report.outcome, SyncOutcome::Stopped(_)), "{fail_after}");
            let served: Vec<(u32, u32)> = first.requests[..first.requests.len() - 1]
                .iter()
                .map(|(p, n, _)| (*p, *n))
                .collect();
            source.fail_after = None;
            let second = run_on(interrupted.path(), &mut source, 100, &seed);
            assert_eq!(second.report.outcome, SyncOutcome::Complete);
            // Only an open PR's conditional poll may touch a page again.
            for (pr, page, etag) in &second.requests {
                assert!(
                    etag.is_some() || !served.contains(&(*pr, *page)),
                    "re-read PR {pr} page {page} (stopped after {fail_after})"
                );
            }
            let (a, b) = (bytes(straight.path()), bytes(interrupted.path()));
            assert_eq!(a.0, b.0, "events differ (stopped after {fail_after})");
            if compare_cursor {
                assert_eq!(a.1, b.1, "cursor differs (stopped after {fail_after})");
            }
        }
    }
}

#[test]
fn a_kill_between_the_settle_walk_and_its_marker_rereads_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let events = dir.path().join("events.jsonl");
    let cursor_file = dir.path().join("events.cursor.json");
    let mut seeded = cache();
    seeded.push(review(1, "approved", 50, 11));
    EventLog::open(&events).unwrap().append(&seeded).unwrap();
    let mut cursor = EventsCursor::empty(REPO);
    cursor.endpoints.insert(
        PerPrKind::Reviews.item_key(1, None),
        EndpointCursor {
            backfill_next_page: 2,
            backfill_complete: true,
            settle_for: Some(t(100)),
            pages_fetched: 1,
            ..EndpointCursor::default()
        },
    );
    cursor.write(&cursor_file).unwrap();
    let mut source = FakeReviews::new();
    let mut log = EventLog::open(&events).unwrap();
    let work: Vec<PrWork> = pr_work(&load_events(&events), REPO)
        .into_iter()
        .filter(|w| w.pr == 1)
        .collect();
    let report = sync_per_pr(
        PerPrKind::Reviews,
        &mut source,
        &work,
        &mut log,
        &mut cursor,
        &cursor_file,
        100,
        now(),
    )
    .unwrap();
    assert_eq!(report.pages, 0);
    assert_eq!(report.settled, 1);
    assert!(source.requests.is_empty());
    assert!(pr_work(&load_events(&events), REPO)[0].reviews_settled);
}

#[test]
fn the_page_budget_is_shared_by_every_pr_of_the_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = FakeReviews::new();
    let first = run(dir.path(), &mut source, 3);
    assert_eq!(first.report.outcome, SyncOutcome::PageBudget);
    assert_eq!(first.report.pages, 3);
    // PR 3 (open) read; PR 4 two pages in; nothing settled yet.
    assert_eq!((first.report.settled, first.report.remaining), (0, 3));
    let second = run(dir.path(), &mut source, 100);
    assert_eq!(second.report.outcome, SyncOutcome::Complete);
    assert_eq!(second.requests[1], (4, 3, None), "PR 4 resumes at page 3");
    assert_eq!(second.report.remaining, 0);
}

#[test]
fn an_open_pr_that_closes_is_read_once_more_in_full() {
    let dir = tempfile::tempdir().unwrap();
    let mut source = FakeReviews::new();
    run(dir.path(), &mut source, 100);
    // PR 3 gets a review and closes.
    source
        .rows
        .get_mut(&3)
        .unwrap()
        .push(review(3, "changes_requested", 27, 33));
    EventLog::open(&dir.path().join("events.jsonl"))
        .unwrap()
        .append(&[pr_row(3, EventKind::Closed, None, 700, 0)])
        .unwrap();
    let r = run(dir.path(), &mut source, 100);
    assert_eq!(r.requests, vec![(3, 1, None), (3, 2, None)], "unconditional, to the end");
    assert_eq!(r.report.settled, 1);
    let cursor = EventsCursor::read(&dir.path().join("events.cursor.json"), REPO);
    assert_eq!(cursor.endpoints.keys().collect::<Vec<_>>(), vec!["forge:reviews"]);
    let events = load_events(&dir.path().join("events.jsonl"));
    assert!(events
        .iter()
        .any(|e| e.label.as_deref() == Some("changes_requested")));
}

#[test]
fn a_moved_head_drops_the_old_check_runs_key() {
    let dir = tempfile::tempdir().unwrap();
    let cursor_file = dir.path().join("c.json");
    let mut cursor = EventsCursor::empty(REPO);
    cursor
        .endpoints
        .insert(PerPrKind::CheckRuns.item_key(3, Some("old")), EndpointCursor::default());
    cursor
        .endpoints
        .insert("forge:pulls".to_string(), EndpointCursor::default());
    let work = vec![PrWork {
        pr: 3,
        closed_at: None,
        head: Some("ccc".to_string()),
        reviews_settled: false,
        checks_settled: false,
    }];
    let mut log = EventLog::open(&dir.path().join("e.jsonl")).unwrap();
    let mut source = FakeReviews::new();
    sync_per_pr(
        PerPrKind::CheckRuns,
        &mut source,
        &work,
        &mut log,
        &mut cursor,
        &cursor_file,
        0,
        now(),
    )
    .unwrap();
    assert!(cursor.endpoints.contains_key("forge:pulls"), "other endpoints untouched");
    assert!(!cursor.endpoints.contains_key("forge:check-runs#3@old"));
    assert_eq!(PerPrKind::CheckRuns.item_key(3, Some("ccc")), "forge:check-runs#3@ccc");
}
