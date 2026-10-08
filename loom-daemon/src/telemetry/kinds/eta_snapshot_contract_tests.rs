//! The `eta.snapshot` producer/consumer contract (#10391 slice 3).
//!
//! Two fixtures, both vendored so CI stays network-free:
//!
//! - `fixtures/eta-snapshot-golden.json` is what loom **produces**: the
//!   canonical record below, serialized exactly as it rides in an envelope's
//!   `record` field (the `kind`-tagged [`TelemetryRecord`]). loom-ui vendors
//!   it and runs both its parsers on it (2AMLogic/loom-ui#1854).
//! - `fixtures/loom-ui/eta-snapshot-consumer.json` is what loom-ui
//!   **consumes**, vendored from its own test (`.source` sidecar names the
//!   commit). Every key path in it must exist in the golden with a compatible
//!   JSON type — the check that would have caught #10390, where the consumer
//!   read `rows[].alternates[]` and the producer never sent it.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::*;
use crate::telemetry::TelemetryRecord;
use chrono::{TimeZone, Utc};

const GOLDEN: &str = include_str!("fixtures/eta-snapshot-golden.json");
const CONSUMER: &str = include_str!("fixtures/loom-ui/eta-snapshot-consumer.json");
const CONSUMER_SOURCE: &str = include_str!("fixtures/loom-ui/eta-snapshot-consumer.json.source");

fn at(h: u32, m: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 1, h, m, 0).unwrap()
}

/// The canonical record: a `land` row with an estimate and one estimating
/// (tiered) plus one refusing (untiered) alternate, a `land` refusal row, and
/// a `start` row — every field the wire can carry appears at least once.
fn canonical() -> EtaSnapshotRecord {
    let land = EtaSnapshotRow {
        repo: "rjwalters/loom".to_string(),
        visibility: RepoVisibility::Public,
        issue: 10391,
        pr: Some(10431),
        kind: Kind::Land,
        p25: Some(1_200),
        p50: Some(3_600),
        p75: Some(9_000),
        heuristic: "land-v1".to_string(),
        estimate_id: "b3c1d2e4f5a60718".to_string(),
        as_of: at(12, 0),
        stage: Some(Stage::ReviewWait),
        no_estimate_reason: None,
        alternates: vec![
            EtaSnapshotAlternate {
                heuristic: "land-2026-10-04-twin-otter".to_string(),
                tier: Some(Tier::Candidate),
                estimate_id: "0a1b2c3d4e5f6071".to_string(),
                as_of: at(11, 58),
                p25: Some(900),
                p50: Some(2_700),
                p75: Some(7_200),
                p90: Some(14_400),
                no_estimate_reason: None,
            },
            EtaSnapshotAlternate {
                heuristic: "land-2026-10-05-shadow".to_string(),
                // An id this build does not know: `tier` is absent (#10525).
                tier: None,
                estimate_id: "8192a3b4c5d6e7f8".to_string(),
                as_of: at(11, 58),
                p25: None,
                p50: None,
                p75: None,
                p90: None,
                no_estimate_reason: Some(NoEstimateReason::NoModel),
            },
        ],
        // #10929: the row's own per-stage forecast (loom-ui#2753).
        stages: BTreeMap::from([
            (
                Stage::ReviewWait,
                EtaSnapshotStage {
                    entry_p50: 0,
                    entry_p90: 0,
                    dwell_p50: 2_400,
                    dwell_p90: 10_800,
                    reach_pct: 100,
                },
            ),
            (
                Stage::Doctor,
                EtaSnapshotStage {
                    entry_p50: 2_700,
                    entry_p90: 9_000,
                    dwell_p50: 1_800,
                    dwell_p90: 5_400,
                    reach_pct: 30,
                },
            ),
            (
                Stage::MergeWait,
                EtaSnapshotStage {
                    entry_p50: 2_700,
                    entry_p90: 12_600,
                    dwell_p50: 600,
                    dwell_p90: 3_600,
                    reach_pct: 100,
                },
            ),
        ]),
    };
    let refusal = EtaSnapshotRow {
        repo: "acme/secret-app".to_string(),
        visibility: RepoVisibility::Private,
        issue: 42,
        pr: None,
        kind: Kind::Land,
        p25: None,
        p50: None,
        p75: None,
        heuristic: "land-v1".to_string(),
        estimate_id: "c4d5e6f708192a3b".to_string(),
        as_of: at(11, 55),
        stage: None,
        no_estimate_reason: Some(NoEstimateReason::Blocked),
        alternates: Vec::new(),
        stages: BTreeMap::new(),
    };
    let start = EtaSnapshotRow {
        repo: "rjwalters/loom".to_string(),
        visibility: RepoVisibility::Public,
        issue: 10477,
        pr: None,
        kind: Kind::Start,
        p25: Some(600),
        p50: Some(1_800),
        p75: Some(5_400),
        heuristic: "start-v1".to_string(),
        estimate_id: "d5e6f708192a3b4c".to_string(),
        as_of: at(11, 50),
        stage: Some(Stage::ReadyWait),
        no_estimate_reason: None,
        alternates: Vec::new(),
        stages: BTreeMap::new(),
    };
    // Cut-priority order (#10928): the estimating `land` row, then the
    // `land` refusal, then `start`. One row's alternates did not fit the
    // byte budget, so `alternates_truncated` is on the wire too.
    EtaSnapshotRecord {
        as_of: at(12, 0),
        rows: vec![land, refusal, start],
        rows_truncated: 1,
        alternates_truncated: 1,
        rows_truncated_by_kind: BTreeMap::from([(Kind::Finish, 1)]),
    }
}

