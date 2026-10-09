//! OTLP mapping of `pass.summary` / `pass.verdict` (#10752).

use std::collections::{BTreeMap, BTreeSet};

use chrono::{TimeZone, Utc};
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::logs::v1::{LogRecord, SeverityNumber};

use super::super::log_record_for;
use crate::telemetry::kinds::pass::{
    BlockerState, GithubSpend, PassMode, PassOutcome, PassSummaryRecord, PassVerdictRecord,
    PASS_LOG_ATTRIBUTE_KEYS,
};
use crate::telemetry::provenance::Provenance;
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

const QUERIES: &str = include_str!("../../../../../defaults/observability/signoz/pass-queries.sql");

fn summary() -> PassSummaryRecord {
    let at = Utc.with_ymd_and_hms(2026, 10, 7, 12, 0, 0).unwrap();
    PassSummaryRecord {
        pass_id: "0123456789abcdef0123456789abcdef".to_string(),
        mechanism: "stale_blocked_release".to_string(),
        repo: "rjwalters/loom".to_string(),
        host: "loom-worker-1".to_string(),
        mode: PassMode::On,
        outcome: PassOutcome::Completed,
        refusal: None,
        started_at: at,
        ended_at: at + chrono::Duration::milliseconds(1500),
        duration_ms: 1500,
        examined: 46,
        verdicts: [("failed", 0), ("released", 1), ("skipped", 40)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        skipped: [("no-park-record", 30), ("unstated", 10)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        write_cap_hit: true,
        github: GithubSpend {
            calls: 9,
            writes: 2,
            not_modified: 5,
        },
        verdicts_emitted: 3,
        verdicts_unchanged: 43,
        loom: Provenance {
            version: "0.19.853".to_string(),
            revision: "2889d5ed585b4da0935543cb916381cc03b19746".to_string(),
            tree_state: "clean".to_string(),
            complete: true,
        },
    }
}

fn verdict() -> PassVerdictRecord {
    PassVerdictRecord {
        pass_id: "0123456789abcdef0123456789abcdef".to_string(),
        mechanism: "stale_blocked_release".to_string(),
        role: Some("guide".to_string()),
        repo: "rjwalters/loom".to_string(),
        number: 10752,
        artifact: "issue".to_string(),
        verdict: "released".to_string(),
        reason: Some("no-park-record".to_string()),
        detail: Some("free text stays in the body".to_string()),
        blockers: vec![
            BlockerState {
                reference: "#10753".to_string(),
                state: "closed".to_string(),
            },
            BlockerState {
                reference: "acme/app#7".to_string(),
                state: "not_read".to_string(),
            },
        ],
        labels_added: vec!["loom:issue".to_string()],
        labels_removed: vec!["loom:blocked".to_string()],
        mode: PassMode::On,
        applied: true,
        at: Utc.with_ymd_and_hms(2026, 10, 7, 12, 0, 1).unwrap(),
    }
}

fn log(record: TelemetryRecord) -> LogRecord {
    log_record_for(&TelemetryEnvelope::new("loom-worker-1", record)).unwrap()
}

fn attrs(log: &LogRecord) -> BTreeMap<String, Value> {
    log.attributes
        .iter()
        .filter_map(|kv| Some((kv.key.clone(), kv.value.as_ref()?.value.clone()?)))
        .collect()
}

fn string(value: Option<&Value>) -> &str {
    match value {
        Some(Value::StringValue(s)) => s,
        other => panic!("not a string: {other:?}"),
    }
}

fn int(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::IntValue(n)) => *n,
        other => panic!("not an int: {other:?}"),
    }
}

/// Every key either kind exports, beyond the two `log_record_for` adds.
fn allowed(key: &str) -> bool {
    PASS_LOG_ATTRIBUTE_KEYS.contains(&key)
        || matches!(key, "loom.repo" | "loom.role" | "loom.kind" | "loom.record_id")
}

