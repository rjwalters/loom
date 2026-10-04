//! `eta.snapshot` selection, building and routing tests (Issue #9329).

use std::sync::Mutex as StdMutex;

use chrono::{Duration, TimeZone, Utc};

use super::{build_record, decide, fingerprint, is_changed, select_current, EtaSnapshotSink};
use crate::eta::score::EstimateSummary;
use crate::eta::tracker::{EstimateContext, Tracker};
use crate::eta::{Kind, NoEstimateReason, Provenance, Registry, Stage};
use crate::observability::queue::QueueSink;
use crate::telemetry::{RepoVisibility, TelemetryEnvelope};

const REPO: &str = "rjwalters/loom";
const PRIVATE_REPO: &str = "acme/secret";

fn provenance() -> Provenance {
    Provenance {
        version: "0.19.585".to_string(),
        revision: "9d8e226ce0123456789abcdef0123456789abcde".to_string(),
        tree_state: "clean".to_string(),
        complete: true,
    }
}

/// The `current` heuristic of each kind, as the registry's defaults name
/// them — what [`super::eta::snapshot_input`] hands the selector.
fn current() -> std::collections::BTreeMap<Kind, String> {
    [
        (Kind::Start, "start-v1".to_string()),
        (Kind::Finish, "finish-v1".to_string()),
        (Kind::Land, "land-v1".to_string()),
    ]
    .into_iter()
    .collect()
}

fn summary(
    repo: &str,
    issue: u32,
    kind: Kind,
    heuristic: &str,
    age_minutes: i64,
    p50: Option<i64>,
) -> EstimateSummary {
    let as_of =
        Utc.with_ymd_and_hms(2026, 9, 30, 12, 0, 0).unwrap() + Duration::minutes(age_minutes);
    EstimateSummary {
        estimate_id: format!("{repo}-{issue}-{kind:?}-{heuristic}-{age_minutes}"),
        kind,
        heuristic: heuristic.to_string(),
        loom: provenance(),
        repo: repo.to_string(),
        repo_id: Some(1),
        issue,
        pr_number: Some(issue + 1),
        as_of,
        stage: Some(Stage::ReviewWait),
        age_sec: Some(120),
        p25_sec: p50.map(|p| p - 60),
        p50_sec: p50,
        p75_sec: p50.map(|p| p + 60),
        p90_sec: p50.map(|p| p + 120),
        samples_min: Some(12),
        no_estimate_reason: p50
            .is_none()
            .then_some(NoEstimateReason::InsufficientSamples),
        stage_quartiles: Vec::new(),
    }
}

fn visibility() -> std::collections::HashMap<String, RepoVisibility> {
    [
        (REPO.to_string(), RepoVisibility::Public),
        (PRIVATE_REPO.to_string(), RepoVisibility::Private),
    ]
    .into_iter()
    .collect()
}

// ---------------------------------------------------------------------------
// Selection: what counts as "the current estimate".
// ---------------------------------------------------------------------------

/// `pending` is a *series* per item — every refresh still awaiting an outcome,
/// plus one per shadow candidate (#9328). The snapshot is the live view, so
/// exactly one row survives per `(repo, issue, kind)`: the newest estimate of
/// the kind's `current` heuristic.
#[test]
fn only_the_current_heuristics_newest_estimate_per_item_is_a_row() {
    let pending = vec![
        summary(REPO, 9329, Kind::Land, "land-v1", 0, Some(3_600)),
        // A later refresh of the same series: this is the current one.
        summary(REPO, 9329, Kind::Land, "land-v1", 10, Some(1_800)),
        // A shadow candidate at the same instant: never the subject's answer.
        summary(REPO, 9329, Kind::Land, "land-v2", 10, Some(60)),
        // A different kind of the same item is its own row.
        summary(REPO, 9329, Kind::Finish, "finish-v1", 10, Some(900)),
        // A different item.
        summary(PRIVATE_REPO, 42, Kind::Land, "land-v1", 5, Some(7_200)),
    ];
    let selected = select_current(&pending, &current());
    assert_eq!(selected.len(), 3, "{selected:#?}");
    let record = build_record(&selected, &visibility());
    let rows: Vec<(&str, u32, Kind, Option<i64>)> = record
        .rows
        .iter()
        .map(|r| (r.repo.as_str(), r.issue, r.kind, r.p50))
        .collect();
    assert_eq!(
        rows,
        vec![
            (PRIVATE_REPO, 42, Kind::Land, Some(7_200)),
            (REPO, 9329, Kind::Finish, Some(900)),
            (REPO, 9329, Kind::Land, Some(1_800)),
        ]
    );
    assert!(
        record.rows.iter().all(|r| r.heuristic != "land-v2"),
        "a shadow candidate's estimate must never be shown as the ETA"
    );
    // The record's stamp is the newest estimate in the set.
    assert_eq!(record.as_of, selected.iter().map(|e| e.as_of).max().unwrap());
    assert_eq!(record.rows_truncated, 0);
}

