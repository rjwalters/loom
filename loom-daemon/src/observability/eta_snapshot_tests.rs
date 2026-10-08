//! `eta.snapshot` selection, building and routing tests (Issue #9329).

use std::sync::Mutex as StdMutex;

use chrono::{Duration, TimeZone, Utc};

use super::{
    build_record, build_record_with, decide, fingerprint, fingerprint_with, is_changed,
    select_alternates, select_current, EtaSnapshotSink, RegisteredIds,
};
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

/// The registered ids of each kind, from the builtin registry.
fn registered() -> RegisteredIds {
    let registry = Registry::builtin();
    [Kind::Start, Kind::Finish, Kind::Land]
        .into_iter()
        .map(|k| (k, registry.for_kind(k).map(|h| h.id().to_string()).collect()))
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
        tail_extrapolated: false,
        stall_cause: None,
        stage_predictions: Default::default(),
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
        // Cut-priority order (#10928): `land` estimates, then the rest.
        vec![
            (PRIVATE_REPO, 42, Kind::Land, Some(7_200)),
            (REPO, 9329, Kind::Land, Some(1_800)),
            (REPO, 9329, Kind::Finish, Some(900)),
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
    assert!(decide(Some((Vec::new(), current(), registered())), None).is_none());
    // Nor when everything it holds is a shadow candidate.
    let shadows = vec![summary(REPO, 9329, Kind::Land, "land-v2", 0, Some(60))];
    assert!(decide(Some((shadows, current(), registered())), None).is_none());

    // The first pass of a process emits; a restart mid-cadence is exactly
    // this case, and sends the restored set once rather than a stale copy.
    let (selected, _, digest) =
        decide(Some((pending.clone(), current(), registered())), None).expect("emits");
    assert_eq!(selected.len(), 1);

    // The next pass, unchanged, sends nothing…
    assert!(decide(Some((pending.clone(), current(), registered())), Some(digest)).is_none());
    // …and a new estimate for the same item sends again.
    let refreshed = vec![summary(REPO, 9329, Kind::Land, "land-v1", 5, Some(1_800))];
    let (_, _, next) =
        decide(Some((refreshed, current(), registered())), Some(digest)).expect("a new estimate");
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
        stalls: &crate::eta::stall::StallSnapshot::default(),
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
    // Sent in cut-priority order, then (repo, issue, kind) (#10928): the land
    // estimates lead, so a reader keeping only a prefix keeps them.
    let rank = |r: &crate::telemetry::kinds::eta_snapshot::EtaSnapshotRow| match r.kind {
        Kind::Land if r.p50.is_some() => 0,
        Kind::Land => 1,
        _ => 2,
    };
    let keys: Vec<_> = record
        .rows
        .iter()
        .map(|r| (rank(r), r.repo.clone(), r.issue, r.kind))
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
    assert!(record.rows[..10].iter().all(|r| r.repo == "z/late"));
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
    // MAX_ROWS is sized on ~250-350 B/row (~0.5-0.7 MB at 2000 rows), under
    // MAX_RECORD_BYTES with no alternates at all; see its doc comment.
    assert!(bytes < 500, "row grew to {bytes} bytes");
}

// ---------------------------------------------------------------------------
// Alternates (#10390).
// ---------------------------------------------------------------------------

/// A registered candidate shadow (`land-2026-10-04-twin-otter` itself is
/// retired, #10528).
const TWIN: &str = "land-2026-10-04-twin-otter-b";

fn alts(pending: &[EstimateSummary]) -> super::Alternates {
    select_alternates(pending, &current(), &registered())
}

#[test]
fn newest_shadow_estimate_wins_and_current_is_excluded() {
    let pending = vec![
        summary(REPO, 1, Kind::Land, "land-v1", 0, Some(3_600)),
        summary(REPO, 1, Kind::Land, TWIN, 1, Some(100)),
        summary(REPO, 1, Kind::Land, TWIN, 7, Some(200)),
        summary(REPO, 1, Kind::Land, "land-v1", 9, Some(300)),
    ];
    let alternates = alts(&pending);
    let list = &alternates[&(REPO.to_string(), 1, Kind::Land)];
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].p50_sec, Some(200));
    assert_eq!(list[0].heuristic, TWIN);
}

#[test]
fn unregistered_shadows_are_excluded() {
    let pending = vec![summary(REPO, 1, Kind::Land, "land-retired", 0, Some(5))];
    assert!(alts(&pending).is_empty());
}

