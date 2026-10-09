//! The Champion hold-marker log (#10958, Slice 1): marker parsing, trust,
//! the budgeted `since` walk, coverage, and persistence.

use crate::eta::hold_marker_log::{
    append, compact, cursor_path, for_repo, load, log_path, parse_body, parse_page, refresh,
    Coverage, HoldMarker, MarkerCursor, MarkerKind, MarkerSource, RepoCursor, BACKFILL_DAYS,
    MARKER_SCHEMA, PER_PAGE,
};
use chrono::{DateTime, Duration, SecondsFormat, TimeZone, Utc};
use serde_json::{json, Value};
use std::cell::RefCell;

fn t(h: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap() + Duration::hours(h)
}

fn ts(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// A REST comment as the repo-wide listing returns it.
fn comment(id: u64, thread: u32, pr: bool, body: &str, at: DateTime<Utc>, assoc: &str) -> Value {
    let kind = if pr { "pull" } else { "issues" };
    json!({
        "id": id,
        "issue_url": format!("https://api.github.com/repos/o/r/issues/{thread}"),
        "html_url": format!("https://github.com/o/r/{kind}/{thread}#issuecomment-{id}"),
        "body": body,
        "created_at": ts(at),
        "updated_at": ts(at),
        "author_association": assoc,
        "user": {"login": "someone", "type": "User"},
    })
}

fn trusted(v: &Value) -> bool {
    v.get("author_association").and_then(Value::as_str) == Some("MEMBER")
}

#[test]
fn every_kind_round_trips_by_exact_name() {
    for k in MarkerKind::ALL {
        assert_eq!(MarkerKind::from_name(k.name()), Some(k));
    }
    assert_eq!(MarkerKind::from_name("merge-risk-hold-digest"), None);
    assert_eq!(MarkerKind::from_name("hold-state"), None);
}

#[test]
fn a_body_yields_its_hold_markers_with_the_hold_state_head() {
    let body =
        "<!-- champion:critical-file-hold -->\n<!-- champion:hold-state head=ABC123 -->\nHeld.";
    let parsed = parse_body(body);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].kind, MarkerKind::CriticalFileHold);
    assert_eq!(parsed[0].head.as_deref(), Some("abc123"));
}

#[test]
fn a_respected_release_names_its_head_and_ac_hold_names_its_pr() {
    let parsed = parse_body("<!-- champion:hold-release-respected:deadbeef -->");
    assert_eq!(parsed[0].kind, MarkerKind::HoldReleaseRespected);
    assert_eq!(parsed[0].head.as_deref(), Some("deadbeef"));
    let ac = parse_body(
        "<!-- champion:ac-hold pr=42 sha=0a1b -->\n**Champion is holding this issue open.**",
    );
    assert_eq!(ac[0].kind, MarkerKind::AcHold);
    assert_eq!(ac[0].pr, Some(42));
    assert_eq!(ac[0].head.as_deref(), Some("0a1b"));
    assert!(
        parse_body("<!-- champion:ac-hold sha=0a1b -->").is_empty(),
        "ac-hold needs its PR"
    );
}

#[test]
fn quoted_fenced_or_inline_markers_and_other_champion_markers_are_prose() {
    let body = "\
See `<!-- champion:merge-risk-hold -->` for details.
> <!-- champion:merge-risk-hold -->
```
<!-- champion:critical-file-hold -->
```
<!-- champion:merge-risk-hold-digest -->
<!-- champion:dep-defer -->";
    assert!(parse_body(body).is_empty());
}

#[test]
fn a_kind_is_logged_once_per_comment() {
    let body = "<!-- champion:merge-risk-hold -->\n<!-- champion:merge-risk-hold -->";
    assert_eq!(parse_body(body).len(), 1);
}

