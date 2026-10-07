//! The selection boundary of `TIMELINE_SQL` (#10519): which **raw**
//! `signoz_logs.distributed_logs_v2` rows the query admits, and the resolved
//! `kind` / `repo` columns it hands to `parse_row`.
//!
//! ClickHouse cannot run here, so this is a query-contract test in two parts:
//!
//! 1. The resolution expressions are pinned verbatim in the SQL (plus
//!    structural checks that the webhook body fallback reaches both the `kind`
//!    and the `repo` predicate and that the `WHERE` filters on the resolved
//!    repo, not the raw attribute). Any edit to them fails here first.
//! 2. [`select`] is a line-for-line model of exactly those pinned expressions
//!    over a raw row. It shows a webhook export row (empty attribute maps,
//!    `kind` / `repo` only in the JSON body) is selected and parses, that the
//!    daemon path is unchanged, and that the body fallback is scoped to the
//!    webhook service. Because part 1 pins the text the model mirrors, the
//!    model cannot drift from the SQL without a failure.
//!
//! Not covered (needs a live ClickHouse): that ClickHouse evaluates the
//! pinned text as modelled, e.g. `JSONExtractString` on a non-JSON body
//! returning `''`.

use crate::eta::fleet_signoz_timeline_rows::{
    parse_row, ParsedRow, RowBody, Source, Target, Transition, SERVICE_WEBHOOK, TIMELINE_SQL,
};
use serde_json::{json, Map, Value};

const REPO: &str = "rjwalters/loom";

/// The `kind` column, verbatim.
const KIND_EXPR: &str = "\
multiIf(attributes_string['loom.kind'] != '', attributes_string['loom.kind'],
            service = 'loom-ui-d1-export', JSONExtractString(body, 'kind'),
            body) AS kind,";

/// The `repo` column, verbatim.
const REPO_EXPR: &str = "\
multiIf(kind = 'queue.snapshot', {repo:String},
            attributes_string['loom.repo'] != '', attributes_string['loom.repo'],
            service = 'loom-ui-d1-export', JSONExtractString(body, 'repo'),
            '') AS repo,";

/// The repo predicate, verbatim.
const REPO_WHERE: &str = "AND lower(repo) = lower({repo:String})";

/// One raw log row: the columns the resolution reads.
struct Raw {
    attributes_string: Map<String, Value>,
    service: &'static str,
    body: String,
}

/// ClickHouse's `JSONExtractString(body, key)`: the top-level string value,
/// `''` when the body is not a JSON object or the key is absent / not a string.
fn json_extract_string(body: &str, key: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get(key).and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default()
}

