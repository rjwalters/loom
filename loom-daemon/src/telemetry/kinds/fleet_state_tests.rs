//! `fleet.state` registration, wire contract and chunking (Issue #10196).

use super::*;
use crate::telemetry::{
    TelemetryEnvelope, TelemetryKindOtlp, TelemetryRecord, NEW_KIND_SCHEMA_VERSION, TELEMETRY_KINDS,
};
use chrono::{Duration, TimeZone};

fn at(h: u32, m: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 4, h, m, 0).unwrap()
}

fn held_row() -> FleetStateRow {
    FleetStateRow {
        pr: Some(10267),
        host: Some("robb-studio".to_string()),
        slot: Some(FleetSlot::Regular),
        ..FleetStateRow::new(10196, FleetStage::SweepBuilder, at(12, 0))
    }
}

fn listed_row() -> FleetStateRow {
    FleetStateRow {
        entered_at_lower_bound: true,
        pr: Some(10250),
        ..FleetStateRow::new(10193, FleetStage::ReviewWait, at(11, 0))
    }
}

/// A realistic `ready_wait` row: rank and every ranking input, worst case
/// (starred, dated, level 2).
fn ready_row(issue: u32, rank: u32) -> FleetStateRow {
    FleetStateRow {
        entered_at_lower_bound: true,
        rank: Some(rank),
        star: true,
        star_at: Some(at(9, 30)),
        level: 2,
        fleet_priority: Some(100),
        created_at: Some(at(8, 0) - Duration::days(30)),
        ..FleetStateRow::new(issue, FleetStage::ReadyWait, at(10, 0))
    }
}

fn stamps() -> PlannerStamps {
    PlannerStamps {
        planner_version: "0.19.958".to_string(),
        planner_config_hash: "0123456789ab".to_string(),
        fleet_config_hash: Some("4f2a9c0d1e2b3a4c5d6e7f8091a2b3c4d5e6f708".to_string()),
    }
}

fn record() -> FleetStateRecord {
    FleetStateRecord {
        schema: FLEET_STATE_SCHEMA.to_string(),
        as_of: at(12, 5),
        anchor: true,
        anchor_as_of: at(12, 5),
        prev_as_of: None,
        chunk_index: 0,
        chunk_count: 1,
        stamps: stamps(),
        census_at: Some(at(12, 4)),
        slots: Some(FleetSlots {
            max_concurrent: 4,
            occupancy: Some(1),
        }),
        repos: vec![FleetStateRepo {
            repo: "rjwalters/loom".to_string(),
            visibility: RepoVisibility::Public,
            ready_complete: true,
            census: Some(FleetPrCensus {
                open: 2,
                by_stage: [("review_wait".to_string(), 2)].into_iter().collect(),
            }),
            rows: vec![listed_row(), held_row()],
            removed: Vec::new(),
        }],
    }
}

/// An anchor with `n` ready rows spread over three repos.
fn anchor_with(n: u32) -> FleetStateRecord {
    let mut r = record();
    r.repos = ["acme/a", "acme/b", "acme/c"]
        .iter()
        .enumerate()
        .map(|(i, repo)| FleetStateRepo {
            repo: (*repo).to_string(),
            visibility: RepoVisibility::Private,
            ready_complete: true,
            census: Some(FleetPrCensus::default()),
            rows: (0..n)
                .filter(|k| k % 3 == u32::try_from(i).unwrap())
                .map(|k| ready_row(100_000 + k, k + 1))
                .collect(),
            removed: Vec::new(),
        })
        .collect();
    r
}

/// OTLP-only log kind on the shared post-#8921 gate. It is not native: it is a
/// replay record, not a dashboard state key.
#[test]
fn fleet_state_is_registered_as_an_otlp_only_log_kind() {
    let meta = TELEMETRY_KINDS
        .iter()
        .find(|m| m.kind == "fleet.state")
        .expect("fleet.state registered");
    assert_eq!(meta.variant, "FleetState");
    assert_eq!(meta.schema_version, NEW_KIND_SCHEMA_VERSION);
    assert_eq!(meta.otlp, TelemetryKindOtlp::Logs);
    assert!(!meta.native_ingest);

    let record = TelemetryRecord::FleetState(record());
    assert_eq!(record.kind(), "fleet.state");
    assert_eq!(record.otlp_class(), TelemetryKindOtlp::Logs);
    assert!(!record.accepted_by_native_ingest());
    assert_eq!(TelemetryEnvelope::new("host", record).schema_version, NEW_KIND_SCHEMA_VERSION);
}

#[test]
fn fleet_state_round_trips_through_the_envelope() {
    let envelope = TelemetryEnvelope::new("host", TelemetryRecord::FleetState(record()));
    let json = serde_json::to_string(&envelope).unwrap();
    let back: TelemetryEnvelope = serde_json::from_str(&json).unwrap();
    assert_eq!(back, envelope);
}

fn keys(value: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}

