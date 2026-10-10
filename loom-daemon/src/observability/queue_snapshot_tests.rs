//! `queue.snapshot` building and routing tests (Issue #8852, phase 2).

use std::collections::HashMap;

use chrono::{TimeZone, Utc};

use super::{build_record, is_new_tick};
use crate::telemetry::queue_snapshot::{QueueRepoRef, QueueSnapshotRow, MAX_ROWS};
use crate::telemetry::{RepoVisibility, TelemetryEnvelope, TelemetryRecord};
use crate::types::{QueueDisposition as Qd, ReadyQueueRow, WorkFinderTickSummary};

const ROOT: &str = "/Users/someone/src/private-thing";
const PUBLIC_ROOT: &str = "/srv/loom";

fn row(rank: usize, repo: &str, issue: u32, d: Qd, detail: Option<&str>) -> ReadyQueueRow {
    ReadyQueueRow {
        rank,
        repo: repo.into(),
        issue,
        workspace_priority: 100,
        urgent: false,
        operator_priority: false,
        operator_priority_at: None,
        main_red_fix: false,
        created_at: Some("2026-09-01T00:00:00Z".into()),
        tier: Some("tier:goal-advancing".into()),
        story_points: None,
        disposition: d,
        detail: detail.map(str::to_string),
        state: d.state().into(),
        reason: d.reason().into(),
        plan: Default::default(),
    }
}

fn repos() -> HashMap<String, QueueRepoRef> {
    HashMap::from([
        (
            ROOT.to_string(),
            QueueRepoRef {
                repo: "acme/secret".into(),
                visibility: RepoVisibility::Private,
            },
        ),
        (
            PUBLIC_ROOT.to_string(),
            QueueRepoRef {
                repo: "rjwalters/loom".into(),
                visibility: RepoVisibility::Public,
            },
        ),
    ])
}

fn summary(queue: Vec<ReadyQueueRow>) -> WorkFinderTickSummary {
    WorkFinderTickSummary {
        at: Utc.timestamp_opt(1_790_000_000, 0).unwrap(),
        max_concurrent: 4,
        seen: queue.len(),
        queue,
        ..Default::default()
    }
}

#[test]
fn rows_carry_slug_and_visibility_never_a_local_path() {
    let s = summary(vec![
        row(1, PUBLIC_ROOT, 10, Qd::Dispatched, None),
        row(2, ROOT, 11, Qd::DeferredCapacity, None),
        row(3, "workspace #2", 12, Qd::PeerClaim, None),
    ]);
    let record = build_record(&s, &repos());
    assert_eq!(record.tick_at, s.at);
    assert_eq!((record.max_concurrent, record.seen), (4, 3));
    assert_eq!(record.rows.len(), 2);
    assert_eq!(record.unresolved_rows, 1);
    let first = &record.rows[0];
    assert_eq!((first.rank, first.repo.as_str(), first.issue), (1, "rjwalters/loom", 10));
    assert_eq!(first.visibility, RepoVisibility::Public);
    assert_eq!(first.state, "running");
    assert_eq!(first.reason, "dispatched this tick");
    assert_eq!(record.rows[1].visibility, RepoVisibility::Private);
    assert_eq!(record.rows[1].tier.as_deref(), Some("tier:goal-advancing"));
    // Counts cover every row, the unresolved one included.
    assert_eq!((record.counts.running, record.counts.ready, record.counts.blocked), (1, 1, 1));
    let json = serde_json::to_string(&record).unwrap();
    assert!(!json.contains("/Users/") && !json.contains("/srv/"), "{json}");
}

#[test]
fn only_structured_detail_is_exported() {
    let s = summary(vec![
        row(1, ROOT, 1, Qd::Parked, Some("loom:blocked")),
        row(2, ROOT, 2, Qd::OpenPr, Some("open PR #77")),
        row(3, ROOT, 3, Qd::DispatchError, Some("spawn failed: /Users/x/.loom/log")),
        row(4, ROOT, 4, Qd::DispatchBackoff, Some("backoff after: gh exploded")),
    ]);
    let details: Vec<Option<String>> = build_record(&s, &repos())
        .rows
        .into_iter()
        .map(|r| r.detail)
        .collect();
    assert_eq!(
        details,
        vec![
            Some("loom:blocked".into()),
            Some("open PR #77".into()),
            None,
            None
        ]
    );
}