#[test]
fn a_shadow_with_a_different_as_of_still_attaches_but_never_creates_a_row() {
    let pending = vec![
        summary(REPO, 1, Kind::Land, "land-v1", 0, Some(3_600)),
        summary(REPO, 1, Kind::Land, TWIN, 30, None),
        summary(REPO, 2, Kind::Land, TWIN, 0, Some(10)),
    ];
    let selected = select_current(&pending, &current());
    assert_eq!(selected.len(), 1);
    let record = build_record_with(&selected, &alts(&pending), &visibility());
    assert_eq!(record.rows.len(), 1);
    let alt = &record.rows[0].alternates;
    assert_eq!(alt.len(), 1);
    assert_ne!(alt[0].as_of, record.rows[0].as_of);
    assert_eq!(alt[0].no_estimate_reason, Some(NoEstimateReason::InsufficientSamples));
    assert_eq!(alt[0].p50, None);
}

#[test]
fn alternates_are_sorted_by_id_and_truncated() {
    let ids: Vec<String> = (0..15).rev().map(|i| format!("land-x{i:02}")).collect();
    let mut registered = registered();
    registered.insert(Kind::Land, ids.clone());
    let mut current = current();
    current.insert(Kind::Land, "land-x00".to_string());
    let pending: Vec<EstimateSummary> = ids
        .iter()
        .map(|id| summary(REPO, 1, Kind::Land, id, 0, Some(1)))
        .collect();
    let alternates = select_alternates(&pending, &current, &registered);
    let list = &alternates[&(REPO.to_string(), 1, Kind::Land)];
    assert_eq!(list.len(), crate::telemetry::kinds::eta_snapshot::MAX_ALTERNATES);
    let got: Vec<&str> = list.iter().map(|e| e.heuristic.as_str()).collect();
    let mut sorted = got.clone();
    sorted.sort_unstable();
    assert_eq!(got, sorted);
    assert_eq!(got[0], "land-x01");
    assert_eq!(got[12], "land-x13", "the 14th shadow is cut");
}

/// #10521: the cap is 13 (12 since #10549, was 8). A kind with 13
/// non-current heuristics attaches every one of them, in id order, and the
/// row carries all 13.
#[test]
fn thirteen_non_current_heuristics_all_attach_as_alternates() {
    use crate::telemetry::kinds::eta_snapshot::MAX_ALTERNATES;
    assert_eq!(MAX_ALTERNATES, 13);
    let ids: Vec<String> = (0..14).map(|i| format!("land-x{i:02}")).collect();
    let mut registered = registered();
    registered.insert(Kind::Land, ids.clone());
    let mut current = current();
    current.insert(Kind::Land, "land-x00".to_string());
    let pending: Vec<EstimateSummary> = ids
        .iter()
        .map(|id| summary(REPO, 1, Kind::Land, id, 0, Some(1)))
        .collect();
    let selected = select_current(&pending, &current);
    let alternates = select_alternates(&pending, &current, &registered);
    let record = build_record_with(&selected, &alternates, &visibility());
    let got: Vec<&str> = record.rows[0]
        .alternates
        .iter()
        .map(|a| a.heuristic.as_str())
        .collect();
    assert_eq!(got, ids[1..].iter().map(String::as_str).collect::<Vec<_>>());
    assert_eq!(got.len(), 13, "every non-current heuristic, none dropped");
}

#[test]
fn a_shadow_only_change_changes_the_fingerprint_and_triggers_a_snapshot() {
    let base = vec![
        summary(REPO, 1, Kind::Land, "land-v1", 0, Some(3_600)),
        summary(REPO, 1, Kind::Land, TWIN, 0, Some(100)),
    ];
    let (_, _, digest) = decide(Some((base.clone(), current(), registered())), None).unwrap();
    assert!(decide(Some((base.clone(), current(), registered())), Some(digest)).is_none());
    let mut next = base;
    next.push(summary(REPO, 1, Kind::Land, TWIN, 5, Some(90)));
    assert!(decide(Some((next, current(), registered())), Some(digest)).is_some());
    // No alternates: identical to the plain fingerprint.
    let one = vec![summary(REPO, 1, Kind::Land, "land-v1", 0, Some(1))];
    assert_eq!(fingerprint(&one), fingerprint_with(&one, &super::Alternates::new()));
}