/// The record header: no `rows_truncated` (there is no cap), chunk fields
/// and the three regime stamps at the top level.
#[test]
fn the_header_carries_chunks_and_stamps_and_no_truncation_field() {
    let wire = serde_json::to_value(record()).unwrap();
    assert_eq!(
        keys(&wire),
        vec![
            "anchor",
            "anchor_as_of",
            "as_of",
            "census_at",
            "chunk_count",
            "chunk_index",
            "fleet_config_hash",
            "planner_config_hash",
            "planner_version",
            "repos",
            "schema",
            "slots"
        ]
    );
    assert_eq!(wire["chunk_count"], 1);
    // A host with no fleet store omits the fleet stamp.
    let mut r = record();
    r.stamps.fleet_config_hash = None;
    assert!(serde_json::to_value(r)
        .unwrap()
        .get("fleet_config_hash")
        .is_none());
}

/// The per-item facts: stage, entered-at, PR, host and slot. Pinned,
/// because adding a field widens what a host publishes about a private repo.
#[test]
fn a_held_row_carries_stage_entered_at_pr_host_and_slot() {
    let wire = serde_json::to_value(held_row()).unwrap();
    assert_eq!(keys(&wire), vec!["entered_at", "host", "issue", "pr", "slot", "stage"]);
    assert_eq!(wire["stage"], "sweep.builder");
    assert_eq!(wire["slot"], "regular");
    assert_eq!(wire["host"], "robb-studio");
}

/// A row seen only through a review listing has no known host. Its host and
/// slot are absent, not guessed, and a lower-bound entry says so.
#[test]
fn a_listed_row_omits_host_and_slot_and_flags_a_lower_bound_entry() {
    let wire = serde_json::to_value(listed_row()).unwrap();
    assert_eq!(
        keys(&wire),
        vec![
            "entered_at",
            "entered_at_lower_bound",
            "issue",
            "pr",
            "stage"
        ]
    );
    assert_eq!(wire["entered_at_lower_bound"], true);
}

/// A `ready_wait` row carries this host's planner rank and the planner's
/// inputs: star, starred-at, level, fleet priority and the age instant.
#[test]
fn a_ready_row_carries_rank_and_the_planner_inputs() {
    let wire = serde_json::to_value(ready_row(7, 3)).unwrap();
    assert_eq!(
        keys(&wire),
        vec![
            "created_at",
            "entered_at",
            "entered_at_lower_bound",
            "fleet_priority",
            "issue",
            "level",
            "rank",
            "stage",
            "star",
            "star_at"
        ]
    );
    assert_eq!(wire["stage"], "ready_wait");
    assert_eq!(wire["rank"], 3);
    assert_eq!(wire["level"], 2);
    assert_eq!(wire["fleet_priority"], 100);
}

/// The neutral stage enum keeps the replay contract's wire strings.
#[test]
fn stage_wire_names_are_the_contract_names() {
    for (stage, name) in [
        (FleetStage::ReadyWait, "ready_wait"),
        (FleetStage::SweepCurator, "sweep.curator"),
        (FleetStage::SweepBuilder, "sweep.builder"),
        (FleetStage::ReviewWait, "review_wait"),
        (FleetStage::Doctor, "doctor"),
        (FleetStage::MergeWait, "merge_wait"),
        (FleetStage::MergeHold, "merge_hold"),
    ] {
        assert_eq!(stage.as_str(), name);
        assert_eq!(serde_json::to_value(stage).unwrap(), name);
    }
}

/// An absent census decodes as unknown, never as zero, and a missing
/// visibility decodes as private.
#[test]
fn absent_census_is_unknown_and_missing_visibility_is_private() {
    let repo: FleetStateRepo = serde_json::from_value(serde_json::json!({
        "repo": "acme/secret"
    }))
    .unwrap();
    assert_eq!(repo.census, None);
    assert_eq!(repo.visibility, RepoVisibility::Private);
    assert!(repo.rows.is_empty() && repo.removed.is_empty());
    // An older emitter's entry never claims a complete ready queue.
    assert!(!repo.ready_complete);
}

/// `ready_complete` is always on the wire, `false` included: a missing field
/// would read the same as an older emitter's.
#[test]
fn ready_complete_is_always_sent() {
    let mut r = record();
    for complete in [true, false] {
        r.repos[0].ready_complete = complete;
        let wire = serde_json::to_value(&r).unwrap();
        assert_eq!(wire["repos"][0]["ready_complete"], serde_json::json!(complete));
    }
}

#[test]
fn counts_sum_over_repos() {
    let mut r = record();
    r.repos[0].removed = vec![1, 2, 3];
    assert_eq!(r.row_count(), 2);
    assert_eq!(r.removed_count(), 3);
}

