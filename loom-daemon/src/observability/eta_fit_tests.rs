//! `eta.fit` emission (#10391): the record for every fit-check outcome, and
//! both callers' one-record-per-check rule against a capture sink.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::eta::fit::run::FitSkip;
use crate::eta::fleet::{self, FleetSnapshot};
use crate::observability::eta_fleet_refresh::after_cycle;
use chrono::{Duration as Span, TimeZone};
use std::sync::Mutex;

#[derive(Default)]
struct Capture(Mutex<Vec<TelemetryEnvelope>>);

impl QueueSink for Capture {
    fn offer(&self, envelope: TelemetryEnvelope) {
        self.0.lock().unwrap().push(envelope);
    }

    fn offer_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()> {
        self.offer(envelope);
        Ok(())
    }
}

impl Capture {
    fn records(&self) -> Vec<EtaFitRecord> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|e| match &e.record {
                TelemetryRecord::EtaFit(r) => r.clone(),
                other => panic!("unexpected record {other:?}"),
            })
            .collect()
    }
}

fn at(day: u32, h: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, day, h, 0, 0).unwrap()
}

fn record(check: &FitCheck) -> EtaFitRecord {
    record_for(check, Trigger::FleetRefresh, "host", at(5, 1), 7, &Provenance::current())
}

#[test]
fn every_outcome_maps_to_the_closed_vocabulary() {
    let stale = FitSkip::StaleBeforeGrace {
        oldest_as_of: at(4, 20),
        grace_at: at(5, 6),
        snapshots: 3,
    };
    let cases: Vec<(&str, FitCheck, &str, Option<&str>)> = vec![
        ("disabled", FitCheck::Disabled, "skipped", Some("disabled")),
        ("held", FitCheck::Held, "skipped", Some("held")),
        (
            "today_exists",
            FitCheck::Skipped(FitSkip::TodayExists {
                fit_id: "abc".into(),
            }),
            "skipped",
            Some("today_exists"),
        ),
        (
            "no_snapshots",
            FitCheck::Skipped(FitSkip::NoSnapshots),
            "skipped",
            Some("no_snapshots"),
        ),
        ("stale", FitCheck::Skipped(stale), "skipped", Some("stale_before_grace")),
        ("error", FitCheck::Failed("boom".into()), "error", None),
        ("panic", FitCheck::Panicked, "panic", None),
    ];
    for (name, check, outcome, reason) in cases {
        let r = record(&check);
        assert_eq!(r.outcome, outcome, "{name}");
        assert_eq!(r.skip_reason.as_deref(), reason, "{name}");
        if let Some(reason) = reason {
            assert!(crate::telemetry::kinds::eta_fit::SKIP_REASONS.contains(&reason), "{name}");
        }
        assert_eq!(r.error.is_some(), outcome == "error", "{name}");
        assert!(r.has_provenance(), "{name}");
        assert_eq!(r.duration_ms, 7);
        assert_eq!(r.trigger, "fleet_refresh");
    }
    let today = record(&FitCheck::Skipped(FitSkip::TodayExists {
        fit_id: "abc".into(),
    }));
    assert_eq!(today.fit_id.as_deref(), Some("abc"));
    assert_eq!(today.coeff_file.as_deref(), Some("fit-20261005T000000Z.json"));
    let none = record(&FitCheck::Skipped(FitSkip::NoSnapshots));
    assert_eq!(none.snapshots, 0);
    assert_eq!(none.snapshot_oldest_as_of, None, "absent is never zero");
    let stale = record(&FitCheck::Skipped(FitSkip::StaleBeforeGrace {
        oldest_as_of: at(4, 20),
        grace_at: at(5, 6),
        snapshots: 3,
    }));
    assert_eq!((stale.snapshots, stale.snapshot_oldest_as_of), (3, Some(at(4, 20))));
}

#[test]
fn check_id_is_derived_and_distinct_per_host_and_instant() {
    let a = record(&FitCheck::Held);
    assert_eq!(a, record(&FitCheck::Held), "deterministic");
    let b = record_for(&FitCheck::Held, Trigger::DailyTask, "other", at(5, 1), 1, &a.loom);
    assert_ne!(a.check_id, b.check_id);
    assert_eq!(a.check_id.len(), 16);
}