#[test]
fn a_page_keeps_trusted_pr_markers_only() {
    let page = json!([
        comment(1, 7, true, "<!-- champion:merge-risk-hold -->", t(1), "MEMBER"),
        // Untrusted author: a well-formed marker is prose.
        comment(2, 7, true, "<!-- champion:merge-risk-hold-cleared -->", t(2), "NONE"),
        // A PR hold marker on an issue thread is not a PR hold.
        comment(3, 9, false, "<!-- champion:critical-file-hold -->", t(3), "MEMBER"),
        // ac-hold lives on the issue and names its PR.
        comment(4, 9, false, "<!-- champion:ac-hold pr=7 sha=ab -->", t(4), "MEMBER"),
        comment(5, 7, true, "no markers here", t(5), "MEMBER"),
    ]);
    let p = parse_page("O/R", &page, &trusted, t(6), MarkerSource::Live).unwrap();
    assert_eq!(p.rows, 5);
    assert_eq!(p.max_updated, Some(t(5)));
    let got: Vec<(u64, u32, u32, MarkerKind)> = p
        .markers
        .iter()
        .map(|m| (m.comment_id, m.pr, m.thread, m.kind))
        .collect();
    assert_eq!(
        got,
        vec![
            (1, 7, 7, MarkerKind::MergeRiskHold),
            (4, 7, 9, MarkerKind::AcHold)
        ]
    );
    assert!(p
        .markers
        .iter()
        .all(|m| m.repo == "o/r" && m.schema == MARKER_SCHEMA));
    assert_eq!(p.markers[0].created_at, t(1));
    assert!(
        parse_page("o/r", &json!({"message": "x"}), &trusted, t(6), MarkerSource::Live).is_none()
    );
}

#[test]
fn the_walk_advances_since_and_reaches_coverage_on_a_short_page() {
    let mut c = RepoCursor::start(t(0));
    assert_eq!(c.backfill_from, t(0) - Duration::days(BACKFILL_DAYS));
    assert_eq!(c.coverage().through, None);
    assert!(c.url("O/R").starts_with("repos/o/r/issues/comments?since="));
    // A full page moves `since` to its newest row, page 1.
    let full = crate::eta::hold_marker_log::Page {
        markers: vec![],
        rows: PER_PAGE,
        max_updated: Some(t(-100)),
    };
    assert!(!c.advance(&full, t(1)));
    assert_eq!((c.since, c.page), (t(-100), 1));
    // A full page all at that instant steps a page instead of looping.
    assert!(!c.advance(&full, t(1)));
    assert_eq!((c.since, c.page), (t(-100), 2));
    // A short page: caught up, stamped at the read.
    let short = crate::eta::hold_marker_log::Page {
        markers: vec![],
        rows: 3,
        max_updated: Some(t(-1)),
    };
    assert!(c.advance(&short, t(2)));
    assert_eq!((c.since, c.page, c.caught_up_at), (t(-1), 1, Some(t(2))));
    let cov = c.coverage();
    assert!(cov.covers(c.backfill_from, t(2)));
    assert!(!cov.covers(c.backfill_from - Duration::seconds(1), t(2)));
    assert!(!cov.covers(t(0), t(3)));
    assert!(!Coverage::default().covers(t(0), t(0)));
}

/// A fake forge: the repo's comments, served by the listing's `since` (by
/// `updated_at`, ascending) and page, as GitHub does.
fn serve(all: &[Value], url: &str) -> Value {
    let q = url.split_once('?').unwrap().1;
    let get = |k: &str| {
        q.split('&')
            .find_map(|kv| kv.strip_prefix(&format!("{k}=")))
            .unwrap()
            .to_string()
    };
    let since: DateTime<Utc> = get("since").parse().unwrap();
    let page: usize = get("page").parse().unwrap();
    let mut rows: Vec<&Value> = all
        .iter()
        .filter(|v| {
            v["updated_at"]
                .as_str()
                .unwrap()
                .parse::<DateTime<Utc>>()
                .unwrap()
                >= since
        })
        .collect();
    rows.sort_by_key(|v| (v["updated_at"].as_str().unwrap().to_string(), v["id"].as_u64()));
    Value::Array(
        rows.into_iter()
            .skip((page - 1) * PER_PAGE)
            .take(PER_PAGE)
            .cloned()
            .collect(),
    )
}