#[test]
fn rows_past_the_cap_are_counted_not_sent() {
    let queue = (0..MAX_ROWS + 5)
        .map(|i| row(i + 1, ROOT, u32::try_from(i).unwrap(), Qd::DeferredCapacity, None))
        .collect();
    let record = build_record(&summary(queue), &repos());
    assert_eq!(record.rows.len(), MAX_ROWS);
    assert_eq!(record.rows_truncated, 5);
    assert_eq!(record.counts.ready, MAX_ROWS + 5);
}

#[test]
fn listing_failures_name_resolved_repos_and_count_the_rest() {
    let mut s = summary(Vec::new());
    s.listing_failed = vec![ROOT.to_string(), "workspace #3".to_string()];
    let record = build_record(&s, &repos());
    assert_eq!(record.listing_failed.len(), 1);
    assert_eq!(record.listing_failed[0].repo, "acme/secret");
    assert_eq!(record.listing_failed_unresolved, 1);
    assert!(record.rows.is_empty());
}

#[test]
fn a_partial_listing_is_reported_not_passed_off_as_whole() {
    // #11139: the partial repo's rows ARE exported, and the repo is named in
    // `listing_incomplete` so a consumer does not read missing rows as gone.
    let mut s = summary(vec![row(1, ROOT, 100, Qd::DeferredCapacity, None)]);
    s.listing_incomplete = vec![ROOT.to_string(), "workspace #3".to_string()];
    let record = build_record(&s, &repos());
    assert!(record.listing_failed.is_empty());
    assert_eq!(record.listing_incomplete.len(), 1);
    assert_eq!(record.listing_incomplete[0].repo, "acme/secret");
    assert_eq!(record.listing_incomplete_unresolved, 1);
    assert_eq!(record.rows.len(), 1);
    let json = serde_json::to_value(&record).unwrap();
    assert_eq!(json["listing_incomplete"][0]["repo"], "acme/secret");
    assert_eq!(json["listing_incomplete_unresolved"], 1);

    // A whole tick omits both fields on the wire (older readers unchanged).
    let whole = build_record(&summary(Vec::new()), &repos());
    let json = serde_json::to_value(&whole).unwrap();
    assert!(json.get("listing_incomplete").is_none());
    assert!(json.get("listing_incomplete_unresolved").is_none());
}

#[test]
fn only_a_newer_tick_is_emitted() {
    let t = Utc.timestamp_opt(1_790_000_000, 0).unwrap();
    assert!(is_new_tick(t, None));
    assert!(!is_new_tick(t, Some(t)));
    assert!(is_new_tick(t + chrono::Duration::seconds(1), Some(t)));
}

#[test]
fn envelope_is_gated_at_version_11_and_reaches_native_ingest() {
    let record = build_record(&summary(vec![row(1, ROOT, 5, Qd::InFlight, None)]), &repos());
    let envelope = TelemetryEnvelope::new("host-a", TelemetryRecord::QueueSnapshot(record));
    assert_eq!(envelope.schema_version, 11);
    let json = serde_json::to_value(&envelope).unwrap();
    assert_eq!(json["record"]["kind"], "queue.snapshot");
    assert_eq!(json["record"]["rows"][0]["visibility"], "private");
    assert_eq!(json["record"]["rows"][0]["disposition"], "in_flight");
    let back: TelemetryEnvelope = serde_json::from_value(json).unwrap();
    assert_eq!(back, envelope);
    assert_eq!(crate::observability::tracing::native_envelopes(&[envelope]).len(), 1);
}

#[test]
fn a_row_without_visibility_decodes_private() {
    let row: QueueSnapshotRow = serde_json::from_value(serde_json::json!({
        "rank": 1, "repo": "a/b", "issue": 1, "workspace_priority": 100, "urgent": false,
        "disposition": "dispatched", "state": "running", "reason": "dispatched this tick"
    }))
    .unwrap();
    assert_eq!(row.visibility, RepoVisibility::Private);
}

#[test]
fn rows_carry_operator_priority_and_urgent_is_always_false() {
    // #9244: starred rows ship `operator_priority` / `operator_priority_at`;
    // `urgent` stays on the wire for one release but is always false, even if
    // a (stale) summary row still claims it.
    let mut starred = row(1, PUBLIC_ROOT, 10, Qd::Dispatched, None);
    starred.urgent = true;
    starred.operator_priority = true;
    starred.operator_priority_at = Some("2026-09-27T08:00:00Z".into());
    let plain = row(2, PUBLIC_ROOT, 11, Qd::DeferredCapacity, None);
    let record = build_record(&summary(vec![starred, plain]), &repos());
    let wire = serde_json::to_value(&record.rows).unwrap();
    assert_eq!(wire[0]["urgent"], false);
    assert_eq!(wire[0]["operator_priority"], true);
    assert_eq!(wire[0]["operator_priority_at"], "2026-09-27T08:00:00Z");
    assert_eq!(wire[1]["operator_priority"], false);
    assert!(wire[1].get("operator_priority_at").is_none(), "{wire}");
    // An older daemon's row (no new fields) still decodes.
    let mut legacy = wire[1].clone();
    legacy.as_object_mut().unwrap().remove("operator_priority");
    let decoded: QueueSnapshotRow = serde_json::from_value(legacy).unwrap();
    assert!(!decoded.operator_priority);
}