#[test]
fn the_error_text_is_truncated_on_a_char_boundary() {
    let long = "é".repeat(400);
    let r = record(&FitCheck::Failed(long));
    let error = r.error.unwrap();
    assert!(error.len() <= MAX_ERROR_BYTES);
    assert!(error.chars().all(|c| c == 'é'));
}

fn snapshot_as_of(root: &Path, repo: &str, as_of: DateTime<Utc>) {
    let mut snapshot = FleetSnapshot::empty(repo);
    snapshot.merge(&[], as_of);
    fleet::write(&fleet::snapshot_path(root, repo), &snapshot).unwrap();
}

/// Drive `after_cycle` and `finish` the way the refresh tick does.
fn tick(root: &Path, sink: &Capture, now: DateTime<Utc>, held: bool, enabled: bool) -> FitCheck {
    let check = after_cycle(root, now, held, enabled, &run::current_fitter());
    assert!(finish(
        root,
        Some(sink),
        "host",
        Trigger::FleetRefresh,
        now,
        Duration::from_millis(3),
        &check
    ));
    check
}

#[test]
fn after_cycle_emits_exactly_one_record_per_check_across_every_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let sink = Capture::default();
    let mut reasons = Vec::new();
    let mut expect = |sink: &Capture, outcome: &str, reason: Option<&str>| {
        let all = sink.records();
        let last = all.last().unwrap();
        assert_eq!(all.len(), reasons.len() + 1, "one record per check");
        assert_eq!(last.outcome, outcome);
        assert_eq!(last.skip_reason.as_deref(), reason);
        reasons.push(outcome.to_string());
        // fit-check.json is the last record, byte for byte.
        let on_disk = std::fs::read_to_string(crate::eta::health::fit_check_path(root)).unwrap();
        assert_eq!(on_disk, serde_json::to_string(last).unwrap());
    };

    tick(root, &sink, at(5, 1), false, false);
    expect(&sink, "skipped", Some("disabled"));
    tick(root, &sink, at(5, 1), true, true);
    expect(&sink, "skipped", Some("held"));
    tick(root, &sink, at(5, 1), false, true);
    expect(&sink, "skipped", Some("no_snapshots"));

    snapshot_as_of(root, "acme/alpha", at(4, 12));
    tick(root, &sink, at(5, 2), false, true);
    expect(&sink, "skipped", Some("stale_before_grace"));

    // A fresh snapshot: the fit is written.
    snapshot_as_of(root, "acme/alpha", at(5, 3));
    let check = tick(root, &sink, at(5, 4), false, true);
    assert!(matches!(check, FitCheck::Wrote(_)));
    expect(&sink, "written", None);
    let written = sink.records().pop().unwrap();
    assert_eq!(written.snapshots, 1);
    assert!(written.window_days.is_some_and(|d| d > 0));
    assert!(written
        .coeff_sha256
        .as_deref()
        .is_some_and(|h| h.len() == 64));
    assert!(written.coeff_bytes.is_some_and(|b| b > 0));
    assert!(written.snapshot_as_of.contains_key("acme/alpha"));
    assert!(written
        .coeff_file
        .as_deref()
        .is_some_and(|n| n.starts_with("fit-")));
    assert!(!written.stages.is_empty());

    tick(root, &sink, at(5, 5) + Span::minutes(5), false, true);
    expect(&sink, "skipped", Some("today_exists"));
    let again = sink.records();
    assert_eq!(again.last().unwrap().fit_id, written.fit_id, "joins the written id");
}

#[test]
fn a_record_with_invalid_provenance_is_never_emitted() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Capture::default();
    let mut bad = record(&FitCheck::Held);
    bad.loom.revision = "not-a-sha".into();
    assert!(!bad.has_provenance());
    assert!(!emit_record(dir.path(), Some(&sink), "host", bad));
    assert!(sink.records().is_empty());
    assert!(!crate::eta::health::fit_check_path(dir.path()).exists());
}
