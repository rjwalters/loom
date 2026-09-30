//! The stage-sample journal.

use super::{as_of, provenance};
use crate::eta::history::{SampleSource, StageSamples};
use crate::eta::journal::entry_transitions;
use crate::eta::journal::{append, entries_from_pr_history, read, JournalEntry, JOURNAL_SCHEMA};
use crate::eta::Stage;
use crate::pr_latency::history::fixtures::{labeled, merged, pushed, unlabeled};
use crate::pr_latency::history::PrEvent;
use crate::pr_latency::{APPROVED, CHANGES_REQUESTED, REVIEW_REQUESTED};
use chrono::Duration;

fn completed(stage: Stage, secs: i64, in_sweep: bool) -> JournalEntry {
    let mut row = JournalEntry::new("label.transition", "rjwalters/loom", as_of(), &provenance());
    row.issue = Some(1);
    row.stage = Some(stage);
    row.entered_at = Some(as_of() - Duration::seconds(secs));
    row.left_at = Some(as_of());
    row.duration_sec = Some(secs);
    row.in_sweep = in_sweep;
    row
}

#[test]
fn journal_round_trips_and_feeds_only_external_complete_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".loom/logs/eta-stage-samples.jsonl");
    let mut raw_only =
        JournalEntry::new("sweep.dispatch", "rjwalters/loom", as_of(), &provenance());
    raw_only.raw = serde_json::json!({"sweep_id": "sweep-issue-1-1"});
    let mut verdict = completed(Stage::ReviewWait, 600, false);
    verdict.verdict = Some("fail".to_string());
    verdict.attempt = Some(1);
    let rows = vec![
        raw_only,
        completed(Stage::ReviewWait, 900, false),
        completed(Stage::ReviewWait, 300, true), // also in sweep.outcome: not re-read
        verdict,
    ];
    append(&path, &rows[..2]).unwrap();
    append(&path, &rows[2..]).unwrap();
    let back = read(&path);
    assert_eq!(back, rows, "append-only, in order, every raw field kept");
    assert!(back.iter().all(|r| r.schema == JOURNAL_SCHEMA));
    assert!(back.iter().all(|r| r.loom == provenance()), "every row names its build");

    let mut history = StageSamples::default();
    history.push_journal(&back, "host-test");
    assert!(history.stages.iter().all(|s| s.host == "host-test"));
    let durations: Vec<i64> = history.stages.iter().map(|s| s.duration_sec).collect();
    assert_eq!(durations, vec![900, 600]);
    assert!(history
        .stages
        .iter()
        .all(|s| s.source == SampleSource::StageJournal));
    assert_eq!(history.verdicts.len(), 1);
    assert!(history.verdicts[0].rejected);
}

#[test]
fn entries_from_pr_history_derive_review_wait_doctor_and_merge_wait_rows() {
    // requested 1h -> changes 2h -> push 3h -> requested 4h -> approved 6h ->
    // merged 7h (the same shape as `pr_latency::segments`'s own repair-lap
    // fixture, so the two derivations can be sanity-checked against each
    // other by inspection).
    const HOUR: i64 = 3600;
    let h = merged(
        11,
        7 * HOUR,
        vec![
            labeled(REVIEW_REQUESTED, HOUR),
            labeled(CHANGES_REQUESTED, 2 * HOUR),
            pushed(3 * HOUR),
            labeled(REVIEW_REQUESTED, 4 * HOUR),
            labeled(APPROVED, 6 * HOUR),
        ],
    );
    let rows = entries_from_pr_history(&h, "rjwalters/loom", &provenance());
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().all(|r| r.repo == "rjwalters/loom"));
    assert!(rows.iter().all(|r| r.pr_number == Some(11)));
    assert!(rows.iter().all(|r| !r.in_sweep), "backfill never claims in-sweep coverage");
    assert!(rows.iter().all(|r| r.resolution_sec == Some(0)), "forge label events are exact");

    let review: Vec<&JournalEntry> = rows
        .iter()
        .filter(|r| r.stage == Some(Stage::ReviewWait))
        .collect();
    assert_eq!(review.len(), 2);
    assert_eq!(review[0].duration_sec, Some(HOUR));
    assert_eq!(review[0].next_stage, Some(Stage::Doctor));
    assert_eq!(review[0].verdict.as_deref(), Some("fail"));
    assert_eq!(review[0].attempt, Some(1));
    assert_eq!(review[1].duration_sec, Some(2 * HOUR));
    assert_eq!(review[1].next_stage, Some(Stage::MergeWait));
    assert_eq!(review[1].verdict.as_deref(), Some("pass"));
    assert_eq!(review[1].attempt, Some(2));

    let doctor = rows
        .iter()
        .find(|r| r.stage == Some(Stage::Doctor))
        .unwrap();
    assert_eq!(doctor.duration_sec, Some(HOUR));
    assert_eq!(doctor.next_stage, Some(Stage::ReviewWait));

    let merge = rows
        .iter()
        .find(|r| r.stage == Some(Stage::MergeWait))
        .unwrap();
    assert_eq!(merge.duration_sec, Some(HOUR));
    assert_eq!(merge.next_stage, None);
    assert_eq!(merge.event, "pr.resolved");

    // Every row is a well-formed history sample once read back, feeding both
    // the stage distributions and the verdict-count branch probabilities.
    let mut samples = StageSamples::default();
    samples.push_journal(&rows, "host-test");
    assert_eq!(samples.stages.len(), 4);
    assert_eq!(samples.verdicts.len(), 2);
    assert!(samples
        .verdicts
        .iter()
        .any(|v| v.attempt == 1 && v.rejected));
    assert!(samples
        .verdicts
        .iter()
        .any(|v| v.attempt == 2 && !v.rejected));
}