/// Today's queue (~3000 rows, worst-case row shape) is ONE record: no cap,
/// nothing dropped, `chunk_count = 1`.
#[test]
fn a_3000_row_anchor_is_one_record() {
    let anchor = anchor_with(3000);
    let chunks = split_into_chunks(anchor.clone(), CHUNK_BYTES);
    assert_eq!(chunks.len(), 1);
    assert_eq!((chunks[0].chunk_index, chunks[0].chunk_count), (0, 1));
    assert_eq!(chunks[0].row_count(), 3000);
    assert_eq!(chunks[0], anchor);
    assert!(serde_json::to_vec(&chunks[0]).unwrap().len() <= CHUNK_BYTES);
}

/// A 15000-row anchor splits into complete chunks: each under the byte
/// limit, all sharing `as_of`, numbered `0..chunk_count`, and their union is
/// exactly the original rows.
#[test]
fn a_15000_row_anchor_splits_into_complete_chunks_with_nothing_dropped() {
    let anchor = anchor_with(15_000);
    let chunks = split_into_chunks(anchor.clone(), CHUNK_BYTES);
    assert!(chunks.len() > 1, "{} chunk(s)", chunks.len());
    let count = u32::try_from(chunks.len()).unwrap();
    let mut rows: Vec<(String, FleetStateRow)> = Vec::new();
    for (i, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.chunk_index, u32::try_from(i).unwrap());
        assert_eq!(chunk.chunk_count, count);
        assert_eq!(chunk.as_of, anchor.as_of);
        assert_eq!(chunk.anchor_as_of, anchor.anchor_as_of);
        assert_eq!(chunk.stamps, anchor.stamps);
        assert!(chunk.anchor);
        let len = serde_json::to_vec(chunk).unwrap().len();
        assert!(len <= CHUNK_BYTES, "chunk {i} is {len} bytes");
        for repo in &chunk.repos {
            assert!(repo.census.is_some(), "a split repo restates its census");
            rows.extend(repo.rows.iter().map(|r| (repo.repo.clone(), r.clone())));
        }
    }
    let original: Vec<(String, FleetStateRow)> = anchor
        .repos
        .iter()
        .flat_map(|repo| repo.rows.iter().map(|r| (repo.repo.clone(), r.clone())))
        .collect();
    assert_eq!(rows.len(), 15_000);
    assert_eq!(rows, original, "every row, in order, exactly once");
}

/// `removed` entries are packed like rows, and a census-only repo survives.
#[test]
fn removals_and_census_only_repos_survive_a_split() {
    let mut r = record();
    r.anchor = false;
    r.repos = vec![
        FleetStateRepo {
            repo: "acme/a".to_string(),
            visibility: RepoVisibility::Private,
            ready_complete: true,
            census: None,
            rows: (0..40).map(|k| ready_row(k, k + 1)).collect(),
            removed: (1000..1400).collect(),
        },
        FleetStateRepo {
            repo: "acme/b".to_string(),
            visibility: RepoVisibility::Private,
            ready_complete: true,
            census: Some(FleetPrCensus::default()),
            rows: Vec::new(),
            removed: Vec::new(),
        },
    ];
    let chunks = split_into_chunks(r.clone(), 2_000);
    assert!(chunks.len() > 2);
    let rows: usize = chunks.iter().map(FleetStateRecord::row_count).sum();
    let removed: Vec<u32> = chunks
        .iter()
        .flat_map(|c| c.repos.iter().flat_map(|repo| repo.removed.clone()))
        .collect();
    assert_eq!(rows, 40);
    assert_eq!(removed, (1000..1400).collect::<Vec<_>>());
    assert!(chunks
        .iter()
        .any(|c| c.repos.iter().any(|repo| repo.repo == "acme/b")));
    for c in &chunks {
        assert!(serde_json::to_vec(c).unwrap().len() <= 2_000);
    }
}

/// A record from before the chunk fields decodes as one complete chunk.
#[test]
fn a_record_without_chunk_fields_is_one_complete_chunk() {
    let mut wire = serde_json::to_value(record()).unwrap();
    let obj = wire.as_object_mut().unwrap();
    obj.remove("chunk_index");
    obj.remove("chunk_count");
    let back: FleetStateRecord = serde_json::from_value(wire).unwrap();
    assert_eq!((back.chunk_index, back.chunk_count), (0, 1));
}

#[test]
fn collector_keeps_every_fleet_state_log_attribute() {
    const CONFIG: &str = include_str!("../../../../defaults/observability/collector/config.yaml");
    let log_keep = CONFIG
        .lines()
        .find(|l| {
            l.contains("keep_keys(attributes, [")
                && l.contains("loom.ci.chunk_index")
                && l.contains("loom.eta.estimate_id")
        })
        .expect("the transform/privacy log keep_keys line");
    for key in FLEET_STATE_LOG_ATTRIBUTE_KEYS
        .iter()
        .chain(&["loom.kind", "loom.record_id"])
    {
        assert!(log_keep.contains(&format!("\"{key}\"")), "collector drops {key}");
    }
    assert!(!log_keep.contains("loom.fleet.rows_truncated"), "no cap, no truncation key");
}