/// Edge case from the issue's test plan: an issue with a
/// `no_estimate_reason` stays in the snapshot. That an item *cannot* be
/// estimated, and why, is itself informative — dropping it would render as
/// "no such issue" rather than "no estimate".
#[test]
fn a_refusal_is_a_row_carrying_its_reason_and_no_quantiles() {
    let selected =
        select_current(&[summary(REPO, 9329, Kind::Land, "land-v1", 0, None)], &current());
    let record = build_record(&selected, &visibility());
    let row = &record.rows[0];
    assert_eq!(row.no_estimate_reason, Some(NoEstimateReason::InsufficientSamples));
    assert_eq!((row.p25, row.p50, row.p75), (None, None, None));
    // Absent, never a fabricated zero, on the wire too.
    let wire = serde_json::to_value(row).unwrap();
    assert!(wire.get("p50").is_none(), "{wire}");
    assert_eq!(wire["no_estimate_reason"], "insufficient_samples");
}

/// Edge case from the issue's test plan: no current estimates ⇒ no rows, and
/// (in [`super::record`]) no record at all rather than an empty one.
#[test]
fn zero_current_estimates_select_nothing() {
    assert!(select_current(&[], &current()).is_empty());
    // A tracker holding only shadow estimates is the same case.
    let only_shadows = vec![summary(REPO, 9329, Kind::Land, "land-v2", 0, Some(60))];
    assert!(select_current(&only_shadows, &current()).is_empty());
}

#[test]
fn rows_past_the_cap_are_counted_not_sent() {
    let pending: Vec<EstimateSummary> = (0..u32::try_from(super::MAX_ROWS).unwrap() + 5)
        .map(|i| summary(REPO, i, Kind::Land, "land-v1", 0, Some(600)))
        .collect();
    let selected = select_current(&pending, &current());
    let record = build_record(&selected, &visibility());
    assert_eq!(record.rows.len(), super::MAX_ROWS);
    assert_eq!(record.rows_truncated, 5);
}

// ---------------------------------------------------------------------------
// The anti-leak rules (the same three `queue.snapshot` holds).
// ---------------------------------------------------------------------------

#[test]
fn rows_carry_the_slug_and_their_visibility_never_a_local_path() {
    let selected = select_current(
        &[
            summary(REPO, 1, Kind::Land, "land-v1", 0, Some(600)),
            summary(PRIVATE_REPO, 2, Kind::Land, "land-v1", 0, Some(600)),
        ],
        &current(),
    );
    let record = build_record(&selected, &visibility());
    assert_eq!(record.rows[0].repo, PRIVATE_REPO);
    assert_eq!(record.rows[0].visibility, RepoVisibility::Private);
    assert_eq!(record.rows[1].visibility, RepoVisibility::Public);
    let json = serde_json::to_string(&record).unwrap();
    assert!(!json.contains('/') || !json.contains("/Users/"), "{json}");
    assert!(!json.contains("/home/") && !json.contains("/srv/"), "{json}");
}

/// An unresolvable repo is tagged `private`, and a row that reaches a reader
/// without the tag at all decodes the same way.
#[test]
fn an_untagged_repo_is_private() {
    let selected = select_current(
        &[summary(
            "unknown/repo",
            1,
            Kind::Land,
            "land-v1",
            0,
            Some(600),
        )],
        &current(),
    );
    let record = build_record(&selected, &std::collections::HashMap::new());
    assert_eq!(record.rows[0].visibility, RepoVisibility::Private);
    let mut wire = serde_json::to_value(&record.rows[0]).unwrap();
    wire.as_object_mut().unwrap().remove("visibility");
    let decoded: crate::telemetry::kinds::eta_snapshot::EtaSnapshotRow =
        serde_json::from_value(wire).unwrap();
    assert_eq!(decoded.visibility, RepoVisibility::Private);
}

// ---------------------------------------------------------------------------
// Change detection.
// ---------------------------------------------------------------------------

#[test]
fn only_a_changed_estimate_set_is_emitted() {
    let first =
        select_current(&[summary(REPO, 9329, Kind::Land, "land-v1", 0, Some(3_600))], &current());
    let refreshed =
        select_current(&[summary(REPO, 9329, Kind::Land, "land-v1", 10, Some(1_800))], &current());
    let (a, b) = (fingerprint(&first), fingerprint(&refreshed));
    // A daemon restart has no last fingerprint: the restored set is emitted
    // once, and is the live set, not a stale copy.
    assert!(is_changed(a, None), "the first pass of a process always emits");
    // The same pass's set re-read on the next cadence is not re-sent.
    assert!(!is_changed(a, Some(a)));
    assert_eq!(
        a,
        fingerprint(&select_current(
            &[summary(REPO, 9329, Kind::Land, "land-v1", 0, Some(3_600))],
            &current(),
        ))
    );
    // A new estimate for the same item does change it.
    assert_ne!(a, b);
    assert!(is_changed(b, Some(a)));
    // …and so does an item leaving the set (its outcome resolved).
    assert!(is_changed(fingerprint(&[]), Some(a)));
}

