//! The nightly stage-error rollup (#10957): point-in-time, exactly once, the
//! sum identity through the log, a fixed row budget, old files still load.

#![allow(clippy::unwrap_used)]

use super::provenance;
use crate::eta::attribution_log::{self, AttributionRow};
use crate::eta::nightly_folds::DayRecords;
use crate::eta::stage_attribution_fold::{cutoff, rollup};
use crate::eta::stage_forecast::{Attribution, StageError};
use crate::eta::{Kind, Stage};
use crate::telemetry::kinds::eta_stage_attribution::UNATTRIBUTED;
use chrono::{Duration, NaiveDate};
use std::collections::BTreeMap;

fn day() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, 5).unwrap()
}

/// A row for `heuristic` resolved `before_cutoff_sec` before `day`'s cutoff.
/// `builder` and `review` are the stage contributions; `unattributed` the
/// remainder.
fn row(
    id: &str,
    heuristic: &str,
    before_cutoff_sec: i64,
    builder: i64,
    review: i64,
    unattributed: i64,
) -> AttributionRow {
    let actual_at = cutoff(day()) - Duration::seconds(before_cutoff_sec);
    let stage = |c: i64| StageError {
        predicted_entry_sec: Some(0),
        predicted_dwell_sec: 100,
        actual_entry_sec: Some(0),
        actual_dwell_sec: 100 + c,
        contribution_sec: c,
    };
    let stages = BTreeMap::from([
        (Stage::SweepBuilder, stage(builder)),
        (Stage::ReviewWait, stage(review)),
    ]);
    let dominant_stage = if builder.abs() >= review.abs() {
        Stage::SweepBuilder
    } else {
        Stage::ReviewWait
    };
    AttributionRow {
        estimate_id: id.into(),
        heuristic: heuristic.into(),
        kind: Kind::Land,
        repo: "rjwalters/loom".into(),
        issue: 1,
        as_of: actual_at - Duration::hours(1),
        actual_at,
        observed_at: actual_at,
        error_sec: builder + review + unattributed,
        attribution: Attribution {
            stages,
            unattributed_sec: unattributed,
            dominant_stage: Some(dominant_stage),
        },
    }
}

fn records(
    rows: &[AttributionRow],
) -> Vec<crate::telemetry::kinds::eta_stage_attribution::EtaStageAttributionRecord> {
    rollup(rows, &["h1", "h2"], day(), &provenance())
}

fn find<'a>(
    out: &'a [crate::telemetry::kinds::eta_stage_attribution::EtaStageAttributionRecord],
    heuristic: &str,
    stage: &str,
) -> &'a crate::telemetry::kinds::eta_stage_attribution::EtaStageAttributionRecord {
    out.iter()
        .find(|r| r.heuristic == heuristic && r.stage == stage)
        .unwrap()
}

#[test]
fn rows_carry_bias_mean_abs_and_dominant_share_per_heuristic_and_stage() {
    let rows = [
        row("a", "h1", 100, 30, -10, 5),
        row("b", "h1", 200, -10, 50, -5),
        row("c", "h2", 300, 7, 0, 0),
    ];
    let out = records(&rows);
    let builder = find(&out, "h1", "sweep.builder");
    assert_eq!((builder.n, builder.bias_sec, builder.mean_abs_sec), (2, Some(10.0), Some(20.0)));
    assert_eq!(builder.dominant_share, Some(0.5));
    let un = find(&out, "h1", UNATTRIBUTED);
    assert_eq!(
        (un.n, un.bias_sec, un.mean_abs_sec, un.dominant_share),
        (2, Some(0.0), Some(5.0), None)
    );
    assert_eq!(find(&out, "h2", "sweep.builder").bias_sec, Some(7.0));
    let empty = find(&out, "h2", "doctor");
    assert_eq!((empty.n, empty.bias_sec, empty.dominant_share), (0, None, None));
}