#[test]
fn rows_without_alternates_omit_the_key_and_budgets_hold() {
    use crate::telemetry::kinds::eta_snapshot::MAX_ALTERNATES;
    let pending = vec![summary(REPO, 1, Kind::Finish, "finish-v1", 0, Some(10))];
    let selected = select_current(&pending, &current());
    let record = build_record_with(&selected, &alts(&pending), &visibility());
    let wire = serde_json::to_value(&record.rows[0]).unwrap();
    assert!(wire.get("alternates").is_none());

    // 13 alternates (the cap, #10549, #10521) with long ids: one row < 3 KB,
    // 200 rows < 768 KB.
    let ids: Vec<String> = (0..MAX_ALTERNATES)
        .map(|i| format!("land-2026-10-04-long-heuristic-name-{i}"))
        .collect();
    let mut registered = registered();
    registered.insert(Kind::Land, [vec!["land-v1".to_string()], ids.clone()].concat());
    let mut pending = Vec::new();
    for issue in 0..200 {
        pending.push(summary(REPO, issue, Kind::Land, "land-v1", 0, Some(3_600)));
        for id in &ids {
            pending.push(summary(REPO, issue, Kind::Land, id, 0, Some(100)));
        }
    }
    let selected = select_current(&pending, &current());
    let alternates = select_alternates(&pending, &current(), &registered);
    let record = build_record_with(&selected, &alternates, &visibility());
    assert_eq!(record.rows.len(), 200);
    assert_eq!(record.rows[0].alternates.len(), MAX_ALTERNATES);
    let row = serde_json::to_vec(&record.rows[0]).unwrap().len();
    let all = serde_json::to_vec(&record).unwrap().len();
    eprintln!("{MAX_ALTERNATES}-alternate row: {row} B; 200-row record: {all} B");
    assert!(row < 3 * 1_024, "row grew to {row} bytes");
    assert!(all < 768 * 1_024, "record grew to {all} bytes");
    assert_eq!(record.alternates_truncated, 0, "200 full rows fit the budget");
}

#[test]
fn registering_a_fifteenth_land_heuristic_fails_loudly() {
    let land = Registry::builtin().for_kind(Kind::Land).count();
    assert!(land - 1 <= crate::telemetry::kinds::eta_snapshot::MAX_ALTERNATES);
}

/// #10521: `land-2026-10-06-loop-kite` is the fourteenth land heuristic, so
/// the daemon carries 13 alternates. A loom-ui still slicing at the old 12
/// drops only the 13th by id: with the shipped registry and the default
/// `current`, `little-v0`, a baseline the chooser never offers. So either
/// deploy order loses no candidate.
#[test]
fn every_builtin_land_shadow_attaches_and_an_old_twelve_slice_drops_only_a_baseline() {
    let registered = registered();
    let pending: Vec<EstimateSummary> = registered[&Kind::Land]
        .iter()
        .map(|id| summary(REPO, 1, Kind::Land, id, 0, Some(1)))
        .collect();
    let alternates = select_alternates(&pending, &current(), &registered);
    let list = &alternates[&(REPO.to_string(), 1, Kind::Land)];
    assert_eq!(list.len(), registered[&Kind::Land].len() - 1, "the daemon drops none");
    let past_old_cap: Vec<&str> = list[12..].iter().map(|e| e.heuristic.as_str()).collect();
    assert_eq!(past_old_cap, ["little-v0"]);
    assert_eq!(Registry::builtin().tier_of("little-v0"), Some(crate::eta::Tier::Baseline));
}

