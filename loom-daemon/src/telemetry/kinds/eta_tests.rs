//! `eta.estimate` / `eta.outcome` registration and the provenance contract.

use super::*;
use crate::eta::heuristics::LandV1;
use crate::eta::score::{score, OutcomeKind};
use crate::eta::{AgeSource, CurrentStage, CurrentState, EstimateInput, Heuristic, Stage, Subject};
use crate::telemetry::{
    TelemetryEnvelope, TelemetryKindOtlp, TelemetryRecord, NEW_KIND_SCHEMA_VERSION, TELEMETRY_KINDS,
};
use chrono::{TimeZone, Utc};

fn provenance() -> Provenance {
    Provenance {
        version: "0.19.476".to_string(),
        revision: "9d8e226ce0123456789abcdef0123456789abcde".to_string(),
        tree_state: "clean".to_string(),
        complete: true,
    }
}

fn estimate() -> EtaEstimateRecord {
    let as_of = Utc.with_ymd_and_hms(2026, 9, 28, 12, 0, 0).unwrap();
    let input = EstimateInput {
        subject: Subject::new("rjwalters/loom", Some(1), 9289),
        as_of,
        current: CurrentState::At(CurrentStage {
            stage: Stage::ReviewWait,
            entered_at: Some(as_of),
            age_sec: 0,
            age_source: AgeSource::Bus,
            rework_rounds: 0,
        }),
        features: Default::default(),
        features_omitted: Vec::new(),
        provenance: provenance(),
        dispatch: None,
    };
    // No history: a refusal, which must carry provenance all the same.
    EtaEstimateRecord {
        trigger: Trigger::First,
        primary: true,
        explanation: Box::new(LandV1.estimate(&input, &Default::default())),
    }
}

fn outcome() -> EtaOutcomeRecord {
    let record = estimate();
    let summary = EstimateSummary::of(&record.explanation);
    let at = summary.as_of + chrono::Duration::seconds(600);
    EtaOutcomeRecord {
        score: score(&summary, OutcomeKind::Landed, at, &[]),
        estimate: summary,
        loom: Provenance {
            revision: "0123456789abcdef0123456789abcdef01234567".to_string(),
            ..provenance()
        },
        outcome_source: "pulls_read".to_string(),
        outcome_resolution_sec: Some(120),
        result: None,
    }
}

#[test]
fn both_eta_kinds_are_registered_otlp_logs_only() {
    for kind in ["eta.estimate", "eta.outcome"] {
        let meta = TELEMETRY_KINDS
            .iter()
            .find(|m| m.kind == kind)
            .unwrap_or_else(|| panic!("{kind} registered"));
        assert_eq!(meta.schema_version, NEW_KIND_SCHEMA_VERSION);
        assert_eq!(meta.schema_version, 12);
        assert_eq!(meta.otlp, TelemetryKindOtlp::Logs);
        // Operator decision on #9289: explanations and outcomes stay in SigNoz.
        assert!(!meta.native_ingest, "{kind} is OTLP-only");
    }
    let record = TelemetryRecord::EtaEstimate(estimate());
    assert_eq!(record.kind(), "eta.estimate");
    assert_eq!(record.otlp_class(), TelemetryKindOtlp::Logs);
    assert!(!record.accepted_by_native_ingest());
    let envelope = TelemetryEnvelope::new("host", TelemetryRecord::EtaOutcome(outcome()));
    assert_eq!(envelope.schema_version, 12);
}

#[test]
fn eta_records_round_trip_through_the_envelope() {
    for record in [
        TelemetryRecord::EtaEstimate(estimate()),
        TelemetryRecord::EtaOutcome(outcome()),
    ] {
        let envelope = TelemetryEnvelope::new("host", record);
        let json = serde_json::to_string(&envelope).unwrap();
        let back: TelemetryEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back, envelope);
    }
}

