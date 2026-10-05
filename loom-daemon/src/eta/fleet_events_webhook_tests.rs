#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::eta::fleet_events::{load_events, EventLog, SOURCE_FORGE};
use crate::eta::fleet_state::fleet_state;
use serde_json::json;

const REPO: &str = "rjwalters/loom";

fn t(rfc: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(rfc)
        .unwrap()
        .with_timezone(&Utc)
}

fn fetched() -> DateTime<Utc> {
    t("2026-10-05T00:00:00Z")
}

/// One D1 `records` row, payload stored as a JSON string as D1 holds it.
fn row(id: u64, at: &str, target: &str, number: u32, action: &str, label: Option<&str>) -> Value {
    let mut payload = json!({
        "kind": "label.transition",
        "at": at,
        "repo": REPO,
        "target": target,
        "number": number,
        "action": action,
        "labels_after": [],
        "delivery_id": format!("d-{id}"),
    });
    if let Some(l) = label {
        payload["label"] = json!(l);
    }
    json!({
        "id": id,
        "schema_version": 1,
        "emitted_at": at,
        "host_id": "github",
        "kind": "label.transition",
        "repo": REPO,
        "issue": number,
        "payload": payload.to_string(),
        "ingested_at": at,
    })
}

fn jsonl(rows: &[Value]) -> String {
    rows.iter().map(|r| format!("{r}\n")).collect()
}

fn export() -> Vec<Value> {
    vec![
        row(10, "2026-09-01T00:00:00Z", "issue", 5, "opened", None),
        row(11, "2026-09-01T00:00:00Z", "issue", 5, "labeled", Some("loom:issue")),
        row(12, "2026-09-01T01:00:00Z", "issue", 5, "unlabeled", Some("loom:issue")),
        // Same second, two flips: delivery order (`id`) decides.
        row(13, "2026-09-01T01:00:00Z", "issue", 5, "labeled", Some("loom:building")),
        row(14, "2026-09-01T02:00:00Z", "pr", 7, "labeled", Some("loom:review-requested")),
        row(15, "2026-09-01T03:00:00Z", "pr", 7, "unlabeled", Some("loom:review-requested")),
        row(16, "2026-09-01T03:00:00Z", "pr", 7, "labeled", Some("loom:pr")),
    ]
}

#[test]
fn maps_every_action_and_records_source_and_fetch_time_apart() {
    let mut merged: Value = row(20, "2026-09-02T00:00:00Z", "pr", 7, "closed", None);
    let mut payload: Value = serde_json::from_str(merged["payload"].as_str().unwrap()).unwrap();
    payload["merged"] = json!(true);
    merged["payload"] = json!(payload.to_string());
    let rows = vec![
        row(1, "2026-09-01T00:00:00Z", "issue", 5, "opened", None),
        row(2, "2026-09-01T00:00:01Z", "issue", 5, "labeled", Some("loom:issue")),
        row(3, "2026-09-01T00:00:02Z", "issue", 5, "unlabeled", Some("loom:issue")),
        row(4, "2026-09-01T00:00:03Z", "issue", 5, "closed", None),
        row(5, "2026-09-01T00:00:04Z", "issue", 5, "reopened", None),
        merged,
    ];
    let parsed = parse_export(&jsonl(&rows), fetched());
    assert_eq!((parsed.unparseable, parsed.other_kind), (0, 0));
    let events = &parsed.by_repo[REPO];
    let kinds: Vec<EventKind> = events.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            EventKind::Opened,
            EventKind::LabelAdded,
            EventKind::LabelRemoved,
            EventKind::Closed,
            EventKind::Reopened,
            EventKind::Merged,
        ]
    );
    for (e, seq) in events.iter().zip([1u64, 2, 3, 4, 5, 20]) {
        assert_eq!(e.source, SOURCE_WEBHOOK_MIRROR);
        assert_eq!(e.seq, seq, "seq is the D1 row id (delivery order)");
        assert_eq!(e.fetched_at, fetched());
        assert_ne!(e.event_time, e.fetched_at);
    }
    assert_eq!(events[1].label.as_deref(), Some("loom:issue"));
    assert_eq!(events[5].item_kind, ItemKind::Pr);
    assert_eq!(events[0].event_time, t("2026-09-01T00:00:00Z"));
}