#[test]
fn a_backfill_walks_under_budget_then_goes_live_without_duplicates() {
    let now = t(24 * 30);
    let start = now - Duration::days(BACKFILL_DAYS);
    // 250 comments over the window; every 50th is a trusted hold marker.
    let mut all: Vec<Value> = (0..250_u64)
        .map(|i| {
            let at = start + Duration::minutes(i64::try_from(i).unwrap() * 60);
            let body = if i % 50 == 0 {
                "<!-- champion:merge-risk-hold -->"
            } else {
                "chatter"
            };
            comment(i + 1, 7, true, body, at, "MEMBER")
        })
        .collect();
    let calls = RefCell::new(0);
    let mut cursor = MarkerCursor::default();
    let mut log: Vec<HoldMarker> = Vec::new();
    let repos = vec!["o/r".to_string()];
    let pass = |all: &[Value], cursor: &mut MarkerCursor, log: &mut Vec<HoldMarker>, budget| {
        let fresh = refresh(
            &repos,
            log,
            cursor,
            budget,
            || now,
            |_, url| {
                *calls.borrow_mut() += 1;
                Some(serve(all, url))
            },
            |_, v| trusted(v),
        );
        log.extend(fresh.clone());
        fresh
    };
    // Budget 2: two full pages, not caught up.
    let first = pass(&all, &mut cursor, &mut log, 2);
    assert_eq!(*calls.borrow(), 2);
    assert!(cursor.coverage("o/r").through.is_none());
    assert!(first.iter().all(|m| m.source == MarkerSource::Backfill));
    // Next pass finishes the walk.
    pass(&all, &mut cursor, &mut log, 6);
    assert_eq!(cursor.coverage("O/R").through, Some(now));
    assert_eq!(log.len(), 5, "every marker once, overlap deduped");
    // A quiet pass re-reads one page and appends nothing.
    let before = *calls.borrow();
    assert!(pass(&all, &mut cursor, &mut log, 6).is_empty());
    assert_eq!(*calls.borrow(), before + 1);
    // A new marker is logged live.
    all.push(comment(999, 8, true, "<!-- champion:critical-file-hold -->", now, "MEMBER"));
    let live = pass(&all, &mut cursor, &mut log, 6);
    assert_eq!(live.len(), 1);
    assert_eq!((live[0].pr, live[0].source), (8, MarkerSource::Live));
}

#[test]
fn a_failed_read_leaves_the_cursor_and_the_budget_is_shared() {
    let now = t(0);
    let mut cursor = MarkerCursor::default();
    let calls = RefCell::new(Vec::<String>::new());
    let repos = vec!["a/x".to_string(), "b/y".to_string()];
    let out = refresh(
        &repos,
        &[],
        &mut cursor,
        5,
        || now,
        |repo, _| {
            calls.borrow_mut().push(repo.to_string());
            (repo == "b/y").then(|| json!([]))
        },
        |_, _| true,
    );
    assert!(out.is_empty());
    // a/x failed once and stopped; b/y caught up in one call.
    assert_eq!(*calls.borrow(), vec!["a/x".to_string(), "b/y".to_string()]);
    assert_eq!(cursor.repos["a/x"].caught_up_at, None);
    assert_eq!(cursor.repos["a/x"].page, 1);
    assert_eq!(cursor.repos["b/y"].caught_up_at, Some(now));
}

#[test]
fn the_log_and_cursor_persist_beside_the_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert!(load(root).is_empty());
    assert_eq!(MarkerCursor::read(root), MarkerCursor::default());
    let m = |id, repo: &str, at| HoldMarker {
        schema: MARKER_SCHEMA.into(),
        repo: repo.into(),
        pr: 7,
        thread: 7,
        kind: MarkerKind::MergeRiskHold,
        head: None,
        comment_id: id,
        created_at: at,
        fetched_at: at,
        source: MarkerSource::Live,
    };
    append(root, &[m(2, "o/r", t(2)), m(1, "o/r", t(1)), m(3, "p/q", t(3))]).unwrap();
    assert!(log_path(root)
        .to_string_lossy()
        .ends_with("pr-hold-markers.jsonl"));
    assert_eq!(load(root).len(), 3);
    let mine = for_repo(&load(root), "O/R");
    assert_eq!(mine.iter().map(|m| m.comment_id).collect::<Vec<_>>(), vec![1, 2]);
    let mut c = MarkerCursor::default();
    c.repos.insert("o/r".into(), RepoCursor::start(t(5)));
    c.write(root).unwrap();
    assert!(!cursor_path(root).to_string_lossy().ends_with(".json"));
    assert_eq!(MarkerCursor::read(root), c);
    compact(root, t(0)).unwrap();
    assert_eq!(load(root).len(), 3, "small logs are not rewritten");
    // The snapshot listing never sees either file.
    assert!(crate::eta::fleet::load_all(root).is_empty());
}