#[test]
fn a_merge_with_no_approval_label_yields_no_merge_wait_row() {
    let h = merged(1, 3600, vec![labeled(REVIEW_REQUESTED, 0)]);
    assert!(entries_from_pr_history(&h, "rjwalters/loom", &provenance()).is_empty());
}

#[test]
fn entry_transitions_ignore_a_re_application_and_keep_a_real_second_lap() {
    let never = |_: &PrEvent| false;
    let h = merged(
        1,
        9000,
        vec![
            labeled(REVIEW_REQUESTED, 100),
            labeled(REVIEW_REQUESTED, 101), // concurrent agents, same state
            unlabeled(REVIEW_REQUESTED, 500),
            labeled(REVIEW_REQUESTED, 900), // a genuine second lap
        ],
    );
    let entries = entry_transitions(&h, REVIEW_REQUESTED, &never);
    assert_eq!(entries.len(), 2, "a re-application is not a second entry");
    assert_eq!(
        (entries[1] - entries[0]).num_seconds(),
        800,
        "the second entry is the lap at t+900, not the duplicate at t+101"
    );
}

#[test]
fn an_event_that_clears_the_stage_releases_it_without_an_unlabeled() {
    // The real shape: the fleet leaves `loom:review-requested` in place
    // after a verdict, so only `clears` can see the second lap.
    let h = merged(
        1,
        9000,
        vec![
            labeled(REVIEW_REQUESTED, 100),
            labeled(APPROVED, 500),
            labeled(REVIEW_REQUESTED, 900),
        ],
    );
    let never = |_: &PrEvent| false;
    assert_eq!(
        entry_transitions(&h, REVIEW_REQUESTED, &never).len(),
        1,
        "without a clearing event the lap is invisible"
    );
    let approved_clears =
        |e: &PrEvent| matches!(e, PrEvent::Labeled { label, .. } if label == APPROVED);
    assert_eq!(entry_transitions(&h, REVIEW_REQUESTED, &approved_clears).len(), 2);
}

/// Observed live on PR #9569 during #9325's manual backfill: two
/// `loom:review-requested` labelings a second apart, both answered by one
/// verdict. Counting labelings produced two near-duplicate `review_wait`
/// rows — double-weighting one observation and claiming a second Judge
/// attempt that never happened.
#[test]
fn a_doubly_applied_review_request_is_one_wait_not_two() {
    const HOUR: i64 = 3600;
    let h = merged(
        9569,
        3 * HOUR,
        vec![
            labeled(REVIEW_REQUESTED, HOUR),
            labeled(REVIEW_REQUESTED, HOUR + 1),
            labeled(APPROVED, 2 * HOUR),
        ],
    );
    let rows = entries_from_pr_history(&h, "rjwalters/loom", &provenance());
    let review: Vec<&JournalEntry> = rows
        .iter()
        .filter(|r| r.stage == Some(Stage::ReviewWait))
        .collect();
    assert_eq!(review.len(), 1, "one verdict answered one wait");
    assert_eq!(review[0].attempt, Some(1), "not a second Judge attempt");
    assert_eq!(review[0].duration_sec, Some(HOUR));
}

#[test]
fn a_doubly_applied_rejection_is_one_doctor_lap_not_two() {
    const HOUR: i64 = 3600;
    let h = merged(
        2,
        5 * HOUR,
        vec![
            labeled(REVIEW_REQUESTED, HOUR),
            labeled(CHANGES_REQUESTED, 2 * HOUR),
            labeled(CHANGES_REQUESTED, 2 * HOUR + 1),
            pushed(3 * HOUR),
            labeled(APPROVED, 4 * HOUR),
        ],
    );
    let rows = entries_from_pr_history(&h, "rjwalters/loom", &provenance());
    let doctor: Vec<&JournalEntry> = rows
        .iter()
        .filter(|r| r.stage == Some(Stage::Doctor))
        .collect();
    assert_eq!(doctor.len(), 1, "one push answered one lap");
    assert_eq!(doctor[0].duration_sec, Some(HOUR));
}

#[test]
fn journal_rotates_to_one_generation_and_reads_both() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("eta-stage-samples.jsonl");
    // An old first row makes the live file stale: the next append rotates.
    let mut old = completed(Stage::Doctor, 60, false);
    old.observed_at = chrono::Utc::now() - Duration::days(40);
    append(&path, &[old.clone()]).unwrap();
    let fresh = completed(Stage::Doctor, 120, false);
    append(&path, std::slice::from_ref(&fresh)).unwrap();
    assert!(dir.path().join("eta-stage-samples.jsonl.1").exists());
    assert_eq!(read(&path), vec![old, fresh], "rotated generation first");
}
