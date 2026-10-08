//! `eta.stage_outcome` (#10929): registration, exit classification, the
//! journal mapping and the open-estimate links.

use super::*;
use crate::eta::score::EstimateSummary;
use crate::telemetry::{
    TelemetryEnvelope, TelemetryKindOtlp, TelemetryRecord, NEW_KIND_SCHEMA_VERSION, TELEMETRY_KINDS,
};
use chrono::{Duration, TimeZone};

const REPO: &str = "rjwalters/loom";

fn provenance() -> Provenance {
    Provenance {
        version: "0.19.800".to_string(),
        revision: "9d8e226ce0123456789abcdef0123456789abcde".to_string(),
        tree_state: "clean".to_string(),
        complete: true,
    }
}

fn t(sec: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap() + Duration::seconds(sec)
}

fn row(event: &str, stage: Option<Stage>, next: Option<Stage>, left: i64) -> JournalEntry {
    let mut row = JournalEntry::new(event, REPO, t(left), &provenance());
    row.issue = Some(7);
    row.pr_number = Some(8);
    row.stage = stage;
    row.next_stage = next;
    row.left_at = Some(t(left));
    row
}

fn estimate(kind: Kind, heuristic: &str, issue: u32, at: i64) -> EstimateSummary {
    EstimateSummary {
        estimate_id: format!("{kind:?}-{heuristic}-{issue}-{at}"),
        kind,
        heuristic: heuristic.to_string(),
        loom: provenance(),
        repo: REPO.to_string(),
        repo_id: Some(99),
        issue,
        pr_number: None,
        as_of: t(at),
        stage: Some(Stage::ReviewWait),
        age_sec: None,
        p25_sec: None,
        p50_sec: None,
        p75_sec: None,
        p90_sec: None,
        samples_min: None,
        no_estimate_reason: None,
        stage_quartiles: Vec::new(),
        tail_extrapolated: false,
        stall_cause: None,
        stage_predictions: Default::default(),
    }
}

#[test]
fn eta_stage_outcome_is_registered_as_an_otlp_log_kind() {
    let meta = TELEMETRY_KINDS
        .iter()
        .find(|m| m.kind == "eta.stage_outcome")
        .expect("eta.stage_outcome has a registry row");
    assert_eq!(meta.variant, "EtaStageOutcome");
    assert_eq!(meta.otlp, TelemetryKindOtlp::Logs);
    assert!(!meta.native_ingest, "OTLP-only, like the other eta.* log kinds");
    assert_eq!(meta.schema_version, NEW_KIND_SCHEMA_VERSION);
}

#[test]
fn every_boundary_kind_gets_its_exit() {
    let mut verdict_pass =
        row("label.transition", Some(Stage::ReviewWait), Some(Stage::MergeWait), 0);
    verdict_pass.verdict = Some("pass".to_string());
    let mut verdict_fail = row("label.transition", Some(Stage::ReviewWait), Some(Stage::Doctor), 0);
    verdict_fail.verdict = Some("fail".to_string());
    let mut judged = row("sweep.phase", Some(Stage::ReviewWait), None, 0);
    judged.raw = serde_json::json!({"phase": "judge"});
    let mut merge_phase = row("sweep.phase", Some(Stage::MergeWait), None, 0);
    merge_phase.raw = serde_json::json!({"phase": "merge"});
    let mut merged = row("pr.resolved", Some(Stage::MergeWait), None, 0);
    merged.raw = serde_json::json!({"pr": 8, "state": "merged"});
    let mut closed = row("pr.resolved", Some(Stage::ReviewWait), None, 0);
    closed.raw = serde_json::json!({"pr": 8, "state": "closed"});
    let mut censored = row("pr.resolved", Some(Stage::MergeHold), None, 0);
    censored.censored_sec = Some(60);
    let cases = [
        (verdict_pass, StageExit::Pass),
        (verdict_fail, StageExit::Rework),
        (
            row("label.transition", Some(Stage::ReviewWait), Some(Stage::Doctor), 0),
            StageExit::Rework,
        ),
        (
            row("label.transition", Some(Stage::MergeWait), Some(Stage::MergeHold), 0),
            StageExit::Hold,
        ),
        (
            row("label.transition", Some(Stage::MergeHold), Some(Stage::MergeWait), 0),
            StageExit::Released,
        ),
        (
            row("sweep.phase", Some(Stage::SweepCurator), Some(Stage::SweepBuilder), 0),
            StageExit::Advance,
        ),
        (judged, StageExit::Judged),
        (merge_phase, StageExit::Landed),
        (merged, StageExit::Landed),
        (closed, StageExit::CutShort),
        (censored, StageExit::CutShort),
        (row("label.transition", Some(Stage::Doctor), None, 0), StageExit::Unknown),
    ];
    for (row, exit) in cases {
        assert_eq!(StageExit::of(&row), exit, "{} {:?}", row.event, row.stage);
        let wire = serde_json::to_value(exit).unwrap();
        assert_eq!(wire, exit.as_str(), "wire name");
    }
}