#[test]
fn id_ignores_fetch_time_and_differs_from_a_forge_row() {
    let text = jsonl(&export());
    let a = parse_export(&text, fetched());
    let b = parse_export(&text, t("2027-01-01T00:00:00Z"));
    let ids = |p: &MirrorParse| {
        p.by_repo[REPO]
            .iter()
            .map(|e| e.id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&a), ids(&b));
    let webhook = &a.by_repo[REPO][1];
    let forge = RawEvent::new(
        REPO,
        webhook.item,
        webhook.item_kind,
        webhook.kind,
        webhook.label.clone(),
        webhook.event_time,
        SOURCE_FORGE,
        webhook.seq,
        webhook.fetched_at,
    );
    assert_ne!(webhook.id, forge.id, "source is part of the id");
}

#[test]
fn reimport_is_idempotent_and_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    let text = jsonl(&export());
    let mut log = EventLog::open(&path).unwrap();
    let first = parse_export(&text, fetched());
    assert_eq!(log.append(&first.by_repo[REPO]).unwrap(), 7);
    let bytes = std::fs::read(&path).unwrap();

    // Same export again, read later: nothing appended, file unchanged.
    let again = parse_export(&text, t("2026-10-06T00:00:00Z"));
    let mut reopened = EventLog::open(&path).unwrap();
    assert_eq!(reopened.append(&again.by_repo[REPO]).unwrap(), 0);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);

    // A fresh import elsewhere produces the same bytes.
    let other = dir.path().join("other.jsonl");
    let mut log2 = EventLog::open(&other).unwrap();
    log2.append(&parse_export(&text, fetched()).by_repo[REPO])
        .unwrap();
    assert_eq!(std::fs::read(&other).unwrap(), bytes);
}

#[test]
fn an_evicted_later_export_only_appends_and_never_removes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    let rows = export();
    let mut log = EventLog::open(&path).unwrap();
    log.append(&parse_export(&jsonl(&rows[..5]), fetched()).by_repo[REPO])
        .unwrap();
    // D1 evicted rows 10-12; the later export holds 13-16 only.
    let later = parse_export(&jsonl(&rows[3..]), t("2026-10-06T00:00:00Z"));
    assert_eq!(log.append(&later.by_repo[REPO]).unwrap(), 2);
    let staged = load_events(&path);

    // Same canonical content as one import of the whole export.
    let whole = dir.path().join("whole.jsonl");
    let mut log2 = EventLog::open(&whole).unwrap();
    log2.append(&parse_export(&jsonl(&rows), fetched()).by_repo[REPO])
        .unwrap();
    let ids = |v: &[RawEvent]| v.iter().map(|e| e.id.clone()).collect::<Vec<_>>();
    assert_eq!(ids(&staged), ids(&load_events(&whole)));
    assert_eq!(staged.len(), 7);
}

#[test]
fn same_second_rows_replay_in_delivery_order() {
    let events = parse_export(&jsonl(&export()), fetched()).by_repo[REPO].clone();
    let mut reversed = events.clone();
    reversed.reverse();
    let as_of = t("2026-09-01T02:00:00Z");
    let a = fleet_state(&events, REPO, as_of);
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&fleet_state(&reversed, REPO, as_of)).unwrap()
    );
    let issue = a.items.iter().find(|i| i.number == 5).unwrap();
    assert_eq!(issue.labels, vec!["loom:building".to_string()]);
}