/// The golden is the producer contract: serializing the canonical record
/// must reproduce it **byte for byte**.
///
/// Re-bless with `LOOM_ETA_BLESS=1` only for a deliberate wire change. **A
/// golden change requires re-vendoring it in loom-ui (2AMLogic/loom-ui#1854)**,
/// whose parsers are tested against this exact file.
#[test]
fn eta_snapshot_matches_golden_bytes() {
    let record = TelemetryRecord::EtaSnapshot(canonical());
    let mut actual = serde_json::to_string_pretty(&record).unwrap();
    actual.push('\n');
    // `GOLDEN` is compiled in, so a bless run compares against what it just
    // wrote; the next build picks the new bytes up.
    let golden = if std::env::var("LOOM_ETA_BLESS").is_ok_and(|v| v == "1") {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/telemetry/kinds/fixtures/eta-snapshot-golden.json"
        );
        std::fs::write(path, &actual).unwrap();
        actual.clone()
    } else {
        GOLDEN.to_string()
    };
    assert!(
        actual == golden,
        "eta.snapshot drifted from fixtures/eta-snapshot-golden.json; re-bless with \
         LOOM_ETA_BLESS=1 only for a deliberate wire change, then re-vendor it in \
         loom-ui (2AMLogic/loom-ui#1854)\n--- actual ---\n{actual}"
    );
    let back: TelemetryRecord = serde_json::from_str(&golden).unwrap();
    assert_eq!(back, record);
}

/// A JSON value's type, for the key-path comparison. Integers and floats are
/// one `Number`: JavaScript does not tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum JsonType {
    Null,
    Bool,
    Number,
    String,
    Array,
    Object,
}

fn json_type(v: &Value) -> JsonType {
    match v {
        Value::Null => JsonType::Null,
        Value::Bool(_) => JsonType::Bool,
        Value::Number(_) => JsonType::Number,
        Value::String(_) => JsonType::String,
        Value::Array(_) => JsonType::Array,
        Value::Object(_) => JsonType::Object,
    }
}

/// Every key path in `v` (`$`, `$.rows`, `$.rows[].alternates[].p50`, …)
/// with the set of types seen at it; array elements share one `[]` path.
fn key_paths(v: &Value) -> BTreeMap<String, BTreeSet<JsonType>> {
    fn walk(v: &Value, path: String, out: &mut BTreeMap<String, BTreeSet<JsonType>>) {
        out.entry(path.clone()).or_default().insert(json_type(v));
        match v {
            Value::Object(map) => {
                for (k, child) in map {
                    walk(child, format!("{path}.{k}"), out);
                }
            }
            Value::Array(items) => {
                for child in items {
                    walk(child, format!("{path}[]"), out);
                }
            }
            _ => {}
        }
    }
    let mut out = BTreeMap::new();
    walk(v, "$".to_string(), &mut out);
    out
}

/// The consumer's key paths the producer does not satisfy: missing outright,
/// or present with no compatible type. A consumer `null` matches any type.
fn unmet_paths(consumer: &Value, producer: &Value) -> Vec<String> {
    let produced = key_paths(producer);
    let mut unmet = Vec::new();
    for (path, types) in key_paths(consumer) {
        let Some(have) = produced.get(&path) else {
            unmet.push(format!("{path}: missing from the golden"));
            continue;
        };
        for t in types {
            if t != JsonType::Null && !have.contains(&t) {
                unmet.push(format!("{path}: consumer reads {t:?}, golden has {have:?}"));
            }
        }
    }
    unmet
}

fn golden() -> Value {
    serde_json::from_str(GOLDEN).unwrap()
}

fn consumer() -> Value {
    serde_json::from_str(CONSUMER).unwrap()
}

/// Every key path loom-ui's fixture reads exists in loom's golden with a
/// compatible type.
#[test]
fn every_consumer_key_path_exists_in_the_golden() {
    let unmet = unmet_paths(&consumer(), &golden());
    assert!(unmet.is_empty(), "loom-ui reads what loom does not send:\n{}", unmet.join("\n"));
}

/// The check bites: the #10390 failure — a golden without `alternates` — is
/// caught, and so is a type change.
#[test]
fn the_key_path_check_fails_without_alternates_or_on_a_type_change() {
    let mut stripped = golden();
    for row in stripped["rows"].as_array_mut().unwrap() {
        row.as_object_mut().unwrap().remove("alternates");
    }
    let unmet = unmet_paths(&consumer(), &stripped);
    assert!(
        unmet.contains(&"$.rows[].alternates: missing from the golden".to_string()),
        "{unmet:?}"
    );
    assert!(unmet
        .iter()
        .any(|u| u.starts_with("$.rows[].alternates[].no_estimate_reason")));

    let mut retyped = golden();
    for row in retyped["rows"].as_array_mut().unwrap() {
        row["issue"] = Value::String("42".to_string());
    }
    let unmet = unmet_paths(&consumer(), &retyped);
    assert_eq!(unmet, ["$.rows[].issue: consumer reads Number, golden has {String}"]);
}

/// The vendored fixture is exactly what its `.source` sidecar records: an
/// edit to it without a re-vendor (new commit and hashes) fails here.
#[test]
fn the_consumer_fixture_matches_its_source_sidecar() {
    let field = |key: &str| {
        CONSUMER_SOURCE
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(": "))
            .unwrap_or_else(|| panic!("sidecar lacks `{key}:`"))
    };
    assert_eq!(field("repo"), "2AMLogic/loom-ui");
    assert_eq!(field("commit").len(), 40, "a full commit sha");
    assert_eq!(field("source-sha256").len(), 64);
    let actual = hex::encode(Sha256::digest(CONSUMER.as_bytes()));
    assert_eq!(field("fixture-sha256"), actual, "re-vendor: update the sidecar");
}