/// Every "no record at all" case, decided without touching the sink — the
/// edge cases the issue's test plan names, in one place.
#[test]
fn nothing_is_emitted_when_there_is_nothing_new_to_say() {
    let pending = vec![summary(REPO, 9329, Kind::Land, "land-v1", 0, Some(3_600))];

    // `autonomous.eta.enabled = false`: no tracker, so no `eta.snapshot`
    // ever — including on the very first pass of a process.
    assert!(decide(None, None).is_none());
    assert!(decide(None, Some(7)).is_none());

    // A tracker with no current estimates: no empty/garbage row.
    assert!(decide(Some((Vec::new(), current())), None).is_none());
    // Nor when everything it holds is a shadow candidate.
    let shadows = vec![summary(REPO, 9329, Kind::Land, "land-v2", 0, Some(60))];
    assert!(decide(Some((shadows, current())), None).is_none());

    // The first pass of a process emits; a restart mid-cadence is exactly
    // this case, and sends the restored set once rather than a stale copy.
    let (selected, digest) = decide(Some((pending.clone(), current())), None).expect("emits");
    assert_eq!(selected.len(), 1);

    // The next pass, unchanged, sends nothing…
    assert!(decide(Some((pending.clone(), current())), Some(digest)).is_none());
    // …and a new estimate for the same item sends again.
    let refreshed = vec![summary(REPO, 9329, Kind::Land, "land-v1", 5, Some(1_800))];
    let (_, next) = decide(Some((refreshed, current())), Some(digest)).expect("a new estimate");
    assert_ne!(next, digest);
}

// ---------------------------------------------------------------------------
// Routing: native-HTTPS only, and nothing at all without a native exporter.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Capture(StdMutex<Vec<TelemetryEnvelope>>);

impl QueueSink for Capture {
    fn offer(&self, envelope: TelemetryEnvelope) {
        self.0.lock().unwrap().push(envelope);
    }
    fn offer_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()> {
        self.offer(envelope);
        Ok(())
    }
}

#[test]
fn the_sink_offers_the_record_to_the_native_queue_only() {
    let capture = std::sync::Arc::new(Capture::default());
    let sink = EtaSnapshotSink::new(capture.clone(), "host-a");
    let selected =
        select_current(&[summary(REPO, 9329, Kind::Land, "land-v1", 0, Some(3_600))], &current());
    sink.push(build_record(&selected, &visibility()));

    let offered = capture.0.lock().unwrap().clone();
    assert_eq!(offered.len(), 1);
    let envelope = offered[0].clone();
    assert_eq!(envelope.host_id, "host-a");
    // The registry's routing decision, which is what both exporters read:
    // native ingest accepts it, and OTLP exports it as nothing at all.
    assert_eq!(envelope.record.kind(), "eta.snapshot");
    assert!(envelope.record.accepted_by_native_ingest());
    assert_eq!(envelope.record.otlp_class(), crate::telemetry::TelemetryKindOtlp::NotExported);
    assert!(envelope.record.otlp_class().signal().is_none(), "no OTLP signal carries it");
    assert_eq!(envelope.schema_version, crate::telemetry::NEW_KIND_SCHEMA_VERSION);
    assert_eq!(envelope.schema_version, 12);
    // The native export path keeps it…
    assert_eq!(
        crate::observability::tracing::native_envelopes(std::slice::from_ref(&envelope)).len(),
        1
    );
    // …and it round-trips through the envelope unchanged.
    let json = serde_json::to_value(&envelope).unwrap();
    assert_eq!(json["record"]["kind"], "eta.snapshot");
    assert_eq!(json["record"]["rows"][0]["kind"], "land");
    assert_eq!(json["record"]["rows"][0]["visibility"], "public");
    assert_eq!(json["record"]["rows"][0]["stage"], "review_wait");
    assert_eq!(json["record"]["rows"][0]["p50"], 3_600);
    assert_eq!(json["record"]["rows"][0]["pr"], 9_330);
    let back: TelemetryEnvelope = serde_json::from_value(json).unwrap();
    assert_eq!(back, envelope);
}

/// No HTTPS exporter ⇒ no sink ⇒ [`super::record`] returns before doing any
/// work, exactly as `queue.snapshot` does.
#[test]
fn no_native_exporter_means_no_sink() {
    assert!(super::sink_for_native_queues(Vec::new(), "host-a").is_none());
}