#[test]
fn only_stage_closing_issue_rows_become_records() {
    let mut dispatch = row("sweep.dispatch", None, Some(Stage::SweepCurator), 0);
    dispatch.left_at = Some(t(0));
    let mut turnover = row("slot.turnover", Some(Stage::ReadyWait), None, 0);
    turnover.issue = None;
    let mut verdict = row("verdict", None, Some(Stage::Doctor), 0);
    verdict.left_at = None;
    let mut done = row("sweep.phase", Some(Stage::SweepBuilder), Some(Stage::ReviewWait), 900);
    done.entered_at = Some(t(100));
    done.duration_sec = Some(800);
    done.resolution_sec = Some(0);
    let records = from_journal(
        &[dispatch, turnover, verdict, done],
        std::iter::empty(),
        t(1000),
        &provenance(),
    );
    assert_eq!(records.len(), 1);
    let r = &records[0];
    assert_eq!(
        (r.stage, r.exit, r.next_stage),
        (Stage::SweepBuilder, StageExit::Advance, Some(Stage::ReviewWait))
    );
    assert_eq!((r.entered_at, r.left_at, r.dwell_sec), (Some(t(100)), t(900), Some(800)));
    assert_eq!((r.issue, r.pr_number, r.observed_at), (7, Some(8), t(1000)));
    assert_eq!((r.open_estimates, r.repo_id), (0, None));
    assert!(r.estimate_ids.is_empty());
    assert!(r.has_provenance());
}

#[test]
fn links_name_the_newest_open_estimate_per_series_made_before_the_exit() {
    let open = [
        estimate(Kind::Land, "land-v1", 7, 0),
        estimate(Kind::Land, "land-v1", 7, 300),
        estimate(Kind::Land, "land-v2", 7, 100),
        estimate(Kind::Finish, "finish-v1", 7, 200),
        // At or after the exit: made knowing it, never linked.
        estimate(Kind::Land, "land-v1", 7, 600),
        estimate(Kind::Land, "land-v3", 7, 601),
        // Another item.
        estimate(Kind::Land, "land-v1", 70, 10),
    ];
    let records = from_journal(
        &[row(
            "label.transition",
            Some(Stage::ReviewWait),
            Some(Stage::MergeWait),
            600,
        )],
        open.iter(),
        t(600),
        &provenance(),
    );
    let r = &records[0];
    assert_eq!(r.open_estimates, 4);
    assert_eq!(r.repo_id, Some(99));
    assert_eq!(
        r.estimate_ids,
        vec![
            "Finish-finish-v1-7-200".to_string(),
            "Land-land-v1-7-300".to_string(),
            "Land-land-v2-7-100".to_string(),
        ],
        "one per (kind, heuristic), the newest, in series order"
    );
}

#[test]
fn the_record_round_trips_through_the_envelope_and_omits_what_is_absent() {
    let records = from_journal(
        &[row(
            "label.transition",
            Some(Stage::MergeWait),
            Some(Stage::MergeHold),
            60,
        )],
        std::iter::empty(),
        t(60),
        &provenance(),
    );
    let record = records.into_iter().next().unwrap();
    let wire = serde_json::to_value(&record).unwrap();
    for absent in ["entered_at", "dwell_sec", "repo_id", "estimate_ids"] {
        assert!(wire.get(absent).is_none(), "{absent} absent, never null or 0: {wire}");
    }
    assert_eq!(wire["exit"], "hold");
    assert_eq!(wire["stage"], "merge_wait");
    assert_eq!(wire["next_stage"], "merge_hold");
    let envelope = TelemetryEnvelope::new("host", TelemetryRecord::EtaStageOutcome(record));
    let json = serde_json::to_string(&envelope).unwrap();
    let back: TelemetryEnvelope = serde_json::from_str(&json).unwrap();
    assert_eq!(back, envelope);
}
