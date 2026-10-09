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
//! The same holds for the `identity` column (#10671): [`identity`] models
//! [`IDENTITY_EXPR`], and the tests show both exporters' copies of one D1
//! record resolve to one identity.
//!
//! Not covered (needs a live ClickHouse): that ClickHouse evaluates the
//! pinned text as modelled, e.g. `JSONExtractString` on a non-JSON body
//! returning `''`. The #10671 query was run against live SigNoz once by
//! hand (alias references and `[-1]` indexing included).

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

/// The `identity` column, verbatim.
const IDENTITY_EXPR: &str = "\
multiIf(attributes_string['loom.record_id'] != '', record_id,
            kind = 'label.transition' AND attributes_string['loom.delivery_id'] != '',
            concat('dlv:', attributes_string['loom.delivery_id']),
            kind = 'label.transition' AND attributes_string['loom.record.delivery_id'] != '',
            concat('dlv:', attributes_string['loom.record.delivery_id']),
            attributes_string['d1sync.record'] != '',
            concat('d1:', replaceOne(splitByChar('/', attributes_string['d1sync.record'])[-1], '#', ':')),
            attributes_string['loom.export.id'] != '', concat('d1:', attributes_string['loom.export.id']),
            record_id) AS identity,";

/// [`IDENTITY_EXPR`] over `raw` of the resolved `kind`, whose row cursor is
/// `record_id`.
fn identity(raw: &Raw, kind: &str, record_id: &str) -> String {
    let label = kind == "label.transition";
    if !attr(raw, "loom.record_id").is_empty() {
        record_id.to_string()
    } else if label && !attr(raw, "loom.delivery_id").is_empty() {
        format!("dlv:{}", attr(raw, "loom.delivery_id"))
    } else if label && !attr(raw, "loom.record.delivery_id").is_empty() {
        format!("dlv:{}", attr(raw, "loom.record.delivery_id"))
    } else if !attr(raw, "d1sync.record").is_empty() {
        let record = attr(raw, "d1sync.record");
        let last = record.rsplit('/').next().unwrap_or_default();
        format!("d1:{}", last.replacen('#', ":", 1))
    } else if !attr(raw, "loom.export.id").is_empty() {
        format!("d1:{}", attr(raw, "loom.export.id"))
    } else {
        record_id.to_string()
    }
}

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

/// A webhook export row as SigNoz stores it (live shape, #10671): no
/// `loom.kind` / `loom.repo` attribute, the D1 record as a flat JSON body.
fn webhook_row(repo_in_body: &str) -> Raw {
    Raw {
        attributes_string: Map::new(),
        service: SERVICE_WEBHOOK,
        body: json!({"kind": "label.transition", "at": "2026-10-01T10:00:00.000Z",
                     "repo": repo_in_body, "target": "pr", "number": 900,
                     "action": "labeled", "labels_after": ["loom:review-requested"],
                     "label": "loom:review-requested"})
        .to_string(),
    }
}

#[test]
fn the_query_pins_the_resolution_the_model_mirrors() {
    for (name, fragment) in [
        ("kind", KIND_EXPR),
        ("repo", REPO_EXPR),
        ("where", REPO_WHERE),
        ("identity", IDENTITY_EXPR),
        ("scope", "scope_name AS scope,"),
        ("bools", "toJSONString(attributes_bool) AS bools,"),
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

fn strings(pairs: &[(&str, &str)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), json!(v)))
        .collect()
}

/// Both exporters' copies of one D1 record share one identity: the delivery
/// for a label row, the D1 record id for any other kind.
#[test]
fn both_exporters_copies_of_one_record_resolve_to_one_identity() {
    let export = |kind: &str, delivery: Option<&str>| Raw {
        attributes_string: strings(
            &[("loom.export.id", "records:1092860")]
                .into_iter()
                .chain(delivery.map(|d| ("loom.record.delivery_id", d)))
                .collect::<Vec<_>>(),
        ),
        service: SERVICE_WEBHOOK,
        body: json!({"kind": kind, "repo": REPO}).to_string(),
    };
    let d1sync = |delivery: Option<&str>| Raw {
        attributes_string: strings(
            &[
                ("loom.repo", REPO),
                ("d1sync.record", "loom-fleet-telemetry/records#1092860"),
            ]
            .into_iter()
            .chain(delivery.map(|d| ("loom.delivery_id", d)))
            .collect::<Vec<_>>(),
        ),
        service: "loom",
        body: "label.transition".to_string(),
    };
    let dlv = Some("82392400-c0c1-11f1-94e9-ea984e41c86c");
    let label = "label.transition";
    let want = "dlv:82392400-c0c1-11f1-94e9-ea984e41c86c";
    assert_eq!(identity(&export(label, dlv), label, "h:1"), want);
    assert_eq!(identity(&d1sync(dlv), label, "h:2"), want);
    // Without a delivery (CI, queue): the D1 record id, from either form.
    assert_eq!(identity(&export("ci.job", None), "ci.job", "h:3"), "d1:records:1092860");
    assert_eq!(identity(&d1sync(None), "ci.job", "h:4"), "d1:records:1092860");
    // A delivery never keys a non-label record (one CI delivery, many records).
    assert_eq!(identity(&export("ci.job", dlv), "ci.job", "h:5"), "d1:records:1092860");
    // The daemon's rows keep their own identity.
    let daemon = Raw {
        attributes_string: strings(&[("loom.kind", "ci.run"), ("loom.repo", REPO)]),
        service: "loom",
        body: "ci.run".to_string(),
    };
    assert_eq!(identity(&daemon, "ci.run", "h:6"), "h:6");
}
