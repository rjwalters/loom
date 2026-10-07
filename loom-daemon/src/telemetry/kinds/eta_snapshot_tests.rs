//! `eta.snapshot` registration and its wire contract (Issue #9329).

use super::*;
use crate::telemetry::{
    TelemetryEnvelope, TelemetryKindOtlp, TelemetryRecord, NEW_KIND_SCHEMA_VERSION, TELEMETRY_KINDS,
};
use chrono::{TimeZone, Utc};

fn row() -> EtaSnapshotRow {
    EtaSnapshotRow {
        repo: "rjwalters/loom".to_string(),
        visibility: RepoVisibility::Public,
        issue: 9329,
        pr: Some(9755),
        kind: Kind::Land,
        p25: Some(1_200),
        p50: Some(3_600),
        p75: Some(9_000),
        heuristic: "land-v1".to_string(),
        estimate_id: "b3c1d2e4f5a60718".to_string(),
        as_of: Utc.with_ymd_and_hms(2026, 9, 30, 12, 0, 0).unwrap(),
        stage: Some(Stage::ReviewWait),
        no_estimate_reason: None,
        alternates: Vec::new(),
    }
}

fn record() -> EtaSnapshotRecord {
    let row = row();
    EtaSnapshotRecord {
        as_of: row.as_of,
        rows: vec![row],
        rows_truncated: 0,
        rows_truncated_by_kind: Default::default(),
    }
}

/// The routing decision operator decision 7 on #9289 asked for: the live
/// list is a dashboard state key, so it goes to the native HTTPS backend
/// only — the mirror of `queue.snapshot`, and the opposite of the
/// OTLP-only `eta.estimate` / `eta.outcome` beside it.
#[test]
fn eta_snapshot_is_registered_native_only() {
    let meta = TELEMETRY_KINDS
        .iter()
        .find(|m| m.kind == "eta.snapshot")
        .expect("eta.snapshot registered");
    assert_eq!(meta.variant, "EtaSnapshot");
    assert_eq!(meta.schema_version, NEW_KIND_SCHEMA_VERSION);
    assert_eq!(meta.schema_version, 12);
    assert_eq!(meta.otlp, TelemetryKindOtlp::NotExported);
    assert!(meta.native_ingest, "eta.snapshot is native-HTTPS only, not OTLP-only");
    assert!(meta.otlp.signal().is_none());

    // The two estimate kinds keep the opposite routing: this record is an
    // addition beside them, never a replacement for them.
    for otlp_only in ["eta.estimate", "eta.outcome"] {
        let meta = TELEMETRY_KINDS
            .iter()
            .find(|m| m.kind == otlp_only)
            .unwrap_or_else(|| panic!("{otlp_only} registered"));
        assert!(!meta.native_ingest, "{otlp_only} must stay OTLP-only");
    }

    let record = TelemetryRecord::EtaSnapshot(record());
    assert_eq!(record.kind(), "eta.snapshot");
    assert_eq!(record.otlp_class(), TelemetryKindOtlp::NotExported);
    assert!(record.accepted_by_native_ingest());
    assert_eq!(TelemetryEnvelope::new("host", record).schema_version, 12);
}

#[test]
fn eta_snapshot_round_trips_through_the_envelope() {
    let envelope = TelemetryEnvelope::new("host", TelemetryRecord::EtaSnapshot(record()));
    let json = serde_json::to_string(&envelope).unwrap();
    let back: TelemetryEnvelope = serde_json::from_str(&json).unwrap();
    assert_eq!(back, envelope);
}

/// The row's field set is operator decision 7 verbatim, plus the
/// `visibility` tag every per-repo row carries. Pinned because loom-ui reads
/// these names: renaming one is a wire break, and adding one silently widens
/// what a host publishes about a private repo.
#[test]
fn a_row_carries_exactly_the_agreed_fields() {
    let wire = serde_json::to_value(row()).unwrap();
    let mut keys: Vec<&str> = wire
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "as_of",
            "estimate_id",
            "heuristic",
            "issue",
            "kind",
            "p25",
            "p50",
            "p75",
            "pr",
            "repo",
            "stage",
            "visibility",
        ]
    );
    assert_eq!(wire["kind"], "land");
    assert_eq!(wire["stage"], "review_wait");
    assert_eq!(wire["visibility"], "public");
}

