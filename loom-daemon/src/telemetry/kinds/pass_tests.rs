//! `pass.summary` / `pass.verdict` (#10752): wire shape, registry rows, and
//! the gateway collector contract for their `loom.pass.*` attributes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use chrono::{TimeZone, Utc};

use super::*;
use crate::telemetry::kinds::{TelemetryKindOtlp, NEW_KIND_SCHEMA_VERSION, TELEMETRY_KINDS};
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

fn provenance() -> Provenance {
    Provenance {
        version: "0.19.853".to_string(),
        revision: "2889d5ed585b4da0935543cb916381cc03b19746".to_string(),
        tree_state: "clean".to_string(),
        complete: true,
    }
}

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
        verdicts: [("released", 1), ("skipped", 40), ("still_blocked", 5)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        skipped: [("no-park-record", 30), ("unstated", 10)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        write_cap_hit: false,
        github: GithubSpend {
            calls: 9,
            writes: 2,
            not_modified: 5,
        },
        verdicts_emitted: 3,
        verdicts_unchanged: 43,
        loom: provenance(),
    }
}

fn verdict() -> PassVerdictRecord {
    PassVerdictRecord {
        pass_id: "0123456789abcdef0123456789abcdef".to_string(),
        mechanism: "stale_blocked_release".to_string(),
        role: None,
        repo: "rjwalters/loom".to_string(),
        number: 10752,
        artifact: "issue".to_string(),
        verdict: "released".to_string(),
        reason: None,
        detail: None,
        blockers: vec![BlockerState {
            reference: "#10753".to_string(),
            state: "closed".to_string(),
        }],
        labels_added: vec!["loom:issue".to_string()],
        labels_removed: vec!["loom:blocked".to_string()],
        mode: PassMode::On,
        applied: true,
        at: Utc.with_ymd_and_hms(2026, 10, 7, 12, 0, 1).unwrap(),
    }
}

#[test]
fn both_kinds_round_trip_with_their_wire_tags() {
    for (record, tag) in [
        (TelemetryRecord::PassSummary(summary()), "pass.summary"),
        (TelemetryRecord::PassVerdict(verdict()), "pass.verdict"),
    ] {
        let envelope = TelemetryEnvelope::new("loom-worker-1", record.clone());
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["record"]["kind"], tag, "{json}");
        let back: TelemetryEnvelope = serde_json::from_value(json).unwrap();
        assert_eq!(back.record, record);
        assert_eq!(envelope.schema_version, NEW_KIND_SCHEMA_VERSION);
    }
}

#[test]
fn the_verdict_body_uses_ref_and_omits_empty_fields() {
    let mut v = verdict();
    v.labels_added.clear();
    let json = serde_json::to_value(&v).unwrap();
    assert_eq!(json["blockers"][0]["ref"], "#10753");
    assert!(json.get("labels_added").is_none());
    assert!(json.get("role").is_none(), "a daemon pass has no role");
    assert_eq!(json["mode"], "on");
}

#[test]
fn both_kinds_are_otlp_only_logs() {
    for tag in ["pass.summary", "pass.verdict"] {
        let row = TELEMETRY_KINDS.iter().find(|k| k.kind == tag).unwrap();
        assert_eq!(row.otlp, TelemetryKindOtlp::Logs);
        assert!(!row.native_ingest);
        assert_eq!(row.schema_version, NEW_KIND_SCHEMA_VERSION);
    }
}

#[test]
fn collector_keeps_every_pass_log_attribute() {
    const CONFIG: &str = include_str!("../../../../defaults/observability/collector/config.yaml");
    let log_keep = CONFIG
        .lines()
        .find(|l| {
            l.contains("keep_keys(attributes, [")
                && l.contains("loom.ci.chunk_index")
                && l.contains("loom.eta.estimate_id")
        })
        .expect("the transform/privacy log keep_keys line");
    for key in PASS_LOG_ATTRIBUTE_KEYS
        .iter()
        .chain(&["loom.repo", "loom.role"])
    {
        assert!(log_keep.contains(&format!("\"{key}\"")), "collector drops {key}");
    }
}

/// `defaults/observability/signoz/pass-queries.sql` reads only keys a pass
/// record or an `invoke github` span exports (the OTLP mapping tests also
/// check each key's attribute map against its exported type).
#[test]
fn the_saved_queries_read_only_exported_keys() {
    const QUERIES: &str =
        include_str!("../../../../defaults/observability/signoz/pass-queries.sql");
    let mut read = 0;
    for chunk in QUERIES.split("attributes_").skip(1) {
        let Some((_, rest)) = chunk.split_once("['") else {
            continue;
        };
        let Some((key, _)) = rest.split_once("']") else {
            continue;
        };
        read += 1;
        let known = PASS_LOG_ATTRIBUTE_KEYS.contains(&key)
            || crate::gh_invocation::telemetry::SPAN_ATTRIBUTE_KEYS.contains(&key)
            || matches!(key, "loom.kind" | "loom.repo" | "loom.role");
        assert!(known, "pass-queries.sql reads {key}, which nothing exports");
    }
    assert!(read > 10, "found the queries' keys");
    for kind in ["'pass.summary'", "'pass.verdict'", "'invoke github'"] {
        assert!(QUERIES.contains(kind), "pass-queries.sql no longer reads {kind}");
    }
}
