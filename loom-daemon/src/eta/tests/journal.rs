//! The stage-sample journal.

use super::{as_of, provenance};
use crate::eta::history::{SampleSource, StageSamples};
use crate::eta::journal::{append, read, JournalEntry, JOURNAL_SCHEMA};
use crate::eta::Stage;
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