/// A same-second label then unlabel of the *same* label: only `seq` (the D1
/// row id) orders them, and the final state depends on that order.
#[test]
fn seq_decides_a_same_second_flip_of_one_label() {
    let replay = |labeled_id: u64, unlabeled_id: u64| {
        let rows = vec![
            row(1, "2026-09-01T00:00:00Z", "issue", 5, "opened", None),
            row(labeled_id, "2026-09-01T01:00:00Z", "issue", 5, "labeled", Some("loom:issue")),
            row(
                unlabeled_id,
                "2026-09-01T01:00:00Z",
                "issue",
                5,
                "unlabeled",
                Some("loom:issue"),
            ),
        ];
        let events = parse_export(&jsonl(&rows), fetched()).by_repo[REPO].clone();
        let mut reversed = events.clone();
        reversed.reverse();
        let as_of = t("2026-09-01T02:00:00Z");
        let state = fleet_state(&events, REPO, as_of);
        assert_eq!(
            serde_json::to_string(&state).unwrap(),
            serde_json::to_string(&fleet_state(&reversed, REPO, as_of)).unwrap(),
            "input order never matters"
        );
        state
            .items
            .iter()
            .find(|i| i.number == 5)
            .unwrap()
            .labels
            .clone()
    };
    // Delivered label-then-unlabel: the label is gone.
    assert!(replay(10, 11).is_empty());
    // Delivered unlabel-then-label: the label stays.
    assert_eq!(replay(11, 10), vec!["loom:issue".to_string()]);
}

/// Imported webhook rows never change the forge fan-out's work list: no
/// webhook-only PR joins it, and a closed PR settled before the import stays
/// settled although the webhook `closed` row sorts after the forge close.
#[test]
fn pr_work_ignores_imported_webhook_rows() {
    use crate::eta::fleet_events_fanout::{pr_work, settle_marker, PerPrKind};

    let forge = |pr: u32, kind: EventKind, label: Option<&str>, at: &str| {
        RawEvent::new(
            REPO,
            pr,
            ItemKind::Pr,
            kind,
            label.map(str::to_string),
            t(at),
            SOURCE_FORGE,
            0,
            fetched(),
        )
    };
    let closed_at = t("2026-09-10T00:00:00Z");
    let forge_rows = vec![
        forge(7, EventKind::Opened, None, "2026-09-09T00:00:00Z"),
        forge(7, EventKind::HeadCommit, Some("abc"), "2026-09-09T01:00:00Z"),
        forge(7, EventKind::Closed, None, "2026-09-10T00:00:00Z"),
        settle_marker(REPO, 7, PerPrKind::Reviews, closed_at, fetched()),
        settle_marker(REPO, 7, PerPrKind::CheckRuns, closed_at, fetched()),
        forge(8, EventKind::Opened, None, "2026-09-11T00:00:00Z"),
    ];
    let before = pr_work(&forge_rows, REPO);
    assert!(before[0].reviews_settled && before[0].checks_settled);

    let webhook = parse_export(
        &jsonl(&[
            // Receipt lag: the webhook close lands seconds after the forge's.
            row(50, "2026-09-10T00:00:04Z", "pr", 7, "closed", None),
            row(51, "2026-09-10T00:00:05Z", "pr", 7, "reopened", None),
            row(52, "2026-09-10T00:00:06Z", "pr", 7, "closed", None),
            // PRs only the mirror has seen, one closed and one never closed.
            row(53, "2026-06-01T00:00:00Z", "pr", 99, "opened", None),
            row(54, "2026-06-02T00:00:00Z", "pr", 99, "closed", None),
            row(55, "2026-06-03T00:00:00Z", "pr", 98, "opened", None),
            row(56, "2026-09-12T00:00:00Z", "pr", 8, "closed", None),
        ]),
        fetched(),
    )
    .by_repo[REPO]
        .clone();
    let mut mixed = forge_rows;
    mixed.extend(webhook);
    assert_eq!(pr_work(&mixed, REPO), before);
}

