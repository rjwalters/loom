//! Point-in-time filtering (#10511): the leak test — a record observed after
//! the cutoff is ignored, whatever its event time says.

use super::{as_of, provenance};
use crate::eta::history::{SampleSource, StageSamples};
use crate::eta::journal::JournalEntry;
use crate::eta::point_in_time::{knowable_at, knowable_by, Observed};
use crate::eta::{Stage, MIN_SAMPLES};
use crate::telemetry::ci::{CiDurationMetric, CiDurationRecord, CiJobRecord, CiRunRecord};
use crate::telemetry::RepoVisibility;
use chrono::{DateTime, Duration, Utc};

const REPO: &str = "rjwalters/loom";

/// A completed `review_wait` row whose segment ended (event time) at
/// `left_at` and which the tracker observed at `observed_at`.
fn row(left_at: DateTime<Utc>, observed_at: DateTime<Utc>, secs: i64) -> JournalEntry {
    let mut row = JournalEntry::new("label.transition", REPO, observed_at, &provenance());
    row.pr_number = Some(7);
    row.stage = Some(Stage::ReviewWait);
    row.entered_at = Some(left_at - Duration::seconds(secs));
    row.left_at = Some(left_at);
    row.duration_sec = Some(secs);
    row
}

fn run(completed_at: DateTime<Utc>, observed_at: Option<DateTime<Utc>>) -> CiRunRecord {
    CiRunRecord {
        repo: REPO.into(),
        visibility: RepoVisibility::Public,
        run_id: 1,
        run_attempt: 1,
        workflow: "CI".into(),
        git_ref: Some("main".into()),
        head_sha: "0".repeat(40),
        event: "push".into(),
        status: "completed".into(),
        conclusion: Some("success".into()),
        triggered_by: None,
        started_at: completed_at - Duration::minutes(5),
        completed_at,
        duration_ms: 300_000,
        queued_ms: None,
        trigger_reason: None,
        observed_at,
    }
}

#[test]
fn knowable_by_admits_at_or_before_the_cutoff_and_never_an_unobserved_record() {
    let cutoff = as_of();
    assert!(knowable_by(Some(cutoff - Duration::seconds(1)), cutoff));
    assert!(knowable_by(Some(cutoff), cutoff), "observed exactly at the cutoff is knowable");
    assert!(!knowable_by(Some(cutoff + Duration::seconds(1)), cutoff));
    assert!(!knowable_by(None, cutoff), "no observation time is never knowable");
}

/// The leak: a row whose *event* happened before the cutoff but which was
/// only observed after it (a late poll, a backfill) must be ignored.
#[test]
fn a_journal_row_observed_after_the_cutoff_is_ignored_even_when_its_event_preceded_it() {
    let cutoff = as_of();
    let early = row(cutoff - Duration::hours(2), cutoff - Duration::hours(2), 600);
    let late = row(cutoff - Duration::hours(1), cutoff + Duration::minutes(1), 900);
    let rows = vec![early.clone(), late];
    let kept: Vec<&JournalEntry> = knowable_at(&rows, cutoff).collect();
    assert_eq!(kept, vec![&early]);
}

#[test]
fn a_ci_record_observed_after_the_cutoff_is_ignored_and_a_legacy_one_is_refused() {
    let cutoff = as_of();
    let completed = cutoff - Duration::minutes(30);
    let seen = run(completed, Some(cutoff - Duration::minutes(29)));
    let late = run(completed, Some(cutoff + Duration::minutes(10)));
    let legacy = run(completed, None);
    let rows = vec![seen.clone(), late, legacy];
    let kept: Vec<&CiRunRecord> = knowable_at(&rows, cutoff).collect();
    assert_eq!(kept, vec![&seen]);
}

#[test]
fn every_ci_kind_reports_its_own_observed_at() {
    let at = as_of();
    let job = CiJobRecord {
        repo: REPO.into(),
        visibility: RepoVisibility::Public,
        run_id: 1,
        job_id: 2,
        workflow: "CI".into(),
        job: "test".into(),
        runner: None,
        attempts: 1,
        status: "completed".into(),
        conclusion: None,
        timed_out: false,
        started_at: at,
        completed_at: at,
        duration_ms: 0,
        queued_ms: None,
        dependency_wait_ms: None,
        shard_index: None,
        shard_total: None,
        shard_kind: "none".into(),
        observed_at: Some(at),
    };
    let duration = CiDurationRecord {
        metric: CiDurationMetric::Job,
        repo: REPO.into(),
        visibility: RepoVisibility::Public,
        run_id: 1,
        run_attempt: 1,
        job_id: Some(2),
        workflow: "CI".into(),
        job: Some("test".into()),
        runner: None,
        conclusion: None,
        started_at: at,
        completed_at: at,
        duration_ms: 0,
        observed_at: None,
    };
    assert_eq!(job.observed_at(), Some(at));
    assert_eq!(Observed::observed_at(&duration), None);
    assert_eq!(Observed::observed_at(&run(at, Some(at))), Some(at));
}

/// Serving: the late row also cannot reach an estimator's selection. Without
/// it the selection is exactly the eight in-window samples; with it pushed
/// into history, still exactly those eight.
#[test]
fn serving_selection_ignores_a_row_observed_after_the_cutoff() {
    let cutoff = as_of();
    let mut rows: Vec<JournalEntry> = (0..MIN_SAMPLES)
        .map(|i| {
            let at = cutoff - Duration::hours(i64::try_from(i).unwrap() + 1);
            row(at, at, 600)
        })
        .collect();
    rows.push(row(cutoff - Duration::minutes(5), cutoff + Duration::minutes(1), 999_999));
    let mut samples = StageSamples::default();
    samples.push_journal(&rows, "host-a");
    let selection = samples
        .select(REPO, Stage::ReviewWait, cutoff, &[SampleSource::StageJournal])
        .expect("eight in-window samples are enough to select");
    assert_eq!(selection.sorted, vec![600; MIN_SAMPLES]);
}

/// The CI record wire shape: `observed_at` round-trips, and a pre-#10511
/// record (no field) still deserializes, as `None`.
#[test]
fn ci_run_observed_at_round_trips_and_is_optional_on_the_wire() {
    let at = as_of();
    let record = run(at, Some(at));
    let json = serde_json::to_value(&record).unwrap();
    assert_eq!(json["observed_at"], serde_json::json!(at));
    let back: CiRunRecord = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(back, record);

    let mut legacy = json;
    legacy.as_object_mut().unwrap().remove("observed_at");
    let back: CiRunRecord = serde_json::from_value(legacy).unwrap();
    assert_eq!(back.observed_at, None);
    assert!(
        !serde_json::to_string(&back)
            .unwrap()
            .contains("observed_at"),
        "an absent observation time is omitted, not serialized as null"
    );
}

/// The OTLP log attribute carries it, so a SigNoz reader can filter on it.
#[test]
fn ci_run_renders_observed_at_as_a_log_attribute_only_when_present() {
    let at = as_of();
    let attrs = run(at, Some(at)).log_attributes();
    assert!(attrs
        .iter()
        .any(|(key, value)| *key == "loom.ci.observed_at"
            && *value == crate::telemetry::ci::CiAttr::Str(at.to_rfc3339())));
    assert!(!run(at, None)
        .log_attributes()
        .iter()
        .any(|(key, _)| *key == "loom.ci.observed_at"));
}