fn attr(raw: &Raw, key: &str) -> String {
    raw.attributes_string
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// [`KIND_EXPR`], [`REPO_EXPR`], the kind list and [`REPO_WHERE`] over `raw`:
/// `Some((kind, repo))` when the query would select it.
fn select(raw: &Raw, repo: &str) -> Option<(String, String)> {
    let kind = if !attr(raw, "loom.kind").is_empty() {
        attr(raw, "loom.kind")
    } else if raw.service == "loom-ui-d1-export" {
        json_extract_string(&raw.body, "kind")
    } else {
        raw.body.clone()
    };
    let resolved_repo = if kind == "queue.snapshot" {
        repo.to_string()
    } else if !attr(raw, "loom.repo").is_empty() {
        attr(raw, "loom.repo")
    } else if raw.service == "loom-ui-d1-export" {
        json_extract_string(&raw.body, "repo")
    } else {
        String::new()
    };
    let kind_ok = crate::eta::fleet_signoz_timeline_rows::kind::ALL.contains(&kind.as_str());
    (kind_ok && resolved_repo.to_lowercase() == repo.to_lowercase())
        .then_some((kind, resolved_repo))
}

/// The `JSONEachRow` line the query emits for a selected `raw`.
fn emitted(raw: &Raw, kind: &str, repo: &str) -> String {
    json!({
        "record_id": "h:raw",
        "kind": kind,
        "service": raw.service,
        "repo": repo,
        "attrs": Value::Object(raw.attributes_string.clone()).to_string(),
        "nums": "{}",
        "body": raw.body,
        "event_time_ns": "1790848800000000000",
        "knowable_time_ns": "1791194400000000000",
    })
    .to_string()
}

/// A webhook export row as SigNoz stores it: no attributes, the D1 record
/// (the fixture's `h:w1`) as the body.
fn webhook_row(repo_in_body: &str) -> Raw {
    let payload = json!({"kind": "label.transition", "at": "2026-10-01T10:00:00Z",
                         "repo": repo_in_body, "target": "pr", "number": 900,
                         "action": "labeled", "labels_after": [],
                         "label": "loom:review-requested"});
    Raw {
        attributes_string: Map::new(),
        service: SERVICE_WEBHOOK,
        body: json!({"id": 1001, "kind": "label.transition", "repo": repo_in_body,
                     "emitted_at": "2026-10-01T10:00:00Z",
                     "payload": payload.to_string()})
        .to_string(),
    }
}

#[test]
fn the_query_pins_the_resolution_the_model_mirrors() {
    for (name, fragment) in [
        ("kind", KIND_EXPR),
        ("repo", REPO_EXPR),
        ("where", REPO_WHERE),
    ] {
        assert!(
            TIMELINE_SQL.contains(fragment),
            "TIMELINE_SQL's {name} no longer matches the modelled text:\n{fragment}"
        );
    }
}

#[test]
fn the_webhook_body_fallback_reaches_both_predicates_and_is_service_scoped() {
    let webhook = format!("service = '{SERVICE_WEBHOOK}'");
    for (expr, key) in [(KIND_EXPR, "kind"), (REPO_EXPR, "repo")] {
        assert!(expr.contains(&format!("JSONExtractString(body, '{key}')")));
        assert!(expr.contains(&webhook), "the {key} fallback is not scoped to the webhook");
    }
    assert!(TIMELINE_SQL.contains("resources_string['service.name'] AS service"));
    // The old predicate filtered the raw attribute, which a webhook row lacks.
    assert!(!TIMELINE_SQL.contains("lower(attributes_string['loom.repo'])"));
    // Point-in-time bounds and the keyset are unchanged.
    for clause in [
        "observed_timestamp >= {since_ns:UInt64}",
        "observed_timestamp <= {until_ns:UInt64}",
        "(observed_timestamp, record_id) > ({after_ns:UInt64}, {after_id:String})",
        "ORDER BY observed_timestamp, record_id",
    ] {
        assert!(TIMELINE_SQL.contains(clause), "TIMELINE_SQL lacks `{clause}`");
    }
}

#[test]
fn a_raw_webhook_row_with_empty_attribute_maps_is_selected_and_parses() {
    let raw = webhook_row("RJWalters/Loom");
    let (kind, repo) = select(&raw, REPO).expect("the webhook row is not selected");
    assert_eq!(kind, "label.transition");
    assert_eq!(repo, "RJWalters/Loom");
    let (_, parsed) = parse_row(&emitted(&raw, &kind, &repo), REPO).unwrap();
    let ParsedRow::Admitted(row) = parsed else {
        panic!("not admitted: {parsed:?}");
    };
    assert_eq!(row.source, Source::Webhook);
    assert!(matches!(
        row.body,
        RowBody::Label { ref item, ref label, transition: Transition::Added, .. }
            if item.target == Target::Pr && item.number == 900 && label == "loom:review-requested"
    ));
}

#[test]
fn a_webhook_row_for_another_repo_is_not_selected() {
    assert_eq!(select(&webhook_row("other/repo"), REPO), None);
}

#[test]
fn the_daemon_path_still_selects_by_attribute_and_never_by_body() {
    let attrs = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), json!(v)))
            .collect::<Map<String, Value>>()
    };
    let ci = Raw {
        attributes_string: attrs(&[("loom.kind", "ci.run"), ("loom.repo", REPO)]),
        service: "loom-daemon",
        body: "ci.run".to_string(),
    };
    assert_eq!(select(&ci, REPO), Some(("ci.run".into(), REPO.into())));
    let snapshot = Raw {
        attributes_string: Map::new(),
        service: "loom-daemon",
        body: "queue.snapshot".to_string(),
    };
    assert_eq!(select(&snapshot, REPO), Some(("queue.snapshot".into(), REPO.into())));
    // A non-webhook row with the record only in its body: not selected.
    let daemon_json = Raw {
        attributes_string: Map::new(),
        service: "loom-daemon",
        body: json!({"kind": "label.transition", "repo": REPO}).to_string(),
    };
    assert_eq!(select(&daemon_json, REPO), None);
}