/// The star's training inputs (#10372) read forge rows only: an import moves
/// neither coverage floor and adds no star change.
#[test]
fn star_inputs_ignore_imported_webhook_rows() {
    use crate::eta::star::RepoStar;
    const STAR: &str = "loom:operator-priority";

    let forge = |item: u32, item_kind: ItemKind, kind: EventKind, label: Option<&str>, at: &str| {
        RawEvent::new(
            REPO,
            item,
            item_kind,
            kind,
            label.map(str::to_string),
            t(at),
            SOURCE_FORGE,
            0,
            fetched(),
        )
    };
    let forge_rows = vec![
        forge(5, ItemKind::Issue, EventKind::Opened, None, "2026-09-05T00:00:00Z"),
        forge(7, ItemKind::Pr, EventKind::ClosingRef, Some("closes"), "2026-09-05T01:00:00Z")
            .with_target(Some(5)),
        forge(5, ItemKind::Issue, EventKind::LabelAdded, Some(STAR), "2026-09-06T00:00:00Z"),
    ];
    let webhook = parse_export(
        &jsonl(&[
            // Before the forge window, and a duplicate of the forge's star.
            row(60, "2026-06-01T00:00:00Z", "issue", 5, "labeled", Some(STAR)),
            row(61, "2026-09-06T00:00:03Z", "issue", 5, "labeled", Some(STAR)),
            row(62, "2026-09-07T00:00:00Z", "issue", 6, "labeled", Some(STAR)),
        ]),
        fetched(),
    )
    .by_repo[REPO]
        .clone();
    let before = RepoStar::from_events(&forge_rows);
    let mut mixed = forge_rows;
    mixed.extend(webhook);
    let after = RepoStar::from_events(&mixed);
    assert_eq!(after.issue_events_from, before.issue_events_from);
    assert_eq!(after.links_from, before.links_from);
    assert_eq!(after.issue_stars, before.issue_stars);
    assert_eq!(after.links, before.links);
}

#[test]
fn rows_at_or_after_t_do_not_change_fleet_state() {
    let rows = export();
    let as_of = t("2026-09-01T03:00:00Z");
    let base = parse_export(&jsonl(&rows), fetched()).by_repo[REPO].clone();
    let baseline = serde_json::to_string(&fleet_state(&base, REPO, as_of)).unwrap();
    let mut perturbed_rows = rows;
    perturbed_rows.push(row(30, "2026-09-01T03:00:00Z", "issue", 5, "closed", None));
    perturbed_rows.push(row(31, "2026-09-01T04:00:00Z", "issue", 99, "opened", None));
    perturbed_rows.push(row(
        32,
        "2026-09-02T00:00:00Z",
        "issue",
        5,
        "unlabeled",
        Some("loom:building"),
    ));
    let perturbed = parse_export(&jsonl(&perturbed_rows), fetched()).by_repo[REPO].clone();
    assert_eq!(baseline, serde_json::to_string(&fleet_state(&perturbed, REPO, as_of)).unwrap());
}

#[test]
fn reads_wrangler_json_and_object_payloads_and_counts_skips() {
    let mut object_payload =
        row(40, "2026-09-03T00:00:00Z", "issue", 8, "labeled", Some("loom:issue"));
    let p: Value = serde_json::from_str(object_payload["payload"].as_str().unwrap()).unwrap();
    object_payload["payload"] = p;
    let other_repo = {
        let mut r = row(41, "2026-09-03T00:00:00Z", "issue", 8, "opened", None);
        r["repo"] = json!("2AMLogic/loom-ui");
        r
    };
    let doc = json!([{
        "results": [
            object_payload,
            other_repo,
            {"id": 42, "kind": "eta.snapshot", "payload": "{}"},
            {"id": 43, "kind": "label.transition", "payload": "not json"},
            row(44, "2026-09-03T00:00:00Z", "issue", 8, "edited", None),
        ],
        "success": true,
    }]);
    let parsed = parse_export(&doc.to_string(), fetched());
    assert_eq!(parsed.by_repo[REPO].len(), 1);
    assert_eq!(parsed.by_repo["2amlogic/loom-ui"].len(), 1);
    assert_eq!(parsed.other_kind, 1);
    assert_eq!(parsed.unparseable, 2);
    assert_eq!(parsed.mapped(), 2);

    let lines =
        format!("{}\nnot json\n\n", row(45, "2026-09-03T00:00:00Z", "issue", 8, "opened", None));
    let parsed = parse_export(&lines, fetched());
    assert_eq!((parsed.mapped(), parsed.unparseable), (1, 1));
}