/// #10484: `land-v3` and `land-2026-10-04-amber-heron` were retired from the
/// live shadow set, #10549 `land-2026-10-04-fresh-tide`, #10528
/// `land-2026-10-04-twin-otter`, and #10489 `land-2026-10-06-calm-plover`. Pending
/// estimates restored from disk that still name them are never offered as
/// `alternates[]`; the live shadows are.
#[test]
fn retired_heuristics_never_appear_as_alternates_even_when_pending_names_them() {
    let retired = [
        "land-v3",
        "land-2026-10-04-amber-heron",
        "land-2026-10-04-fresh-tide",
        "land-2026-10-04-twin-otter",
        "land-2026-10-06-calm-plover",
    ];
    let registered = registered();
    for id in retired {
        assert!(!registered[&Kind::Land].iter().any(|r| r == id), "{id} is retired");
    }
    let pending = vec![
        summary(REPO, 1, Kind::Land, "land-v1", 0, Some(3_600)),
        summary(REPO, 1, Kind::Land, "land-v2", 0, Some(3_000)),
        summary(REPO, 1, Kind::Land, "land-v3", 0, Some(2_000)),
        summary(REPO, 1, Kind::Land, "land-2026-10-04-amber-heron", 0, Some(1_000)),
        summary(REPO, 1, Kind::Land, "land-2026-10-04-fresh-tide", 0, Some(500)),
        summary(REPO, 1, Kind::Land, "land-2026-10-04-twin-otter", 0, Some(400)),
        summary(REPO, 1, Kind::Land, "land-2026-10-06-calm-plover", 0, Some(400)),
    ];
    let alternates = select_alternates(&pending, &current(), &registered);
    let list = &alternates[&(REPO.to_string(), 1, Kind::Land)];
    let got: Vec<&str> = list.iter().map(|e| e.heuristic.as_str()).collect();
    assert_eq!(got, vec!["land-v2"]);
    for id in retired {
        assert!(!got.contains(&id));
    }
}

/// #10525: every alternate carries its heuristic's tier, so the ETA chooser
/// can offer only candidates. With `land-v4` current, the `land-v1` and
/// `little-v0` baselines ride along as alternates beside the candidates.
#[test]
fn alternates_carry_each_heuristics_tier() {
    use crate::eta::Tier;
    let mut current = current();
    current.insert(Kind::Land, "land-v4".to_string());
    let pending = vec![
        summary(REPO, 1, Kind::Land, "land-v4", 0, Some(3_600)),
        summary(REPO, 1, Kind::Land, "land-v1", 0, Some(3_000)),
        summary(REPO, 1, Kind::Land, "little-v0", 0, None),
        summary(REPO, 1, Kind::Land, TWIN, 0, Some(2_000)),
    ];
    let selected = select_current(&pending, &current);
    let alternates = select_alternates(&pending, &current, &registered());
    let record = build_record_with(&selected, &alternates, &visibility());
    let tiers: Vec<(&str, Option<Tier>)> = record.rows[0]
        .alternates
        .iter()
        .map(|a| (a.heuristic.as_str(), a.tier))
        .collect();
    assert_eq!(
        tiers,
        [
            (TWIN, Some(Tier::Candidate)),
            ("land-v1", Some(Tier::Baseline)),
            ("little-v0", Some(Tier::Baseline)),
        ]
    );
    let wire = serde_json::to_value(&record.rows[0]).unwrap();
    assert_eq!(wire["alternates"][0]["tier"], "candidate");
    assert_eq!(wire["alternates"][1]["tier"], "baseline");
}

// ---------------------------------------------------------------------------
// The fleet-authority budget (#10928): one host emits the whole fleet's rows.
// ---------------------------------------------------------------------------

/// A fleet-authority-shaped estimate set: `items` items over 58 repos with
/// production-length slugs and 16-hex estimate ids, `shape(i)` giving each
/// item's kind and `p50`. Every `land` item also has a pending estimate from
/// every registered land shadow (refusing when the row refuses), so each
/// `land` row carries the full alternate list, as live rows do.
fn fleet(
    items: u32,
    shape: impl Fn(u32) -> (Kind, Option<i64>),
) -> (Vec<EstimateSummary>, super::Alternates) {
    let land_ids = registered().remove(&Kind::Land).unwrap();
    let current = current();
    let mut pending = Vec::new();
    for i in 0..items {
        let repo = format!("2AMLogic/fleet-repo-{:02}", i % 58);
        let (kind, p50) = shape(i);
        let heuristics: Vec<&String> = match kind {
            Kind::Land => land_ids.iter().collect(),
            _ => vec![&current[&kind]],
        };
        for heuristic in heuristics {
            let mut estimate = summary(&repo, 1000 + i, kind, heuristic, 0, p50.map(|p| p + 60));
            estimate.estimate_id = format!("{:016x}", pending.len() * 7919 + 1);
            pending.push(estimate);
        }
    }
    let selected = select_current(&pending, &current);
    let alternates = select_alternates(&pending, &current, &registered());
    (selected, alternates)
}

fn wire_bytes(record: &crate::telemetry::kinds::eta_snapshot::EtaSnapshotRecord) -> usize {
    serde_json::to_vec(record).unwrap().len()
}

