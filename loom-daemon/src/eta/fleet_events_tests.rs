#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use chrono::Duration;

const REPO: &str = "rjwalters/loom";

fn t(secs: i64) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
        + Duration::seconds(secs)
}

/// The fixed fetch time every fake page reports, so two runs are comparable
/// byte for byte.
fn fetched() -> DateTime<Utc> {
    t(1_000_000)
}

fn label(item: u32, name: &str, secs: i64, seq: u64) -> RawEvent {
    RawEvent::new(
        REPO,
        item,
        ItemKind::Issue,
        EventKind::LabelAdded,
        Some(name.to_string()),
        t(secs),
        SOURCE_FORGE,
        seq,
        fetched(),
    )
}

/// A newest-first paged listing over `rows`, optionally failing (as a
/// rate-limit stop) once `fail_after` pages have been served.
struct FakeSource {
    /// Newest first.
    rows: Vec<RawEvent>,
    per_page: usize,
    generation: u32,
    served: u64,
    fail_after: Option<u64>,
    requests: Vec<(u32, Option<String>)>,
}

impl FakeSource {
    fn new(mut rows: Vec<RawEvent>, per_page: usize) -> Self {
        rows.sort_by_key(|r| std::cmp::Reverse(r.seq));
        FakeSource {
            rows,
            per_page,
            generation: 1,
            served: 0,
            fail_after: None,
            requests: Vec::new(),
        }
    }

    fn push_head(&mut self, event: RawEvent) {
        self.rows.insert(0, event);
        self.generation += 1;
    }

    fn head_etag(&self) -> String {
        format!("W/\"g{}\"", self.generation)
    }
}

impl RawEventSource for FakeSource {
    fn cursor_key(&self) -> String {
        "forge:fake".to_string()
    }

    fn fetch_page(&mut self, page: u32, etag: Option<&str>) -> PageFetch {
        self.requests.push((page, etag.map(str::to_string)));
        if self.fail_after.is_some_and(|n| self.served >= n) {
            return PageFetch::Stopped("rate limited".to_string());
        }
        self.served += 1;
        if page == 1 && etag == Some(self.head_etag().as_str()) {
            return PageFetch::NotModified;
        }
        let start = (page as usize - 1) * self.per_page;
        let events: Vec<RawEvent> = self
            .rows
            .iter()
            .skip(start)
            .take(self.per_page)
            .cloned()
            .collect();
        PageFetch::Page {
            last: start + self.per_page >= self.rows.len(),
            etag: (page == 1).then(|| self.head_etag()),
            events,
        }
    }
}

fn history(n: u64) -> Vec<RawEvent> {
    (1..=n)
        .map(|i| label(u32::try_from(i % 7).unwrap() + 1, "loom:issue", i as i64 * 60, 100 + i))
        .collect()
}

/// Run a backfill to completion in `dir`, failing after `fail_after` pages on
/// the first attempt. Returns the bytes of the events file and the cursor.
fn backfill(dir: &Path, fail_after: Option<u64>) -> (Vec<u8>, Vec<u8>) {
    let events = dir.join("events.jsonl");
    let cursor_file = dir.join("events.cursor.json");
    let mut source = FakeSource::new(history(23), 5);
    source.fail_after = fail_after;
    let mut log = EventLog::open(&events).unwrap();
    let mut cursor = EventsCursor::read(&cursor_file, REPO);
    let first =
        sync(&mut source, &mut log, &mut cursor, &cursor_file, SyncMode::Backfill, 100).unwrap();
    if fail_after.is_some() {
        assert!(matches!(first.outcome, SyncOutcome::Stopped(_)), "{first:?}");
        // A fresh process: everything is re-read from disk.
        let resumed_from =
            EventsCursor::read(&cursor_file, REPO).endpoints["forge:fake"].backfill_next_page;
        source.fail_after = None;
        source.requests.clear();
        let mut log = EventLog::open(&events).unwrap();
        let mut cursor = EventsCursor::read(&cursor_file, REPO);
        let second =
            sync(&mut source, &mut log, &mut cursor, &cursor_file, SyncMode::Backfill, 100)
                .unwrap();
        assert_eq!(second.outcome, SyncOutcome::Complete);
        // Resumed where it stopped: no already-cached page was re-read.
        assert_eq!(source.requests[0].0, resumed_from);
        assert!(source.requests.iter().all(|(p, _)| *p >= resumed_from));
    } else {
        assert_eq!(first.outcome, SyncOutcome::Complete);
    }
    (std::fs::read(&events).unwrap(), std::fs::read(&cursor_file).unwrap())
}

