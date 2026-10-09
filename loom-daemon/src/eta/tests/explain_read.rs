//! Reading emitted estimates back: by id, by subject, and `--at` (#10930).

use super::*;
use crate::eta::explain;
use crate::eta::fleet_signoz_refresh::FileRows;
use chrono::TimeZone;

const GOLDEN: &str = include_str!("../fixtures/explanation-golden.json");

fn golden() -> Explanation {
    serde_json::from_str(GOLDEN).expect("golden parses")
}

fn at(h: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 20, h, 0, 0).unwrap()
}

/// One export row for `e`, knowable `lag_ns` after its `as_of`.
fn row(e: &Explanation, record: &str, lag_ns: i64) -> String {
    let ns = e.as_of.timestamp_nanos_opt().unwrap();
    serde_json::json!({
        "record_id": record,
        "repo": e.subject.repo,
        "estimate_id": e.estimate_id,
        "body": serde_json::to_string(e).unwrap(),
        "event_time_ns": ns.to_string(),
        "knowable_time_ns": (ns + lag_ns).to_string(),
    })
    .to_string()
}

fn variant(id: &str, hour: u32) -> Explanation {
    let mut e = golden();
    e.estimate_id = id.to_string();
    e.as_of = at(hour);
    e
}

/// Three land estimates of the same issue 12:00 / 13:00 / 14:00, plus a
/// `finish` one and another issue's, plus a row that is not an explanation.
fn export() -> FileRows {
    let (a, b, c) = (variant("aaaa", 12), variant("bbbb", 13), variant("cccc", 14));
    let mut fin = variant("ffff", 13);
    fin.kind = Kind::Finish;
    let mut other = variant("oooo", 13);
    other.subject.issue = 1;
    let junk = serde_json::json!({
        "record_id": "z", "repo": "rjwalters/loom", "estimate_id": "zzzz",
        "body": "{\"not\":\"an explanation\"}",
        "event_time_ns": "1", "knowable_time_ns": at(13).timestamp_nanos_opt().unwrap().to_string(),
    })
    .to_string();
    let text = [
        row(&a, "r1", 1_000),
        row(&b, "r2", 1_000),
        row(&c, "r3", 1_000),
        row(&fin, "r4", 1_000),
        row(&other, "r5", 1_000),
        junk,
    ]
    .join("\n");
    FileRows::parse(&text).unwrap()
}

fn window() -> (DateTime<Utc>, DateTime<Utc>) {
    (at(0), at(23))
}

fn subject_selector() -> Selector {
    Selector {
        repo: Some("rjwalters/loom".into()),
        issue: Some(9289),
        ..Selector::default()
    }
}

#[test]
fn by_id_finds_one_estimate_in_any_repo() {
    let (since, until) = window();
    let sel = Selector {
        estimate_id: Some("bbbb".into()),
        ..Selector::default()
    };
    let got = read(&mut export(), &sel, since, until).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].estimate_id, "bbbb");
    // The emitted body replays exactly, like the one on disk.
    assert_eq!(explain::report(&got[0]).parity, explain::Parity::Exact);
}

#[test]
fn an_unknown_id_is_none() {
    let (since, until) = window();
    let sel = Selector {
        estimate_id: Some("nope".into()),
        ..Selector::default()
    };
    assert!(read(&mut export(), &sel, since, until).unwrap().is_empty());
}

#[test]
fn at_returns_the_newest_emitted_estimate_not_after_t() {
    let (since, until) = window();
    let mut sel = subject_selector();
    sel.kind = Some(Kind::Land);
    sel.at = Some(at(13) + chrono::Duration::minutes(30));
    let got = newest_per_kind(read(&mut export(), &sel, since, until).unwrap());
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].estimate_id, "bbbb", "13:00 is the newest as_of <= 13:30");
}

#[test]
fn at_is_inclusive_and_before_the_first_estimate_is_none() {
    let (since, until) = window();
    let mut sel = subject_selector();
    sel.at = Some(at(12));
    let got = newest_per_kind(read(&mut export(), &sel, since, until).unwrap());
    assert_eq!(
        got.iter()
            .map(|e| e.estimate_id.as_str())
            .collect::<Vec<_>>(),
        ["aaaa"]
    );
    sel.at = Some(at(11));
    assert!(read(&mut export(), &sel, since, until).unwrap().is_empty());
}

#[test]
fn without_at_each_kind_gets_its_newest() {
    let (since, until) = window();
    let got = newest_per_kind(read(&mut export(), &subject_selector(), since, until).unwrap());
    let ids: Vec<_> = got
        .iter()
        .map(|e| (e.kind, e.estimate_id.as_str()))
        .collect();
    assert_eq!(ids, [(Kind::Finish, "ffff"), (Kind::Land, "cccc")]);
}

#[test]
fn heuristic_and_issue_filters_apply() {
    let (since, until) = window();
    let mut sel = subject_selector();
    sel.heuristic = Some("no-such-heuristic".into());
    assert!(read(&mut export(), &sel, since, until).unwrap().is_empty());
    let mut sel = subject_selector();
    sel.issue = Some(1);
    let got = read(&mut export(), &sel, since, until).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].estimate_id, "oooo");
}

#[test]
fn the_sql_binds_every_selector_it_names() {
    let names: Vec<_> = Selector::default()
        .params()
        .iter()
        .map(|(n, _)| *n)
        .collect();
    for n in &names {
        assert!(ESTIMATE_SQL.contains(&format!("{{{n}:")), "{n} not in ESTIMATE_SQL");
    }
    // The page parameters come from PageQuery.
    for n in [
        "repo", "since_ns", "until_ns", "after_ns", "after_id", "limit",
    ] {
        assert!(ESTIMATE_SQL.contains(&format!("{{{n}:")), "{n}");
    }
}

#[test]
fn a_full_window_beyond_the_page_ceiling_is_refused_not_truncated() {
    struct Endless;
    impl SignozRead for Endless {
        fn page(&mut self, q: &PageQuery) -> Result<String, ReadError> {
            let base = q.after.as_ref().map_or(0, |(ns, _)| *ns);
            Ok((1..=PAGE_LIMIT as i64)
                .map(|i| {
                    format!(
                        "{{\"record_id\":\"{}\",\"knowable_time_ns\":\"{}\"}}",
                        base + i,
                        base + i
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
    }
    let (since, until) = window();
    let err = read(&mut Endless, &Selector::default(), since, until).unwrap_err();
    assert_eq!(err, ExplainReadError::TooManyRows);
}

#[test]
fn an_unavailable_backend_is_reported() {
    struct Down;
    impl SignozRead for Down {
        fn page(&mut self, _: &PageQuery) -> Result<String, ReadError> {
            Err(ReadError::Unavailable("no route".into()))
        }
    }
    let (since, until) = window();
    let err = read(&mut Down, &Selector::default(), since, until).unwrap_err();
    assert!(err.to_string().contains("no route"), "{err}");
}
