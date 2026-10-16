//! Tests for the step engine's all-open mirror listing and `labels_at`
//! (#11897).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use super::client::{MirrorError, MirrorItem};
use super::discovery::{caller_may_list, list_open, list_open_via, Caller, Fetch, Outcome, Pages};
use super::Active;
use crate::forge_events::poll_gate::ReadKind;

const REPO: &str = "acme/widgets";

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-13T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn fresh_mark() -> Value {
    json!({
        "repo": REPO, "known": true,
        "last_event_at": "2026-10-13T11:58:00Z",
        "last_reconciled_at": "2026-10-13T10:00:00Z",
        "latest_seq": 7, "served_at": "2026-10-13T12:00:00Z",
        "archived": false, "gone": null
    })
}

fn item(number: u64, labels: &[&str], kind: &str) -> Value {
    json!({
        "number": number, "type": kind, "state": "open", "labels": labels,
        "created_at": "2026-10-01T00:00:00Z"
    })
}

fn page(items: Vec<Value>, next_after: Option<u64>) -> Value {
    json!({ "repo": REPO, "items": items, "next_after": next_after, "watermark": fresh_mark() })
}

#[derive(Default)]
struct Canned {
    answers: Vec<Result<Value, MirrorError>>,
    calls: Vec<Vec<(String, String)>>,
}

impl Fetch for Canned {
    fn get(
        &mut self,
        _path: &str,
        query: &[(String, String)],
        _deadline: Duration,
    ) -> Result<Value, MirrorError> {
        self.calls.push(query.to_vec());
        if self.answers.is_empty() {
            return Err(MirrorError::Status(500));
        }
        self.answers.remove(0)
    }
}

fn active() -> Active {
    Active::for_test("http://127.0.0.1:9", Duration::from_secs(600))
}

#[test]
fn the_all_open_listing_asks_with_no_label_and_keeps_every_open_item_oldest_first() {
    let mut closed = item(4, &[], "issue");
    closed["state"] = json!("closed");
    let mut f = Canned {
        answers: vec![
            Ok(page(
                vec![
                    item(9, &["loom:issue"], "issue"),
                    item(2, &[], "pr"),
                    closed,
                ],
                Some(9),
            )),
            Ok(page(vec![item(5, &["bug"], "issue"), item(2, &[], "pr")], None)),
        ],
        ..Canned::default()
    };
    let (rows, outcome) =
        list_open_via(ReadKind::Discovery, &active(), &mut f, REPO, None, Pages::All, now());
    assert_eq!(outcome, Outcome::Hit);
    let rows = rows.unwrap();
    assert_eq!(rows.iter().map(|r| r.number).collect::<Vec<_>>(), [2, 5, 9]);
    assert!(rows[0].is_pull_request);
    assert_eq!(f.calls.len(), 2);
    for call in &f.calls {
        assert!(call.iter().all(|(k, _)| k != "label"), "no label filter: {call:?}");
        assert!(call.contains(&("state".to_string(), "open".to_string())));
    }
}

#[test]
fn the_all_open_listing_falls_back_like_any_other() {
    let mut stale = fresh_mark();
    stale["last_event_at"] = json!("2026-10-13T10:00:00Z");
    let mut f = Canned {
        answers: vec![Ok(
            json!({ "items": [], "next_after": null, "watermark": stale }),
        )],
        ..Canned::default()
    };
    let (rows, outcome) =
        list_open_via(ReadKind::Discovery, &active(), &mut f, REPO, None, Pages::All, now());
    assert!(rows.is_none());
    assert_eq!(outcome, Outcome::Stale);
    // And a decision read never asks.
    let mut f = Canned::default();
    let (rows, _) =
        list_open_via(ReadKind::Claim, &active(), &mut f, REPO, None, Pages::All, now());
    assert!(rows.is_none() && f.calls.is_empty());
}