/// The rows that carry alternates are a prefix of the rows: the budget
/// gives them to the highest-priority rows first.
fn assert_alternates_are_a_prefix(
    record: &crate::telemetry::kinds::eta_snapshot::EtaSnapshotRecord,
) {
    let with = record
        .rows
        .iter()
        .filter(|r| !r.alternates.is_empty())
        .count();
    assert!(record.rows[..with].iter().all(|r| !r.alternates.is_empty()));
    assert!(record.rows[with..].iter().all(|r| r.alternates.is_empty()));
}

/// Today's fleet (682 rows live on 2026-10-08, 482 of them dropped at the
/// old 200-row cap), all `land`: every row is sent, and the record stays
/// within the 1 MiB budget by sending the lowest-priority rows' alternates
/// no further.
#[test]
fn a_700_row_fleet_is_sent_whole_within_the_byte_budget() {
    use crate::telemetry::kinds::eta_snapshot::MAX_RECORD_BYTES;
    let (selected, alternates) = fleet(700, |i| (Kind::Land, (i % 5 != 4).then_some(3_600)));
    assert_eq!(selected.len(), 700);
    let record = build_record_with(&selected, &alternates, &visibility());
    assert_eq!(record.rows.len(), 700);
    assert_eq!(record.rows_truncated, 0);
    assert!(record.rows_truncated_by_kind.is_empty());

    let bytes = wire_bytes(&record);
    assert!(bytes <= MAX_RECORD_BYTES, "{bytes} B over the {MAX_RECORD_BYTES} B budget");
    let envelope = crate::telemetry::TelemetryEnvelope::new(
        "loom-worker-1",
        crate::telemetry::TelemetryRecord::EtaSnapshot(record.clone()),
    );
    let enveloped = serde_json::to_vec(&envelope).unwrap().len();
    assert!(enveloped <= MAX_RECORD_BYTES + 1_024, "{enveloped} B enveloped");
    assert_eq!(super::stats(&record).bytes, bytes as u64);

    let with = record
        .rows
        .iter()
        .filter(|r| !r.alternates.is_empty())
        .count();
    assert_eq!(with + record.alternates_truncated, 700, "every land row has shadows");
    assert!(with >= 300, "only {with} rows kept their alternates");
    assert!(record.alternates_truncated > 0, "700 full rows exceed 1 MiB");
    assert_alternates_are_a_prefix(&record);
    // The estimating land rows lead; the refusals (every fifth) follow.
    assert!(record.rows[..560].iter().all(|r| r.p50.is_some()));
    assert!(record.rows[560..].iter().all(|r| r.p50.is_none()));
    let stats = super::stats(&record);
    eprintln!(
        "700-row fleet: {bytes} B ({enveloped} B enveloped), {} rows with alternates, {} without",
        stats.alternates_rows, stats.alternates_truncated
    );
}

/// The headroom: a fleet of the full 2000-row cap is still sent whole.
#[test]
fn a_2000_row_fleet_is_sent_whole() {
    use crate::telemetry::kinds::eta_snapshot::{MAX_RECORD_BYTES, MAX_ROWS};
    let (selected, alternates) = fleet(u32::try_from(MAX_ROWS).unwrap(), |i| match i % 4 {
        0 | 1 => (Kind::Land, Some(3_600)),
        2 => (Kind::Land, None),
        _ => (Kind::Start, Some(600)),
    });
    let record = build_record_with(&selected, &alternates, &visibility());
    assert_eq!(record.rows.len(), MAX_ROWS);
    assert_eq!(record.rows_truncated, 0);
    let bytes = wire_bytes(&record);
    assert!(bytes <= MAX_RECORD_BYTES, "{bytes} B");
    assert_alternates_are_a_prefix(&record);
    eprintln!(
        "2000-row fleet: {bytes} B, {} rows with alternates",
        super::stats(&record).alternates_rows
    );
}