#[test]
fn a_summary_is_one_log_at_its_end_with_its_counts_flattened() {
    let r = summary();
    let log = log(TelemetryRecord::PassSummary(r.clone()));
    assert_eq!(log.event_name, "pass.summary");
    assert_eq!(log.time_unix_nano, super::super::nanos(r.ended_at));
    assert_eq!(log.severity_number, SeverityNumber::Info as i32);
    let a = attrs(&log);
    assert!(a.keys().all(|k| allowed(k)), "{:?}", a.keys());
    assert_eq!(string(a.get("loom.kind")), "pass.summary");
    assert_eq!(string(a.get("loom.repo")), "rjwalters/loom");
    assert_eq!(string(a.get("loom.pass.mechanism")), "stale_blocked_release");
    assert_eq!(string(a.get("loom.pass.verdicts")), "failed=0,released=1,skipped=40");
    assert_eq!(string(a.get("loom.pass.skip_reasons")), "no-park-record=30,unstated=10");
    assert_eq!(int(a.get("loom.pass.skipped")), 40);
    assert_eq!(int(a.get("loom.pass.github_calls")), 9);
    assert_eq!(a.get("loom.pass.write_cap_hit"), Some(&Value::BoolValue(true)));
    // The body is the whole record.
    let Some(Value::StringValue(body)) = log.body.and_then(|b| b.value) else {
        panic!("string body")
    };
    let back: PassSummaryRecord = serde_json::from_str(&body).unwrap();
    assert_eq!(back, r);
}

#[test]
fn a_refused_pass_or_a_failed_write_is_a_warning() {
    let mut refused = summary();
    refused.outcome = PassOutcome::Refused;
    let log1 = log(TelemetryRecord::PassSummary(refused));
    assert_eq!(log1.severity_number, SeverityNumber::Warn as i32);
    let mut failed = summary();
    failed.verdicts.insert("failed".to_string(), 1);
    let log2 = log(TelemetryRecord::PassSummary(failed));
    assert_eq!(log2.severity_number, SeverityNumber::Warn as i32);
    let mut v = verdict();
    v.verdict = "failed".to_string();
    assert_eq!(
        log(TelemetryRecord::PassVerdict(v)).severity_number,
        SeverityNumber::Warn as i32
    );
}

#[test]
fn a_verdict_carries_its_artifact_blockers_and_labels_but_never_its_detail() {
    let r = verdict();
    let log = log(TelemetryRecord::PassVerdict(r.clone()));
    assert_eq!(log.event_name, "pass.verdict");
    assert_eq!(log.time_unix_nano, super::super::nanos(r.at));
    let a = attrs(&log);
    assert!(a.keys().all(|k| allowed(k)), "{:?}", a.keys());
    assert_eq!(int(a.get("loom.pass.number")), 10752);
    assert_eq!(string(a.get("loom.role")), "guide");
    assert_eq!(string(a.get("loom.pass.reason")), "no-park-record");
    assert_eq!(string(a.get("loom.pass.blockers")), "#10753=closed,acme/app#7=not_read");
    assert_eq!(string(a.get("loom.pass.labels_added")), "loom:issue");
    assert_eq!(string(a.get("loom.pass.labels_removed")), "loom:blocked");
    assert_eq!(a.get("loom.pass.applied"), Some(&Value::BoolValue(true)));
    assert!(a
        .values()
        .all(|v| *v != Value::StringValue("free text stays in the body".into())));
}

#[test]
fn optional_verdict_attributes_are_absent_not_empty() {
    let mut r = verdict();
    r.role = None;
    r.reason = None;
    r.blockers.clear();
    r.labels_added.clear();
    r.labels_removed.clear();
    let a = attrs(&log(TelemetryRecord::PassVerdict(r)));
    for key in [
        "loom.role",
        "loom.pass.reason",
        "loom.pass.blockers",
        "loom.pass.labels_added",
        "loom.pass.labels_removed",
    ] {
        assert!(!a.contains_key(key), "{key} must be absent");
    }
}

