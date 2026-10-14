//! Tests for the events walk (#11924): the latest `labeled` event comes from
//! the end of the events, page 1 is believed only when the walk is
//! contiguous, and every page counts.

use super::*;

fn t(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn labeled(label: &str, at: &str) -> serde_json::Value {
    serde_json::json!({"event": "labeled", "label": {"name": label}, "created_at": at})
}

/// A scripted item: `pages[i]` is page `i + 1`; every read is recorded.
struct Item {
    pages: Vec<Vec<serde_json::Value>>,
    fail: Option<u32>,
    reads: Vec<u32>,
}

impl Item {
    fn new(pages: Vec<Vec<serde_json::Value>>) -> Self {
        Self {
            pages,
            fail: None,
            reads: Vec::new(),
        }
    }

    fn walk(&mut self, label: &str, max_pages: usize) -> LabelTimes {
        let last = u32::try_from(self.pages.len()).unwrap();
        let (pages, fail, reads) = (&self.pages, self.fail, &mut self.reads);
        walk(
            |n| {
                reads.push(n);
                if fail == Some(n) {
                    return None;
                }
                Some(EventsPage {
                    events: serde_json::Value::Array(pages[n as usize - 1].clone()),
                    // GitHub names `rel="last"` on every page but the last.
                    last: (n < last).then_some(last),
                })
            },
            label,
            max_pages,
        )
    }
}

fn filler(n: usize) -> Vec<serde_json::Value> {
    vec![serde_json::json!({"event": "commented", "created_at": "2026-09-01T00:00:00Z"}); n]
}

#[test]
fn a_single_page_item_is_one_read() {
    let mut item = Item::new(vec![vec![
        labeled("loom:review-requested", "2026-09-01T00:00:00Z"),
        labeled("loom:review-requested", "2026-09-05T00:00:00Z"),
    ]]);
    let read = item.walk("loom:review-requested", 3);
    assert_eq!(read.pages, 1);
    assert_eq!(read.times.unwrap()["loom:review-requested"], t("2026-09-05T00:00:00Z"));
}

#[test]
fn a_label_reapplied_after_page_one_takes_the_last_pages_time() {
    let mut p1 = vec![labeled("loom:review-requested", "2026-09-01T00:00:00Z")];
    p1.extend(filler(99));
    let mut item = Item::new(vec![
        p1,
        filler(100),
        vec![labeled("Loom:Review-Requested", "2026-10-10T00:00:00Z")],
    ]);
    let read = item.walk("loom:review-requested", 3);
    assert_eq!(item.reads, [1, 3], "page 1, then straight to the last");
    assert_eq!(read.pages, 2);
    let times = read.times.unwrap();
    assert_eq!(times["Loom:Review-Requested"], t("2026-10-10T00:00:00Z"));
    assert!(
        !times.contains_key("loom:review-requested"),
        "page 1's older time is not believed across a skipped page"
    );
}

#[test]
fn the_walk_steps_back_one_page_when_the_last_has_no_event_for_the_label() {
    let mut item = Item::new(vec![
        vec![labeled("loom:issue", "2026-09-01T00:00:00Z")],
        filler(50),
        vec![labeled("loom:building", "2026-09-20T00:00:00Z")],
        filler(3),
    ]);
    // Page 1 for the `Link`, the last (no event for the label), one back.
    let read = item.walk("loom:building", 3);
    assert_eq!(item.reads, [1, 4, 3]);
    assert_eq!(read.pages, 3);
    let times = read.times.unwrap();
    assert_eq!(times["loom:building"], t("2026-09-20T00:00:00Z"));
    assert!(
        !times.contains_key("loom:issue"),
        "pages 1..2 were not contiguous with the walk"
    );
}

#[test]
fn a_walk_reaching_page_two_believes_page_one_too() {
    let mut item = Item::new(vec![
        vec![
            labeled("loom:issue", "2026-09-01T00:00:00Z"),
            labeled("loom:building", "2026-09-02T00:00:00Z"),
        ],
        vec![labeled("loom:building", "2026-09-10T00:00:00Z")],
        filler(1),
    ]);
    let read = item.walk("loom:issue", 3);
    assert_eq!(item.reads, [1, 3, 2]);
    let times = read.times.unwrap();
    assert_eq!(times["loom:issue"], t("2026-09-01T00:00:00Z"));
    assert_eq!(times["loom:building"], t("2026-09-10T00:00:00Z"), "the later page wins");
}

#[test]
fn a_label_not_in_the_pages_read_is_not_found_and_the_cap_holds() {
    let mut item = Item::new(vec![
        vec![labeled("loom:curated", "2026-09-01T00:00:00Z")],
        filler(100),
        filler(100),
        filler(100),
        filler(1),
    ]);
    let read = item.walk("loom:curated", 10);
    assert_eq!(item.reads, [1, 5, 4], "never past MAX_PAGES");
    assert_eq!(read.pages, MAX_PAGES);
    assert!(read.times.unwrap().is_empty());
}

#[test]
fn the_budget_bounds_the_walk_and_one_page_cannot_answer_a_long_item() {
    let mut item = Item::new(vec![filler(100), vec![labeled("x", "2026-09-01T00:00:00Z")]]);
    let read = item.walk("x", 1);
    assert_eq!((read.pages, read.times), (1, None));
    assert_eq!(item.reads, [1]);

    let mut item = Item::new(vec![filler(100), filler(1), filler(1)]);
    let read = item.walk("x", 2);
    assert_eq!(item.reads, [1, 3]);
    assert_eq!(read.pages, 2);
    assert!(read.times.unwrap().is_empty());
}

#[test]
fn a_failed_page_fails_the_read_and_still_counts() {
    let mut item = Item::new(vec![filler(100), filler(1)]);
    item.fail = Some(2);
    let read = item.walk("x", 3);
    assert_eq!((read.pages, read.times), (2, None));

    let mut item = Item::new(vec![filler(1)]);
    item.fail = Some(1);
    assert_eq!(
        item.walk("x", 3),
        LabelTimes {
            times: None,
            pages: 1
        }
    );
}

#[test]
fn the_last_page_comes_from_the_link_header_only() {
    let raw = "HTTP/2.0 200 OK\r\n\
        Link: <https://api.github.com/repositories/1/issues/7/events?per_page=100&page=2>; rel=\"next\", \
        <https://api.github.com/repositories/1/issues/7/events?per_page=100&page=14>; rel=\"last\"\r\n\
        Content-Type: application/json\r\n\r\n\
        [{\"event\": \"labeled\", \"label\": {\"name\": \"a\"}, \"created_at\": \"2026-09-01T00:00:00Z\"}]";
    let page = parse_page(raw).unwrap();
    assert_eq!(page.last, Some(14), "per_page is not page");
    assert_eq!(page.events.as_array().unwrap().len(), 1);

    let last_page_raw = "HTTP/2.0 200 OK\r\n\
        Link: <https://api.github.com/x?page=13>; rel=\"prev\", <https://api.github.com/x?page=1>; rel=\"first\"\r\n\r\n[]";
    assert_eq!(parse_page(last_page_raw).unwrap().last, None);

    // A body line that looks like a Link header is never read as one.
    let body_link = "HTTP/2.0 200 OK\r\n\r\nLink: <https://x?page=9>; rel=\"last\"\n";
    assert_eq!(last_page(body_link), None);

    assert_eq!(parse_page("HTTP/2.0 404 Not Found\r\n\r\n{\"message\": \"Not Found\"}"), None);
    assert_eq!(parse_page("not an http response"), None);
}
