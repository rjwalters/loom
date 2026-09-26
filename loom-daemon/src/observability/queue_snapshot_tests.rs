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
        created_at: Some("2026-09-01T00:00:00Z".into()),
        tier: Some("tier:goal-advancing".into()),
        disposition: d,
        detail: detail.map(str::to_string),
        state: d.state().into(),
        reason: d.reason().into(),
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