/// `(container, key)` for every `attributes_<type>['key']` the saved queries read.
fn sql_keys(sql: &str) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    let mut rest = sql;
    while let Some(i) = rest.find("attributes_") {
        rest = &rest[i + "attributes_".len()..];
        let Some(open) = rest.find("['") else { break };
        let container = &rest[..open];
        let after = &rest[open + 2..];
        let Some(close) = after.find("']") else { break };
        if container.chars().all(|c| c.is_ascii_lowercase()) {
            out.insert((container.to_string(), after[..close].to_string()));
        }
    }
    out
}

/// The saved queries read every `loom.pass.*` key from the map its exported
/// type lands in (`string`, `number`, `bool`), and every `github.*` span key
/// they read is one the facade exports and the gateway keeps.
#[test]
fn the_saved_queries_read_each_key_from_the_map_its_type_lands_in() {
    let mut types: BTreeMap<String, &str> = BTreeMap::new();
    for record in [
        TelemetryRecord::PassSummary(summary()),
        TelemetryRecord::PassVerdict(verdict()),
    ] {
        for (key, value) in attrs(&log(record)) {
            let container = match value {
                Value::StringValue(_) => "string",
                Value::IntValue(_) | Value::DoubleValue(_) => "number",
                Value::BoolValue(_) => "bool",
                other => panic!("{key}: unexpected {other:?}"),
            };
            types.insert(key, container);
        }
    }
    let keys = sql_keys(QUERIES);
    assert!(keys.len() > 10, "the parser found the queries' keys: {keys:?}");
    for (container, key) in &keys {
        if key.starts_with("github.") {
            assert_eq!(container, "string", "span attributes are strings: {key}");
            assert!(
                crate::gh_invocation::telemetry::SPAN_ATTRIBUTE_KEYS.contains(&key.as_str()),
                "{key} is not an invoke github span attribute"
            );
        } else {
            let exported = types
                .get(key)
                .unwrap_or_else(|| panic!("the queries read {key}, which no pass record exports"));
            assert_eq!(container, exported, "{key} lands in attributes_{exported}");
        }
    }
}

/// A dry-run pass reports the verdicts it planned (`report.acted` still counts
/// them) but removes nothing, so the saved queries must not fold them into the
/// applied release/re-park totals, and the per-artifact query must say whether
/// the newest verdict was written. No ClickHouse runs in CI, so this pins the
/// gating in the SQL against the mode strings the daemon actually exports.
#[test]
fn the_saved_queries_keep_dry_run_plans_out_of_applied_totals() {
    let mut on = summary();
    on.mode = PassMode::On;
    let mut dry = summary();
    dry.mode = PassMode::DryRun;
    let mode = |r: PassSummaryRecord| {
        string(attrs(&log(TelemetryRecord::PassSummary(r))).get("loom.pass.mode")).to_string()
    };
    let (on_mode, dry_mode) = (mode(on), mode(dry));
    assert_eq!((on_mode.as_str(), dry_mode.as_str()), ("on", "dry_run"));

    let flat: String = QUERIES.split_whitespace().collect::<Vec<_>>().join(" ");
    for (verdict, alias) in [
        ("released", "released"),
        ("reparked", "reparked"),
        ("failed", "failed"),
    ] {
        let applied = format!(
            "sumIf(JSONExtractUInt(body, 'verdicts', '{verdict}'), \
             attributes_string['loom.pass.mode'] = '{on_mode}') AS {alias}"
        );
        assert!(flat.contains(&applied), "{alias} must count mode {on_mode} only");
    }
    for verdict in ["released", "reparked"] {
        let planned = format!(
            "sumIf(JSONExtractUInt(body, 'verdicts', '{verdict}'), \
             attributes_string['loom.pass.mode'] = '{dry_mode}') AS planned_{verdict}"
        );
        assert!(flat.contains(&planned), "planned_{verdict} must count mode {dry_mode} only");
    }
    assert!(
        !flat.contains("sum(JSONExtractUInt(body, 'verdicts'"),
        "an ungated sum over body verdicts mixes dry-run plans into applied totals"
    );
    // Query 2 shows mode and applied beside the verdict.
    assert!(flat.contains("argMax(attributes_string['loom.pass.mode'], timestamp) AS mode"));
    assert!(flat.contains("argMax(attributes_bool['loom.pass.applied'], timestamp) AS applied"));
}