/// The operator requirement: neither kind can exist on the wire without the
/// heuristic id, version, full revision and tree state — and an outcome
/// without both provenances.
#[test]
fn eta_records_cannot_be_emitted_without_provenance() {
    let estimate_json = serde_json::to_value(TelemetryEnvelope::new(
        "host",
        TelemetryRecord::EtaEstimate(estimate()),
    ))
    .unwrap();
    for path in [
        &["explanation", "loom", "version"][..],
        &["explanation", "loom", "revision"],
        &["explanation", "loom", "tree_state"],
        &["explanation", "loom", "complete"],
        &["explanation", "loom"],
        &["explanation", "heuristic"],
    ] {
        let mut value = estimate_json.clone();
        let (last, parents) = path.split_last().unwrap();
        let mut node = &mut value["record"];
        for p in parents {
            node = &mut node[*p];
        }
        node.as_object_mut().unwrap().remove(*last);
        assert!(
            serde_json::from_value::<TelemetryEnvelope>(value).is_err(),
            "eta.estimate without {path:?} must not parse"
        );
    }

    let outcome_json = serde_json::to_value(TelemetryEnvelope::new(
        "host",
        TelemetryRecord::EtaOutcome(outcome()),
    ))
    .unwrap();
    for path in [
        &["loom", "revision"][..],
        &["loom", "version"],
        &["loom", "tree_state"],
        &["loom", "complete"],
        &["loom"],
        &["estimate", "loom", "revision"],
        &["estimate", "loom", "version"],
        &["estimate", "loom", "tree_state"],
        &["estimate", "loom", "complete"],
        &["estimate", "loom"],
        &["estimate", "heuristic"],
    ] {
        let mut value = outcome_json.clone();
        let (last, parents) = path.split_last().unwrap();
        let mut node = &mut value["record"];
        for p in parents {
            node = &mut node[*p];
        }
        node.as_object_mut().unwrap().remove(*last);
        assert!(
            serde_json::from_value::<TelemetryEnvelope>(value).is_err(),
            "eta.outcome without {path:?} must not parse"
        );
    }

    // Present but malformed provenance fails the emit gate.
    assert!(estimate().has_provenance());
    assert!(outcome().has_provenance());
    let mut short = estimate();
    short.explanation.loom.revision = "9d8e226".to_string();
    assert!(!short.has_provenance());
    let mut empty = outcome();
    empty.estimate.loom.version = String::new();
    assert!(!empty.has_provenance());
    let mut own = outcome();
    own.loom.tree_state = String::new();
    assert!(!own.has_provenance());
}

#[test]
fn an_outcome_keeps_both_builds() {
    let record = outcome();
    assert_ne!(record.loom.revision, record.estimate.loom.revision);
    let json = serde_json::to_value(&record).unwrap();
    assert_eq!(json["estimate"]["loom"]["revision"], "9d8e226ce0123456789abcdef0123456789abcde");
    assert_eq!(json["loom"]["revision"], "0123456789abcdef0123456789abcdef01234567");
}

#[test]
fn collector_keeps_every_eta_log_attribute() {
    const CONFIG: &str = include_str!("../../../../defaults/observability/collector/config.yaml");
    let log_keep = CONFIG
        .lines()
        .find(|l| {
            l.contains("keep_keys(attributes, [")
                && l.contains("loom.ci.chunk_index")
                && l.contains("loom.eta.estimate_id")
        })
        .expect("the transform/privacy log keep_keys line");
    for key in ETA_LOG_ATTRIBUTE_KEYS {
        assert!(log_keep.contains(&format!("\"{key}\"")), "collector drops {key}");
    }
    for generic in ["loom.repo", "loom.issue", "loom.pr_number"] {
        assert!(log_keep.contains(&format!("\"{generic}\"")));
    }
}

#[test]
fn accuracy_queries_group_by_revision_and_exclude_incomplete_provenance() {
    const QUERIES: &str = include_str!("../../../../defaults/observability/signoz/eta-queries.sql");
    for section in ["-- Q1.", "-- Q2.", "-- Q3."] {
        let start = QUERIES
            .find(section)
            .unwrap_or_else(|| panic!("{section} present"));
        let rest = &QUERIES[start + section.len()..];
        let body = &rest[..rest.find("\n-- Q").unwrap_or(rest.len())];
        assert!(body.contains("revision"), "{section} groups by revision");
        assert!(
            body.contains("attributes_bool['loom.eta.provenance_complete'] = true")
                && body.contains("attributes_bool['loom.eta.outcome_provenance_complete'] = true"),
            "{section} excludes incomplete provenance"
        );
    } // #10233: Q4 and Q6 read outcome rows, so both builds must be pinned; Q5
      // and Q7 read estimate rows (Q7's outcome rows only end a series), so the
      // estimating build must be.
    for (section, outcome_rows) in [
        ("-- Q4.", true),
        ("-- Q5.", false),
        ("-- Q6.", true),
        ("-- Q7.", false),
    ] {
        let start = QUERIES
            .find(section)
            .unwrap_or_else(|| panic!("{section} present"));
        let rest = &QUERIES[start + section.len()..];
        let body = &rest[..rest.find("\n-- Q").unwrap_or(rest.len())];
        assert!(body.contains("revision"), "{section} groups by revision");
        assert!(
            body.contains("attributes_bool['loom.eta.provenance_complete'] = true"),
            "{section} excludes an unpinned estimating build"
        );
        assert_eq!(
            body.contains("attributes_bool['loom.eta.outcome_provenance_complete'] = true"),
            outcome_rows,
            "{section}: the observing build's flag exactly where outcome rows are scored"
        );
    }
}
