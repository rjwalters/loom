//! Payload archive tests (#12974): round trip, replay from the archive,
//! retention, an unwritable directory, and the export path.

use std::sync::{Arc, Mutex};

use chrono::NaiveDate;

use super::*;
use crate::observability::ops::OpsSink;
use crate::observability::queue::QueueSink;
use crate::telemetry::kinds::pick_summary::{tests::worst_case, SUMMARY_MAX_BYTES};
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

#[derive(Default)]
struct Recording(Mutex<Vec<TelemetryEnvelope>>);

impl QueueSink for Recording {
    fn offer(&self, envelope: TelemetryEnvelope) {
        self.0.lock().unwrap().push(envelope);
    }
    fn offer_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()> {
        self.offer(envelope);
        Ok(())
    }
}

fn sink(dir: &Path) -> LocalDirSink {
    LocalDirSink {
        dir: dir.to_path_buf(),
        max_age_days: 14,
        max_bytes: u64::MAX,
    }
}

#[test]
fn the_archived_payload_is_the_exact_pre_change_record() {
    let tmp = tempfile::tempdir().unwrap();
    let sink = sink(tmp.path());
    let record = worst_case();
    let payload_ref = archive_record(&sink, &record);
    assert_eq!(payload_ref, "work_finder-77");
    let path = sink.path_for(&payload_ref, record.ended_at.date_naive());
    assert!(path.ends_with("2026-10-20/work_finder-77.json.zst"), "{path:?}");
    let back = read_ref(&sink, &payload_ref, record.ended_at).unwrap();
    assert_eq!(back, record);
    assert_eq!(back.candidates.len(), 400);
    assert_eq!(back.workspace_draw.unwrap().draws.len(), 150);
    assert_eq!(back.item_draw.unwrap().pools.len(), 300);
}

#[test]
fn a_hostile_tick_id_cannot_leave_the_archive_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let sink = sink(tmp.path());
    let day = NaiveDate::from_ymd_opt(2026, 10, 20).unwrap();
    for id in ["../../etc/x", "..", "", "a/b"] {
        let p = sink.path_for(id, day);
        assert_eq!(p.parent().unwrap(), tmp.path().join("2026-10-20"), "{id:?} -> {p:?}");
    }
}

#[test]
fn retention_drops_old_days_then_the_oldest_files_past_the_size_bound() {
    let tmp = tempfile::tempdir().unwrap();
    let mut sink = sink(tmp.path());
    let d = |day| NaiveDate::from_ymd_opt(2026, 10, day).unwrap();
    for (day, id) in [(1, "a"), (18, "b"), (19, "c"), (20, "d")] {
        let path = sink.path_for(id, d(day));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![0u8; 100]).unwrap();
    }
    sink.max_bytes = 250;
    sink.prune(d(20));
    assert!(!sink.path_for("a", d(1)).exists(), "past max_age_days");
    assert!(!sink.path_for("b", d(18)).exists(), "oldest past the size bound");
    assert!(sink.path_for("c", d(19)).exists() && sink.path_for("d", d(20)).exists());
}

#[test]
fn an_unwritable_archive_is_counted_and_the_summary_still_exports() {
    let tmp = tempfile::tempdir().unwrap();
    // The "directory" is a file, so no payload can be created under it.
    let blocked = tmp.path().join("file");
    std::fs::write(&blocked, b"x").unwrap();
    let before = failure_count();
    let queue = Arc::new(Recording::default());
    let ops = OpsSink::new(queue.clone(), "host-a");
    crate::observability::pick_decision::export_summary(&ops, &sink(&blocked), worst_case(), None);
    assert!(failure_count() > before);
    let got = queue.0.lock().unwrap();
    assert_eq!(got.len(), 1, "one summary, no chunks");
    let TelemetryRecord::PickDecision(s) = &got[0].record else {
        panic!("pick.decision")
    };
    assert_eq!(s.payload_ref.as_deref(), Some("work_finder-77"));
}

#[test]
fn export_archives_the_full_record_and_emits_one_bounded_summary() {
    let tmp = tempfile::tempdir().unwrap();
    let sink = sink(tmp.path());
    let queue = Arc::new(Recording::default());
    let ops = OpsSink::new(queue.clone(), "host-a");
    let full = worst_case();
    crate::observability::pick_decision::export_summary(&ops, &sink, full.clone(), None);
    let got = queue.0.lock().unwrap();
    assert_eq!(got.len(), 1);
    let TelemetryRecord::PickDecision(s) = &got[0].record else {
        panic!("pick.decision")
    };
    let bytes = serde_json::to_vec(s).unwrap().len();
    assert!(bytes <= SUMMARY_MAX_BYTES, "{bytes} bytes");
    assert!(s.candidates.is_empty());
    let archived = read_ref(&sink, s.payload_ref.as_deref().unwrap(), s.ended_at).unwrap();
    assert_eq!(archived, full);
}