#[test]
fn a_killed_backfill_resumes_without_rereading_and_is_byte_identical() {
    let straight = tempfile::tempdir().unwrap();
    let interrupted = tempfile::tempdir().unwrap();
    let a = backfill(straight.path(), None);
    let b = backfill(interrupted.path(), Some(2));
    assert_eq!(a.0, b.0, "events file differs after a resumed backfill");
    assert_eq!(a.1, b.1, "cursor differs after a resumed backfill");
    assert_eq!(load_events(&straight.path().join("events.jsonl")).len(), 23);
}

#[test]
fn a_torn_trailing_line_is_truncated_and_the_rerun_is_byte_identical() {
    let straight = tempfile::tempdir().unwrap();
    let a = backfill(straight.path(), None);

    let torn = tempfile::tempdir().unwrap();
    let events = torn.path().join("events.jsonl");
    let cursor_file = torn.path().join("events.cursor.json");
    // A first page landed, then a kill mid-append of the second, before its
    // cursor checkpoint.
    let mut source = FakeSource::new(history(23), 5);
    source.fail_after = Some(1);
    let mut log = EventLog::open(&events).unwrap();
    let mut cursor = EventsCursor::empty(REPO);
    sync(&mut source, &mut log, &mut cursor, &cursor_file, SyncMode::Backfill, 100).unwrap();
    let mut bytes = std::fs::read(&events).unwrap();
    bytes.extend_from_slice(b"{\"schema\":\"eta-fleet-event/v1\",\"id\":\"trunc");
    std::fs::write(&events, bytes).unwrap();

    source.fail_after = None;
    let mut log = EventLog::open(&events).unwrap();
    let mut cursor = EventsCursor::read(&cursor_file, REPO);
    sync(&mut source, &mut log, &mut cursor, &cursor_file, SyncMode::Backfill, 100).unwrap();
    assert_eq!(std::fs::read(&events).unwrap(), a.0);
}

#[test]
fn a_page_budget_stops_cleanly_and_the_next_run_continues() {
    let dir = tempfile::tempdir().unwrap();
    let events = dir.path().join("e.jsonl");
    let cursor_file = dir.path().join("c.json");
    let mut source = FakeSource::new(history(23), 5);
    let mut log = EventLog::open(&events).unwrap();
    let mut cursor = EventsCursor::empty(REPO);
    let first =
        sync(&mut source, &mut log, &mut cursor, &cursor_file, SyncMode::Backfill, 2).unwrap();
    assert_eq!(first.outcome, SyncOutcome::PageBudget);
    assert_eq!(first.pages, 2);
    let second =
        sync(&mut source, &mut log, &mut cursor, &cursor_file, SyncMode::Backfill, 100).unwrap();
    assert_eq!(second.outcome, SyncOutcome::Complete);
    assert_eq!(second.pages, 3);
    assert_eq!(cursor.endpoints["forge:fake"].pages_fetched, 5);
}

#[test]
fn reimporting_the_same_rows_appends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.jsonl");
    let rows = history(10);
    let mut log = EventLog::open(&path).unwrap();
    assert_eq!(log.append(&rows).unwrap(), 10);
    let before = std::fs::read(&path).unwrap();
    // Same content, a later fetch: the same ids, so nothing new.
    let refetched: Vec<RawEvent> = rows
        .iter()
        .map(|e| RawEvent {
            fetched_at: e.fetched_at + Duration::hours(5),
            ..e.clone()
        })
        .collect();
    let mut reopened = EventLog::open(&path).unwrap();
    assert_eq!(reopened.append(&refetched).unwrap(), 0);
    assert_eq!(reopened.append(&rows).unwrap(), 0);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn the_id_ignores_fetch_time_but_not_content() {
    let a = label(1, "loom:issue", 10, 5);
    let mut b = a.clone();
    b.fetched_at = t(99);
    assert_eq!(
        a.id,
        RawEvent::new(
            REPO,
            1,
            ItemKind::Issue,
            EventKind::LabelAdded,
            Some("loom:issue".into()),
            t(10),
            SOURCE_FORGE,
            5,
            t(99)
        )
        .id
    );
    assert_ne!(a.id, label(1, "loom:issue", 10, 6).id);
    assert_ne!(a.id, label(1, "loom:curated", 10, 5).id);
}

#[test]
fn same_second_events_sort_by_sequence_whatever_the_input_order() {
    let rows = vec![
        label(1, "c", 10, 30),
        label(1, "a", 10, 10),
        label(1, "b", 10, 20),
        label(2, "z", 5, 99),
    ];
    let mut forward = rows.clone();
    let mut backward: Vec<RawEvent> = rows.into_iter().rev().collect();
    canonicalize(&mut forward);
    canonicalize(&mut backward);
    assert_eq!(forward, backward);
    let order: Vec<&str> = forward
        .iter()
        .map(|e| e.label.as_deref().unwrap())
        .collect();
    assert_eq!(order, ["z", "a", "b", "c"]);
}