/// Past the row cap the lowest-priority rows go, and the rest keep the
/// priority order.
#[test]
fn past_the_cap_the_lowest_priority_rows_go_and_the_order_holds() {
    use crate::telemetry::kinds::eta_snapshot::{MAX_RECORD_BYTES, MAX_ROWS};
    let items = u32::try_from(MAX_ROWS).unwrap() + 99;
    let (selected, alternates) = fleet(items, |i| match i % 3 {
        0 => (Kind::Start, Some(600)),
        1 => (Kind::Land, None),
        _ => (Kind::Land, Some(3_600)),
    });
    let record = build_record_with(&selected, &alternates, &visibility());
    assert_eq!(record.rows.len(), MAX_ROWS);
    assert_eq!(record.rows_truncated, 99);
    assert_eq!(
        record.rows_truncated_by_kind,
        std::collections::BTreeMap::from([(Kind::Start, 99)])
    );
    assert!(wire_bytes(&record) <= MAX_RECORD_BYTES);
    let rank = |r: &crate::telemetry::kinds::eta_snapshot::EtaSnapshotRow| match r.kind {
        Kind::Land if r.p50.is_some() => 0,
        Kind::Land => 1,
        _ => 2,
    };
    let keys: Vec<_> = record
        .rows
        .iter()
        .map(|r| (rank(r), r.repo.to_ascii_lowercase(), r.issue, r.kind))
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
    assert_alternates_are_a_prefix(&record);
}

/// Rows alone are held to the byte budget too: with slugs long enough that
/// 2000 bare rows exceed 1 MiB, rows are cut by bytes and counted.
#[test]
fn rows_too_large_for_the_budget_are_cut_by_bytes() {
    use crate::telemetry::kinds::eta_snapshot::{MAX_RECORD_BYTES, MAX_ROWS};
    let long = "r".repeat(600);
    let pending: Vec<EstimateSummary> = (0..u32::try_from(MAX_ROWS).unwrap())
        .map(|i| summary(&format!("o/{long}"), i + 1, Kind::Land, "land-v1", 0, Some(600)))
        .collect();
    let selected = select_current(&pending, &current());
    let record = build_record(&selected, &visibility());
    let row = serde_json::to_vec(&record.rows[0]).unwrap().len();
    assert!(record.rows.len() < MAX_ROWS);
    assert_eq!(record.rows_truncated, MAX_ROWS - record.rows.len());
    assert_eq!(record.rows_truncated_by_kind.get(&Kind::Land), Some(&record.rows_truncated));
    let bytes = wire_bytes(&record);
    assert!(bytes <= MAX_RECORD_BYTES, "{bytes} B");
    assert!(bytes > MAX_RECORD_BYTES - 2 * row - 1_024, "cut early: {bytes} B");
    // The survivors are the first by issue number.
    assert_eq!(record.rows.last().unwrap().issue, u32::try_from(record.rows.len()).unwrap());
}

/// #10929: a row's own stage forecast never costs a row. It rides in row
/// order while it fits, before any alternate, and the record stays within
/// the budget.
#[test]
fn stage_forecasts_ride_before_alternates_and_never_cost_a_row() {
    use crate::eta::stage_forecast::StagePrediction;
    use crate::telemetry::kinds::eta_snapshot::MAX_RECORD_BYTES;
    let (mut selected, alternates) = fleet(700, |i| (Kind::Land, (i % 5 != 4).then_some(3_600)));
    let forecast = |entry: i64| StagePrediction {
        entry_p50: entry,
        entry_p90: entry * 3,
        dwell_p50: 2_400,
        dwell_p90: 10_800,
        alloc: 2_000,
        reach_pct: 100,
    };
    for estimate in selected.iter_mut().filter(|e| e.p50_sec.is_some()) {
        estimate.stage_predictions = [
            (Stage::ReviewWait, forecast(0)),
            (Stage::Doctor, forecast(2_400)),
            (Stage::MergeWait, forecast(4_800)),
        ]
        .into();
    }
    let record = build_record_with(&selected, &alternates, &visibility());
    assert_eq!(record.rows.len(), 700, "a forecast never costs a row");
    assert!(wire_bytes(&record) <= MAX_RECORD_BYTES);
    let staged = record.rows.iter().filter(|r| !r.stages.is_empty()).count();
    assert_eq!(staged, 560, "every estimating row keeps its forecast");
    assert!(record.rows[..560].iter().all(|r| r.stages.len() == 3));
    assert!(
        record.rows[560..].iter().all(|r| r.stages.is_empty()),
        "refusals forecast nothing"
    );
    let row = &record.rows[0].stages[&Stage::Doctor];
    assert_eq!((row.entry_p50, row.entry_p90, row.reach_pct), (2_400, 7_200, 100));
    assert_alternates_are_a_prefix(&record);
}