/// Issue #9288: the plan's row fields and the per-tick block ride
/// `queue.snapshot` unchanged, flattened beside `rank`, without a
/// schema-version bump.
#[test]
fn plan_fields_ride_the_snapshot_at_version_11() {
    use crate::types::{
        DispatchPlanContext, PlanGate, PlanKey, PlanSlots, PlanState, RepoCapView, RowPlan,
    };
    let mut r = row(1, PUBLIC_ROOT, 5, Qd::DeferredRepoCap, None);
    r.plan = RowPlan {
        position: Some(3),
        plan_state: PlanState::Queued,
        keys: vec![PlanKey {
            name: "workspace_priority".into(),
            value: serde_json::json!(100),
        }],
        gate: Some(PlanGate::RepoCap),
        in_slice: Some(true),
        hot: Some(true),
        owning_shard: Some(1),
        repo_cap: Some(RepoCapView {
            cap: Some(1),
            occupancy: 1,
        }),
        // Issue #9311: additive, exercised on its own round-trip coverage in
        // `types::dispatch_plan::tests`; `None` here keeps this test's own
        // `schema_version` assertion about the pre-existing fields.
        held_until: None,
    };
    let mut s = summary(vec![r.clone()]);
    s.plan = Some(DispatchPlanContext {
        slots: PlanSlots {
            max_concurrent: 4,
            free: Some(2),
            ..PlanSlots::default()
        },
        tick_interval_secs: Some(60),
        ..DispatchPlanContext::default()
    });
    let record = build_record(&s, &repos());
    assert_eq!(record.rows[0].plan, r.plan);
    assert_eq!(record.plan, s.plan);
    let envelope = TelemetryEnvelope::new("host-a", TelemetryRecord::QueueSnapshot(record));
    assert_eq!(envelope.schema_version, 11, "an additive field is not a schema bump");
    let json = serde_json::to_value(&envelope).unwrap();
    let row0 = &json["record"]["rows"][0];
    assert_eq!(row0["position"], 3);
    assert_eq!(row0["plan_state"], "queued");
    assert_eq!(row0["gate"], "repo_cap");
    assert_eq!(row0["repo_cap"]["cap"], 1);
    assert_eq!(json["record"]["plan"]["slots"]["free"], 2);
    let back: TelemetryEnvelope = serde_json::from_value(json).unwrap();
    assert_eq!(back, envelope);
}

/// A pre-#9288 row (no plan fields) still decodes, with the plan defaulted.
#[test]
fn a_row_without_plan_fields_decodes_with_defaults() {
    let row: QueueSnapshotRow = serde_json::from_value(serde_json::json!({
        "rank": 1, "repo": "o/r", "issue": 1, "workspace_priority": 100,
        "urgent": false, "disposition": "dispatched", "state": "running",
        "reason": "dispatched this tick"
    }))
    .unwrap();
    assert_eq!(row.plan, crate::types::RowPlan::default());
}

#[test]
fn deferred_file_overlap_row_exports_its_shared_paths_in_the_serialized_snapshot() {
    let s = summary(vec![
        row(
            1,
            PUBLIC_ROOT,
            10,
            Qd::DeferredFileOverlap,
            Some("file overlap: x/b.rs, Dockerfile"),
        ),
        row(2, PUBLIC_ROOT, 11, Qd::DispatchError, Some("boom: secret stderr")),
    ]);
    let record = build_record(&s, &repos());
    let json = serde_json::to_value(&record).unwrap();
    let rows = json["rows"].as_array().unwrap();
    assert_eq!(rows[0]["detail"], "file overlap: x/b.rs, Dockerfile");
    assert!(rows[1].get("detail").is_none_or(serde_json::Value::is_null), "{}", rows[1]);
    assert!(
        !json.to_string().contains("secret stderr"),
        "dispatch-error text stays excluded"
    );
}