#[test]
fn refresh_sends_the_head_etag_and_a_304_reads_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let events = dir.path().join("e.jsonl");
    let cursor_file = dir.path().join("c.json");
    let mut source = FakeSource::new(history(12), 5);
    let mut log = EventLog::open(&events).unwrap();
    let mut cursor = EventsCursor::empty(REPO);
    sync(&mut source, &mut log, &mut cursor, &cursor_file, SyncMode::Backfill, 100).unwrap();
    source.requests.clear();
    let report =
        sync(&mut source, &mut log, &mut cursor, &cursor_file, SyncMode::Refresh, 100).unwrap();
    assert_eq!(report.pages, 1);
    assert_eq!(report.appended, 0);
    assert_eq!(source.requests, vec![(1, Some(source.head_etag()))]);
}

#[test]
fn refresh_reads_from_the_head_until_it_meets_cached_rows() {
    let dir = tempfile::tempdir().unwrap();
    let events = dir.path().join("e.jsonl");
    let cursor_file = dir.path().join("c.json");
    let mut source = FakeSource::new(history(12), 5);
    let mut log = EventLog::open(&events).unwrap();
    let mut cursor = EventsCursor::empty(REPO);
    sync(&mut source, &mut log, &mut cursor, &cursor_file, SyncMode::Backfill, 100).unwrap();
    for i in 0..7 {
        source.push_head(label(3, "loom:building", 10_000 + i, 1000 + i as u64));
    }
    source.requests.clear();
    let report =
        sync(&mut source, &mut log, &mut cursor, &cursor_file, SyncMode::Refresh, 100).unwrap();
    assert_eq!(report.appended, 7);
    // Page 2 holds the first cached row; the walk stops there.
    assert_eq!(source.requests.len(), 2);
    assert_eq!(
        cursor.endpoints["forge:fake"].head_etag.as_deref(),
        Some(source.head_etag().as_str())
    );
    assert_eq!(load_events(&events).len(), 19);
}

#[test]
fn an_interrupted_refresh_never_promotes_its_etag() {
    let dir = tempfile::tempdir().unwrap();
    let events = dir.path().join("e.jsonl");
    let cursor_file = dir.path().join("c.json");
    let mut source = FakeSource::new(history(12), 5);
    let mut log = EventLog::open(&events).unwrap();
    let mut cursor = EventsCursor::empty(REPO);
    sync(&mut source, &mut log, &mut cursor, &cursor_file, SyncMode::Backfill, 100).unwrap();
    let old_etag = cursor.endpoints["forge:fake"].head_etag.clone();
    for i in 0..12 {
        source.push_head(label(4, "loom:pr", 20_000 + i, 2000 + i as u64));
    }
    source.served = 0;
    source.fail_after = Some(1);
    let stopped =
        sync(&mut source, &mut log, &mut cursor, &cursor_file, SyncMode::Refresh, 100).unwrap();
    assert!(matches!(stopped.outcome, SyncOutcome::Stopped(_)));
    let state = &EventsCursor::read(&cursor_file, REPO).endpoints["forge:fake"];
    // Page 1 is cached but pages 2–3 are not: a 304 on page 1 now would hide
    // them, so the old validator stays and the walk resumes at page 2.
    assert_eq!(state.head_etag, old_etag);
    assert_eq!(state.refresh_next_page, Some(2));

    source.fail_after = None;
    source.requests.clear();
    let mut cursor = EventsCursor::read(&cursor_file, REPO);
    let done =
        sync(&mut source, &mut log, &mut cursor, &cursor_file, SyncMode::Refresh, 100).unwrap();
    assert_eq!(done.outcome, SyncOutcome::Complete);
    assert_eq!(source.requests[0], (2, None));
    assert_eq!(load_events(&events).len(), 24);
}

#[test]
fn paths_live_beside_the_snapshots() {
    let root = Path::new("/tmp/ws");
    let events = events_path(root, "RJWalters/Loom");
    let cursor = cursor_path(root, "RJWalters/Loom");
    assert!(events.ends_with("events-rjwalters-loom.jsonl"), "{}", events.display());
    assert!(cursor.ends_with("events-rjwalters-loom.cursor.json"), "{}", cursor.display());
    assert_eq!(events.parent(), Some(super::super::fleet::snapshot_dir(root).as_path()));
}
