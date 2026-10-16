//! Tests for the stage-entry provider: the three sources in order, the
//! events budget, the persisted cache, and pruning.

use super::*;
use std::collections::BTreeMap;

const REPO: &str = "acme/widgets";

fn t(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn now() -> DateTime<Utc> {
    t("2026-10-14T12:00:00Z")
}

fn item(number: u32, labels: &[&str], created: Option<&str>) -> RestIssue {
    let labels: Vec<_> = labels
        .iter()
        .map(|l| serde_json::json!({ "name": l }))
        .collect();
    let mut row = serde_json::json!({ "number": number, "state": "open", "labels": labels });
    if let Some(c) = created {
        row["created_at"] = serde_json::json!(c);
    }
    crate::forge_listing::parse_rest_issues(&serde_json::json!([row]).to_string())
        .unwrap()
        .remove(0)
}

fn updated(mut row: RestIssue, at: &str) -> RestIssue {
    row.updated_at = Some(at.to_string());
    row
}

fn with_labels_at(mut row: RestIssue, times: &[(&str, &str)]) -> RestIssue {
    row.labels_at = Some(
        times
            .iter()
            .map(|(l, at)| ((*l).to_string(), (*at).to_string()))
            .collect(),
    );
    row
}

/// A fake events read: canned label times per item, every read counted.
/// Each read costs `pages` pages (`0` is taken as 1).
#[derive(Default)]
struct FakeEvents {
    times: BTreeMap<u32, BTreeMap<String, DateTime<Utc>>>,
    fail: bool,
    pages: usize,
    reads: Vec<u32>,
    /// The `max_pages` each read was allowed.
    allowed: Vec<usize>,
}

impl FakeEvents {
    fn with(mut self, number: u32, label: &str, at: &str) -> Self {
        self.times
            .entry(number)
            .or_default()
            .insert(label.to_string(), t(at));
        self
    }
}

impl EventsSource for FakeEvents {
    fn label_times(
        &mut self,
        _root: &Path,
        slug: &str,
        number: u32,
        _label: &str,
        max_pages: usize,
    ) -> LabelTimes {
        assert_eq!(slug, REPO);
        self.reads.push(number);
        self.allowed.push(max_pages);
        let pages = self.pages.max(1).min(max_pages);
        if self.fail {
            return LabelTimes { times: None, pages };
        }
        LabelTimes {
            times: Some(self.times.get(&number).cloned().unwrap_or_default()),
            pages,
        }
    }
}

fn provider(budget: usize) -> StageEntryProvider {
    StageEntryProvider::load(None, budget)
}

fn ask(
    p: &mut StageEntryProvider,
    f: &mut FakeEvents,
    row: &RestIssue,
    label: &str,
) -> Option<StageEntry> {
    p.entry(f, Path::new("/nonexistent"), REPO, row, label, now())
}

#[test]
fn the_mirror_answers_first_and_costs_no_read() {
    let row = with_labels_at(
        item(1, &["loom:issue"], Some("2026-10-01T00:00:00Z")),
        &[("Loom:Issue", "2026-10-10T00:00:00Z")],
    );
    let mut f = FakeEvents::default().with(1, "loom:issue", "2026-10-05T00:00:00Z");
    let mut p = provider(8);
    let e = ask(&mut p, &mut f, &row, "loom:issue").unwrap();
    assert_eq!(e.source, Source::Mirror);
    assert_eq!(e.at, t("2026-10-10T00:00:00Z"));
    assert!(f.reads.is_empty());
}

#[test]
fn a_mirror_time_for_a_label_the_item_no_longer_carries_is_not_believed() {
    let row = with_labels_at(
        item(1, &["loom:building"], Some("2026-10-01T00:00:00Z")),
        &[
            ("loom:issue", "2026-10-10T00:00:00Z"),
            ("loom:building", "nonsense"),
        ],
    );
    let mut f = FakeEvents::default().with(1, "loom:building", "2026-10-11T00:00:00Z");
    let mut p = provider(8);
    // A malformed mirror time falls through to the events.
    let e = ask(&mut p, &mut f, &row, "loom:building").unwrap();
    assert_eq!((e.source, e.at), (Source::Events, t("2026-10-11T00:00:00Z")));
    // A label not carried has no mirror answer either.
    let e = ask(&mut p, &mut f, &row, "loom:issue").unwrap();
    assert_ne!(e.source, Source::Mirror);
}

#[test]
fn events_answer_next_and_one_read_caches_every_carried_label() {
    let row = item(2, &["loom:issue", "loom:curated"], Some("2026-10-01T00:00:00Z"));
    let mut f = FakeEvents::default()
        .with(2, "loom:curated", "2026-10-03T00:00:00Z")
        .with(2, "loom:issue", "2026-10-04T00:00:00Z")
        .with(2, "loom:triage", "2026-10-02T00:00:00Z");
    let mut p = provider(8);
    let e = ask(&mut p, &mut f, &row, "loom:issue").unwrap();
    assert_eq!((e.source, e.at), (Source::Events, t("2026-10-04T00:00:00Z")));
    let e = ask(&mut p, &mut f, &row, "loom:curated").unwrap();
    assert_eq!((e.source, e.at), (Source::Events, t("2026-10-03T00:00:00Z")));
    assert_eq!(f.reads, [2], "one read served both labels");
    // A label the item no longer carries is not cached.
    assert_eq!(p.cached_len(), 2);
}

#[test]
fn created_at_is_the_last_resort_and_none_when_unknown() {
    let mut f = FakeEvents::default();
    let mut p = provider(8);
    let e = ask(
        &mut p,
        &mut f,
        &item(3, &["loom:issue"], Some("2026-09-01T00:00:00Z")),
        "loom:issue",
    )
    .unwrap();
    assert_eq!((e.source, e.at), (Source::Created, t("2026-09-01T00:00:00Z")));
    assert!(ask(&mut p, &mut f, &item(4, &["loom:issue"], None), "loom:issue").is_none());
}

#[test]
fn the_events_budget_is_per_tick_and_a_spent_budget_falls_back_to_created() {
    let mut f = FakeEvents::default()
        .with(5, "loom:issue", "2026-10-05T00:00:00Z")
        .with(6, "loom:issue", "2026-10-06T00:00:00Z");
    let mut p = provider(1);
    let a = item(5, &["loom:issue"], Some("2026-10-01T00:00:00Z"));
    let b = item(6, &["loom:issue"], Some("2026-10-01T00:00:00Z"));
    assert_eq!(ask(&mut p, &mut f, &a, "loom:issue").unwrap().source, Source::Events);
    assert_eq!(p.budget_left(), 0);
    assert_eq!(ask(&mut p, &mut f, &b, "loom:issue").unwrap().source, Source::Created);
    assert_eq!(f.reads, [5]);
    // A cached answer needs no budget.
    assert_eq!(ask(&mut p, &mut f, &a, "loom:issue").unwrap().source, Source::Events);
    p.begin_tick();
    assert_eq!(ask(&mut p, &mut f, &b, "loom:issue").unwrap().source, Source::Events);
    assert_eq!(f.reads, [5, 6]);
}

#[test]
fn a_failed_read_is_not_cached_and_a_not_found_one_is_until_its_ttl() {
    let row = item(7, &["loom:issue"], Some("2026-10-01T00:00:00Z"));
    let mut p = provider(8);
    let mut failing = FakeEvents {
        fail: true,
        ..FakeEvents::default()
    };
    assert_eq!(
        ask(&mut p, &mut failing, &row, "loom:issue")
            .unwrap()
            .source,
        Source::Created
    );
    assert_eq!(p.cached_len(), 0, "a failed or shed read proves nothing");
    // Read, but the event lies before the pages read: "not found" is cached.
    let mut f = FakeEvents::default();
    assert_eq!(ask(&mut p, &mut f, &row, "loom:issue").unwrap().source, Source::Created);
    assert_eq!(ask(&mut p, &mut f, &row, "loom:issue").unwrap().source, Source::Created);
    assert_eq!(f.reads, [7], "the not-found answer is reused");
    // Past the TTL it is read again.
    let later = now() + Duration::seconds(NOT_FOUND_TTL_SECS);
    p.entry(&mut f, Path::new("/x"), REPO, &row, "loom:issue", later);
    assert_eq!(f.reads, [7, 7]);
}

#[test]
fn the_cache_persists_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("step-dispatch").join("stage-entry.json");
    let row = item(8, &["loom:review-requested"], Some("2026-10-01T00:00:00Z"));
    let mut f = FakeEvents::default().with(8, "loom:review-requested", "2026-10-09T00:00:00Z");
    let mut p = StageEntryProvider::load(Some(path.clone()), 8);
    ask(&mut p, &mut f, &row, "loom:review-requested").unwrap();
    p.flush().unwrap();
    assert!(path.is_file());

    let mut reloaded = StageEntryProvider::load(Some(path.clone()), 8);
    let mut none = FakeEvents::default();
    let e = ask(&mut reloaded, &mut none, &row, "loom:review-requested").unwrap();
    assert_eq!((e.source, e.at), (Source::Events, t("2026-10-09T00:00:00Z")));
    assert!(none.reads.is_empty(), "served from the persisted cache");

    // A file of another schema, or garbage, reads as empty.
    std::fs::write(&path, r#"{"schema": 99, "entries": []}"#).unwrap();
    assert_eq!(StageEntryProvider::load(Some(path.clone()), 8).cached_len(), 0);
    std::fs::write(&path, "not json").unwrap();
    assert_eq!(StageEntryProvider::load(Some(path), 8).cached_len(), 0);
}

#[test]
fn a_label_that_left_the_listing_is_forgotten() {
    let mut f = FakeEvents::default()
        .with(9, "loom:issue", "2026-10-05T00:00:00Z")
        .with(10, "loom:issue", "2026-10-06T00:00:00Z");
    let mut p = provider(8);
    let a = item(9, &["loom:issue"], Some("2026-10-01T00:00:00Z"));
    let b = item(10, &["loom:issue"], Some("2026-10-01T00:00:00Z"));
    ask(&mut p, &mut f, &a, "loom:issue");
    ask(&mut p, &mut f, &b, "loom:issue");
    assert_eq!(p.cached_len(), 2);
    // #9 moved on to building; #10 is unchanged; another repo is untouched.
    let moved = item(9, &["loom:building"], Some("2026-10-01T00:00:00Z"));
    p.retain_present("Acme/Widgets", &[moved, b]);
    assert_eq!(p.cached_len(), 1);
    p.retain_present("acme/other", &[]);
    assert_eq!(p.cached_len(), 1);
    // Re-labelled `loom:issue` later: read again, not the stale cached time.
    let back = item(9, &["loom:issue"], Some("2026-10-01T00:00:00Z"));
    f = f.with(9, "loom:issue", "2026-10-12T00:00:00Z");
    let e = ask(&mut p, &mut f, &back, "loom:issue").unwrap();
    assert_eq!(e.at, t("2026-10-12T00:00:00Z"));
}

#[test]
fn the_cache_keeps_the_most_recent_reads_when_over_its_cap() {
    let mut p = provider(0);
    for n in 0..(MAX_ENTRIES as u32 + 5) {
        let read_at = now() + Duration::seconds(i64::from(n));
        p.cache.insert(
            key(REPO, n, "loom:issue"),
            Cached {
                at: Some(now()),
                read_at,
                updated_at: None,
            },
        );
    }
    p.evict();
    assert_eq!(p.cached_len(), MAX_ENTRIES);
    assert!(!p.cache.contains_key(&key(REPO, 0, "loom:issue")), "the oldest read went first");
    assert!(p
        .cache
        .contains_key(&key(REPO, MAX_ENTRIES as u32 + 4, "loom:issue")));
}

/// The live read is the shared page walk under the provider's own caller.
#[test]
fn the_live_events_read_has_its_own_caller() {
    fn takes(_: &mut impl EventsSource) {}
    takes(&mut GhEventsReader);
    assert!(crate::gh_invocation::Operation::is_valid(EVENTS_CALLER));
}

#[test]
fn every_events_page_counts_against_the_budget() {
    let mut f = FakeEvents {
        pages: 3,
        ..FakeEvents::default()
    }
    .with(11, "loom:pr", "2026-10-11T00:00:00Z")
    .with(12, "loom:pr", "2026-10-12T00:00:00Z");
    let mut p = provider(4);
    let a = item(11, &["loom:pr"], Some("2026-10-01T00:00:00Z"));
    let b = item(12, &["loom:pr"], Some("2026-10-01T00:00:00Z"));
    assert_eq!(ask(&mut p, &mut f, &a, "loom:pr").unwrap().source, Source::Events);
    assert_eq!(p.budget_left(), 1, "a three-page read spent three");
    ask(&mut p, &mut f, &b, "loom:pr");
    assert_eq!(f.allowed, [4, 1], "a read is never allowed more than is left");
    assert_eq!(p.budget_left(), 0);
}

#[test]
fn an_item_updated_since_the_read_is_re_read_once_its_window_passes() {
    let row = updated(
        item(13, &["loom:review-requested"], Some("2026-10-01T00:00:00Z")),
        "2026-10-10T00:00:00Z",
    );
    let mut f = FakeEvents::default().with(13, "loom:review-requested", "2026-10-09T00:00:00Z");
    let mut p = provider(8);
    let at = |p: &mut StageEntryProvider, f: &mut FakeEvents, row: &RestIssue, when| {
        p.entry(f, Path::new("/x"), REPO, row, "loom:review-requested", when)
            .unwrap()
    };
    assert_eq!(at(&mut p, &mut f, &row, now()).at, t("2026-10-09T00:00:00Z"));
    // Unchanged item: served from the cache however long after.
    let much_later = now() + Duration::days(3);
    at(&mut p, &mut f, &row, much_later);
    assert_eq!(f.reads, [13]);

    // Removed and re-applied between two listings: only `updated_at` shows it.
    let moved = updated(row.clone(), "2026-10-14T11:00:00Z");
    f = f.with(13, "loom:review-requested", "2026-10-14T11:00:00Z");
    let soon = now() + Duration::seconds(REVALIDATE_SECS - 1);
    assert_eq!(
        at(&mut p, &mut f, &moved, soon).at,
        t("2026-10-09T00:00:00Z"),
        "inside the window"
    );
    assert_eq!(f.reads, [13]);

    // Past the window with no budget left, the old time stands (closer than
    // `created`); with budget, it is re-read.
    let past = now() + Duration::seconds(REVALIDATE_SECS);
    let mut spent = provider(0);
    spent.cache = p.cache.clone();
    let e = at(&mut spent, &mut f, &moved, past);
    assert_eq!((e.source, e.at), (Source::Events, t("2026-10-09T00:00:00Z")));
    assert_eq!(f.reads, [13]);
    assert_eq!(at(&mut p, &mut f, &moved, past).at, t("2026-10-14T11:00:00Z"));
    assert_eq!(f.reads, [13, 13]);
    // The new answer is keyed to the new `updated_at`.
    at(&mut p, &mut f, &moved, past + Duration::days(1));
    assert_eq!(f.reads, [13, 13]);
}

#[test]
fn a_re_read_that_misses_the_label_keeps_the_time_already_known() {
    let row = updated(
        item(14, &["loom:review-requested"], Some("2026-10-01T00:00:00Z")),
        "2026-10-10T00:00:00Z",
    );
    let mut f = FakeEvents::default().with(14, "loom:review-requested", "2026-10-09T00:00:00Z");
    let mut p = provider(8);
    let at = |p: &mut StageEntryProvider, f: &mut FakeEvents, row: &RestIssue, when| {
        p.entry(f, Path::new("/x"), REPO, row, "loom:review-requested", when)
            .unwrap()
    };
    assert_eq!(at(&mut p, &mut f, &row, now()).at, t("2026-10-09T00:00:00Z"));

    // Updated since, and the capped re-read no longer shows the label.
    let moved = updated(row, "2026-10-14T11:00:00Z");
    f.times.clear();
    let past = now() + Duration::seconds(REVALIDATE_SECS);
    let e = at(&mut p, &mut f, &moved, past);
    assert_eq!((e.source, e.at), (Source::Events, t("2026-10-09T00:00:00Z")));
    assert_eq!(f.reads, [14, 14]);
    // The kept time is re-keyed to the new `updated_at`: no further read.
    at(&mut p, &mut f, &moved, past + Duration::days(1));
    assert_eq!(f.reads, [14, 14]);
}

/// A cached `loom:pr` time for `number`, read at `now()` when the item's
/// `updated_at` was earlier than the row's, so the next ask past the
/// window is a re-read.
fn known(p: &mut StageEntryProvider, number: u32, at: &str) {
    p.cache.insert(
        key(REPO, number, "loom:pr"),
        Cached {
            at: Some(t(at)),
            read_at: now() - Duration::seconds(REVALIDATE_SECS),
            updated_at: Some("2026-10-01T00:00:00Z".to_string()),
        },
    );
}

fn moved(number: u32) -> RestIssue {
    updated(item(number, &["loom:pr"], Some("2026-09-01T00:00:00Z")), "2026-10-14T11:00:00Z")
}

#[test]
fn first_time_reads_take_the_budget_before_re_reads() {
    let mut f = FakeEvents::default()
        .with(21, "loom:pr", "2026-10-12T00:00:00Z")
        .with(22, "loom:pr", "2026-10-12T00:00:00Z")
        .with(23, "loom:pr", "2026-10-12T00:00:00Z");
    let mut p = provider(4);
    known(&mut p, 21, "2026-10-08T00:00:00Z");
    known(&mut p, 22, "2026-10-08T00:00:00Z");
    // Re-reads may spend only the half not kept for first-time reads.
    assert_eq!(
        ask(&mut p, &mut f, &moved(21), "loom:pr").unwrap().at,
        t("2026-10-12T00:00:00Z")
    );
    assert_eq!(
        ask(&mut p, &mut f, &moved(22), "loom:pr").unwrap().at,
        t("2026-10-12T00:00:00Z")
    );
    assert_eq!(f.allowed, [2, 1], "never into the reserved half");
    assert_eq!(p.budget_left(), 2);
    known(&mut p, 24, "2026-10-08T00:00:00Z");
    assert_eq!(
        ask(&mut p, &mut f, &moved(24), "loom:pr").unwrap().at,
        t("2026-10-08T00:00:00Z")
    );
    assert_eq!(f.reads, [21, 22], "the reserve is not a re-read's");
    // A first-time read still has it.
    let fresh = item(23, &["loom:pr"], Some("2026-09-01T00:00:00Z"));
    assert_eq!(ask(&mut p, &mut f, &fresh, "loom:pr").unwrap().source, Source::Events);

    // A tick in which a first-time read went unserved: no re-reads in it or
    // the next one.
    let mut p = provider(2);
    known(&mut p, 21, "2026-10-08T00:00:00Z");
    let mut f = FakeEvents::default().with(21, "loom:pr", "2026-10-12T00:00:00Z");
    for n in [30, 31, 32] {
        ask(&mut p, &mut f, &item(n, &["loom:pr"], Some("2026-09-01T00:00:00Z")), "loom:pr");
    }
    assert_eq!(f.reads, [30, 31], "#32 found the budget spent");
    p.begin_tick();
    assert_eq!(
        ask(&mut p, &mut f, &moved(21), "loom:pr").unwrap().at,
        t("2026-10-08T00:00:00Z")
    );
    assert_eq!(f.reads, [30, 31], "the tick after a starved one re-reads nothing");
    p.begin_tick();
    assert_eq!(
        ask(&mut p, &mut f, &moved(21), "loom:pr").unwrap().at,
        t("2026-10-12T00:00:00Z")
    );
}