/// Absent is never zero: a refusal's quantiles are missing from the wire, not
/// sent as `0`, and `no_estimate_reason` says why.
#[test]
fn a_refusals_quantiles_are_absent_and_its_reason_is_present() {
    let refusal = EtaSnapshotRow {
        p25: None,
        p50: None,
        p75: None,
        no_estimate_reason: Some(NoEstimateReason::Blocked),
        ..row()
    };
    let wire = serde_json::to_value(&refusal).unwrap();
    for absent in ["p25", "p50", "p75"] {
        assert!(wire.get(absent).is_none(), "{absent} must be absent, not 0: {wire}");
    }
    assert_eq!(wire["no_estimate_reason"], "blocked");
    let back: EtaSnapshotRow = serde_json::from_value(wire).unwrap();
    assert_eq!(back, refusal);
}

/// An older reader's record (no `rows_truncated`) still decodes, and an
/// untagged row is private — the default every per-repo record shares.
#[test]
fn missing_optional_fields_decode_to_the_safe_default() {
    let decoded: EtaSnapshotRecord = serde_json::from_value(serde_json::json!({
        "as_of": "2026-09-30T12:00:00Z",
        "rows": [{
            "repo": "acme/secret",
            "issue": 1,
            "kind": "start",
            "heuristic": "start-v1",
            "estimate_id": "abc",
            "as_of": "2026-09-30T12:00:00Z"
        }]
    }))
    .unwrap();
    assert_eq!(decoded.rows_truncated, 0);
    assert_eq!(decoded.rows[0].visibility, RepoVisibility::Private);
    assert_eq!(decoded.rows[0].kind, Kind::Start);
    assert_eq!(decoded.rows[0].stage, None);
}

fn alt_estimating() -> EtaSnapshotAlternate {
    EtaSnapshotAlternate {
        heuristic: "land-2026-10-04-twin-otter".to_string(),
        tier: Some(Tier::Candidate),
        estimate_id: "0a1b2c3d4e5f6071".to_string(),
        as_of: Utc.with_ymd_and_hms(2026, 9, 30, 12, 5, 0).unwrap(),
        p25: Some(100),
        p50: Some(200),
        p75: Some(300),
        p90: Some(400),
        no_estimate_reason: None,
    }
}

fn alt_refusing() -> EtaSnapshotAlternate {
    EtaSnapshotAlternate {
        p25: None,
        p50: None,
        p75: None,
        p90: None,
        no_estimate_reason: Some(NoEstimateReason::NoModel),
        ..alt_estimating()
    }
}

fn keys(v: &serde_json::Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    k.sort();
    k
}

/// A row with no alternates serializes with no `alternates` key.
#[test]
fn a_row_without_alternates_omits_the_key() {
    let wire = serde_json::to_value(row()).unwrap();
    assert!(wire.get("alternates").is_none(), "{wire}");
}

#[test]
fn an_alternate_carries_exactly_the_agreed_fields() {
    let est = serde_json::to_value(alt_estimating()).unwrap();
    assert_eq!(
        keys(&est),
        [
            "as_of",
            "estimate_id",
            "heuristic",
            "p25",
            "p50",
            "p75",
            "p90",
            "tier"
        ]
    );
    let refusal = serde_json::to_value(alt_refusing()).unwrap();
    assert_eq!(
        keys(&refusal),
        [
            "as_of",
            "estimate_id",
            "heuristic",
            "no_estimate_reason",
            "tier"
        ]
    );
    assert_eq!(refusal["no_estimate_reason"], "no_model");
    assert_eq!(est["tier"], "candidate");
}

/// Shape pinned against loom-ui `test/etaState.test.ts`
/// "normalizeEtaSnapshot alternates (#1669)": integer seconds, RFC 3339
/// `as_of`, `{heuristic, estimate_id, p25, p50, p75}` / `{..., no_estimate_reason}`.
#[test]
fn alternates_round_trip_and_match_the_loom_ui_fixture_shape() {
    let with = EtaSnapshotRow {
        alternates: vec![alt_estimating(), alt_refusing()],
        ..row()
    };
    let wire = serde_json::to_value(&with).unwrap();
    let a = &wire["alternates"];
    assert!(a[0]["p50"].is_i64());
    assert_eq!(a[0]["as_of"], "2026-09-30T12:05:00Z");
    assert_eq!(a[0]["heuristic"], "land-2026-10-04-twin-otter");
    assert!(a[1].get("p50").is_none());
    assert_eq!(a[1]["no_estimate_reason"], "no_model");
    let back: EtaSnapshotRow = serde_json::from_value(wire).unwrap();
    assert_eq!(back, with);

    // An older row (no `alternates`) decodes to an empty list.
    let mut old = serde_json::to_value(row()).unwrap();
    old.as_object_mut().unwrap().remove("alternates");
    let back: EtaSnapshotRow = serde_json::from_value(old).unwrap();
    assert!(back.alternates.is_empty());
}

#[path = "eta_snapshot_contract_tests.rs"]
mod contract;