/// Only the step engine may list with no label: the single-label callers
/// replace single-label REST reads, and get `None` (their REST path) without
/// a request.
#[test]
fn only_the_step_engine_may_list_every_open_item() {
    assert!(caller_may_list(Caller::StepEngine, None));
    assert!(caller_may_list(Caller::StepEngine, Some("loom:issue")));
    assert!(!caller_may_list(Caller::WorkFinder, None));
    assert!(caller_may_list(Caller::WorkFinder, Some("loom:issue")));
    assert_eq!(Caller::StepEngine.as_str(), "step_engine");
    // With no mirror live the answer is `None` whoever asks.
    assert!(
        list_open(ReadKind::Discovery, Caller::WorkFinder, None, Some(REPO), None, Pages::All)
            .is_none()
    );
}

#[test]
fn a_wrongly_shaped_labels_at_is_none_for_that_item_only() {
    for bad in [
        json!(["loom:issue", "2026-10-10T00:00:00.000Z"]),
        json!("2026-10-10T00:00:00.000Z"),
        json!(17),
        json!(null),
    ] {
        let parsed: MirrorItem = serde_json::from_value(json!({
            "number": 3, "state": "open", "labels": ["loom:issue"], "labels_at": bad
        }))
        .unwrap_or_else(|e| panic!("labels_at {bad}: the item must still parse: {e}"));
        assert_eq!(parsed.labels_at, None, "{bad}");
        assert_eq!(parsed.to_rest().unwrap().labels_at, None, "{bad}");
    }

    // One bad item neither fails the page nor costs its neighbours theirs.
    let mut bad = item(5, &["loom:curated"], "issue");
    bad["labels_at"] = json!(["loom:curated"]);
    let mut good = item(6, &["loom:curated"], "issue");
    good["labels_at"] = json!({ "loom:curated": "2026-10-11T08:00:00.000Z" });
    let mut f = Canned {
        answers: vec![Ok(page(vec![bad, good], None))],
        ..Canned::default()
    };
    let rows = list_open_via(
        ReadKind::Discovery,
        &active(),
        &mut f,
        REPO,
        Some("loom:curated"),
        Pages::First,
        now(),
    )
    .0
    .unwrap();
    assert_eq!(rows.len(), 2);
    let by = |n: u32| rows.iter().find(|r| r.number == n).unwrap();
    assert_eq!(by(5).labels_at, None);
    assert!(by(6)
        .labels_at
        .as_ref()
        .is_some_and(|m| m.contains_key("loom:curated")));
}

#[test]
fn labels_at_is_carried_to_the_rest_row_and_stays_optional() {
    let with: MirrorItem = serde_json::from_value(json!({
        "number": 3, "state": "open", "labels": ["loom:issue"],
        "labels_at": { "loom:issue": "2026-10-10T00:00:00.000Z", "odd": 17 }
    }))
    .unwrap();
    let row = with.to_rest().unwrap();
    let at = row.labels_at.unwrap();
    assert_eq!(at.get("loom:issue").map(String::as_str), Some("2026-10-10T00:00:00.000Z"));
    assert!(!at.contains_key("odd"), "a malformed time drops only itself");

    let without: MirrorItem =
        serde_json::from_value(json!({ "number": 3, "state": "open", "labels": [] })).unwrap();
    assert_eq!(without.to_rest().unwrap().labels_at, None);

    // And through a listing.
    let mut served = item(6, &["loom:curated"], "issue");
    served["labels_at"] = json!({ "loom:curated": "2026-10-11T08:00:00.000Z" });
    let mut f = Canned {
        answers: vec![Ok(page(vec![served], None))],
        ..Canned::default()
    };
    let rows = list_open_via(
        ReadKind::Discovery,
        &active(),
        &mut f,
        REPO,
        Some("loom:curated"),
        Pages::First,
        now(),
    )
    .0
    .unwrap();
    assert_eq!(
        rows[0]
            .labels_at
            .as_ref()
            .and_then(|m| m.get("loom:curated"))
            .map(String::as_str),
        Some("2026-10-11T08:00:00.000Z")
    );
}