#[test]
fn dominant_share_is_over_all_the_heuristics_outcomes_not_only_the_visitors() {
    // `d` never visited review_wait, so review_wait has n = 2 (a, b) while h1
    // has 3 outcomes in the window; only `b` is review-dominant.
    let mut d = row("d", "h1", 300, 40, 0, 0);
    d.attribution.stages.remove(&Stage::ReviewWait);
    d.error_sec = 40;
    let rows = [
        row("a", "h1", 100, 30, -10, 5),
        row("b", "h1", 200, -10, 50, -5),
        d,
    ];
    let out = records(&rows);
    let review = find(&out, "h1", "review_wait");
    assert_eq!(review.n, 2);
    assert_eq!(review.dominant_share, Some(0.3333), "1 of 3, not 1 of 2");
    let builder = find(&out, "h1", "sweep.builder");
    assert_eq!((builder.n, builder.dominant_share), (3, Some(0.6667)));
}

#[test]
fn an_outcome_after_the_cutoff_leaves_the_rows_bit_identical() {
    let base = vec![row("a", "h1", 100, 30, -10, 5)];
    let before = records(&base);
    let mut with_late = base.clone();
    // Resolved after the cutoff.
    with_late.push(row("late", "h1", -60, 999, 999, 999));
    // Resolved before it but scored (knowable) only after it.
    let mut scored_late = row("scored-late", "h1", 50, 999, 999, 999);
    scored_late.observed_at = cutoff(day()) + Duration::seconds(1);
    with_late.push(scored_late);
    assert_eq!(records(&with_late), before);
    // Resolved just inside the window is counted.
    with_late.push(row("in", "h1", 1, 10, 10, 10));
    assert_ne!(records(&with_late), before);
    // Resolved before the window opens is not.
    let old = row("old", "h1", 7 * 86_400 + 1, 999, 999, 999);
    assert_eq!(records(&[base[0].clone(), old]), before);
}

#[test]
fn a_row_is_counted_once_however_often_it_is_logged() {
    let a = row("a", "h1", 100, 30, -10, 5);
    let once = records(std::slice::from_ref(&a));
    assert_eq!(find(&once, "h1", "sweep.builder").n, 1);
    let mut again = a.clone();
    again.observed_at += Duration::seconds(5);
    assert_eq!(records(&[a.clone(), again, a.clone()]), once);
    assert_eq!(records(std::slice::from_ref(&a)), once, "a re-run is the same records");
}

#[test]
fn the_sum_identity_survives_the_log_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = attribution_log::path(dir.path());
    let rows = [
        row("a", "h1", 100, 30, -10, 5),
        row("b", "h1", 200, -10, 50, -5),
    ];
    attribution_log::append(&path, &rows[..1]).unwrap();
    attribution_log::append(&path, &rows[1..]).unwrap();
    let back = attribution_log::read(&path);
    assert_eq!(back, rows);
    for r in &back {
        let explained: i64 = r
            .attribution
            .stages
            .values()
            .map(|s| s.contribution_sec)
            .sum();
        assert_eq!(explained + r.attribution.unattributed_sec, r.error_sec);
    }
    // And the parts of the mean error are the rollup's biases.
    let out = records(&back);
    let parts: f64 = ["sweep.builder", "review_wait", UNATTRIBUTED]
        .iter()
        .map(|s| find(&out, "h1", s).bias_sec.unwrap())
        .sum();
    let mean_error = back.iter().map(|r| r.error_sec as f64).sum::<f64>() / back.len() as f64;
    assert!((parts - mean_error).abs() < 1e-3);
}

#[test]
fn the_row_count_is_heuristics_times_eight_whatever_the_outcome_count() {
    assert_eq!(records(&[]).len(), 2 * 8);
    let many: Vec<AttributionRow> = (0..500)
        .map(|i| row(&format!("e{i}"), if i % 2 == 0 { "h1" } else { "h2" }, 100 + i, 1, 2, 3))
        .collect();
    let out = records(&many);
    assert_eq!(out.len(), 2 * 8);
    // A heuristic that is not registered gets no rows.
    let mut stray = many.clone();
    stray.push(row("x", "retired", 10, 1, 1, 1));
    assert_eq!(records(&stray), out);
    let ids: std::collections::BTreeSet<_> = out.iter().map(|r| r.row_id.clone()).collect();
    assert_eq!(ids.len(), out.len(), "row ids are distinct");
}

#[test]
fn a_day_file_saved_before_the_rollup_still_loads() {
    let old = r#"{"day":"2026-10-04","folds":[],"summaries":[]}"#;
    let parsed: DayRecords = serde_json::from_str(old).unwrap();
    assert!(parsed.stage_attribution.is_empty());
}