/// The rows are what the live tracker actually holds — built here from a real
/// `Tracker` over the fixture history rather than from hand-written
/// summaries, so a change to what an estimate carries reaches this test.
#[test]
fn rows_are_built_from_the_trackers_own_pending_estimates() {
    let at = crate::eta::tests::as_of();
    let mut tracker = Tracker::new(provenance());
    let registry = Registry::builtin();
    let history = crate::eta::tests::history_a();
    let repo_ids = [(REPO.to_string(), 1_073_994_527_u64)]
        .into_iter()
        .collect();
    let ctx = EstimateContext {
        registry: &registry,
        current_start: None,
        current_finish: None,
        current_land: None,
        history: &history,
        refresh_secs: 300,
        host_id: Some("host-test"),
        repo_ids: &repo_ids,
    };
    tracker.on_dispatch(REPO, 9289, "sweep-issue-9289-1", at);
    let emissions = tracker.estimate(None, &ctx, at);
    assert!(!emissions.is_empty(), "the fixture history produces estimates");

    let selected = select_current(tracker.pending(), &current());
    assert!(!selected.is_empty(), "the tracker's pending set is the snapshot's input");
    let record = build_record(&selected, &visibility());
    for row in &record.rows {
        assert_eq!(row.repo, REPO);
        assert_eq!(row.issue, 9289);
        assert!(!row.estimate_id.is_empty(), "the 'why this ETA?' join key");
        assert!(
            current().get(&row.kind) == Some(&row.heuristic),
            "{} is not the current heuristic of {:?}",
            row.heuristic,
            row.kind
        );
        assert!(
            row.p50.is_some() || row.no_estimate_reason.is_some(),
            "a row is either an estimate or a stated refusal, never neither"
        );
    }
    // One row per estimated kind, never one per emission.
    let mut kinds: Vec<Kind> = record.rows.iter().map(|r| r.kind).collect();
    kinds.dedup();
    assert_eq!(kinds.len(), record.rows.len(), "one row per (item, kind)");
}

#[test]
fn the_cap_keeps_land_rows_of_every_repo_and_drops_start_finish_first() {
    let cap = u32::try_from(super::MAX_ROWS).unwrap();
    let mut pending = Vec::new();
    // An early-sorting repo floods the cap with start refusals and finish rows.
    for i in 0..cap {
        pending.push(summary("a/early", i, Kind::Start, "start-v1", 0, None));
        pending.push(summary("a/early", i, Kind::Finish, "finish-v1", 0, Some(60)));
    }
    pending.push(summary("a/early", 0, Kind::Land, "land-v1", 0, None));
    // A last-sorting repo with land estimates.
    for i in 0..10 {
        pending.push(summary("z/late", i, Kind::Land, "land-v1", 0, Some(600)));
    }
    let selected = select_current(&pending, &current());
    let record = build_record(&selected, &visibility());
    assert_eq!(record.rows.len(), super::MAX_ROWS);
    assert_eq!(
        record
            .rows
            .iter()
            .filter(|r| r.repo == "z/late" && r.kind == Kind::Land && r.p50.is_some())
            .count(),
        10
    );
    // The land refusal outranks every start/finish row.
    assert!(record
        .rows
        .iter()
        .any(|r| r.repo == "a/early" && r.kind == Kind::Land && r.p50.is_none()));
    // Re-sorted by (repo, issue, kind) after the cut.
    let keys: Vec<_> = record
        .rows
        .iter()
        .map(|r| (r.repo.clone(), r.issue, r.kind))
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
    // Dropped counts: start/finish only, no land.
    assert_eq!(record.rows_truncated, selected.len() - super::MAX_ROWS);
    assert_eq!(record.rows_truncated_by_kind.get(&Kind::Land), None);
    assert_eq!(record.rows_truncated_by_kind.values().sum::<usize>(), record.rows_truncated);
    assert!(record.rows_truncated_by_kind.contains_key(&Kind::Start));
}

#[test]
fn no_truncation_means_no_per_kind_counts() {
    let pending = vec![summary(REPO, 1, Kind::Land, "land-v1", 0, Some(60))];
    let selected = select_current(&pending, &current());
    let record = build_record(&selected, &visibility());
    assert!(record.rows_truncated_by_kind.is_empty());
}

#[test]
fn a_serialized_row_stays_within_the_measured_budget() {
    let pending = vec![summary(
        "rjwalters/loom",
        10052,
        Kind::Land,
        "land-v1",
        0,
        Some(600),
    )];
    let selected = select_current(&pending, &current());
    let record = build_record(&selected, &visibility());
    let bytes = serde_json::to_vec(&record.rows[0]).unwrap().len();
    // MAX_ROWS is sized on ~250-350 B/row (~50-70 KB/record); see its doc comment.
    assert!(bytes < 500, "row grew to {bytes} bytes");
}
