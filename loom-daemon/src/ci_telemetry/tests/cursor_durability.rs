//! Crash-safe cursor persistence and rotation ordering (#11062).
//!
//! Failures are injected at each step of the durable cursor save through the
//! `save_cursor_with` / `rotate_if_due_with` seams (no chmod tricks, no claim
//! of simulated power loss), then the "restart" is a fresh `backfill`.

use std::io;

use super::super::export::{self, ExportCursor, SaveStep};
use super::super::rotation::{self, rotated_path, RotationPolicy};
use super::*;

const STEPS: [SaveStep; 5] = [
    SaveStep::CreateDir,
    SaveStep::WriteTemp,
    SaveStep::SyncFile,
    SaveStep::Rename,
    SaveStep::SyncDir,
];

fn failing(step: SaveStep) -> impl Fn(SaveStep) -> io::Result<()> {
    move |s| {
        if s == step {
            Err(io::Error::other("injected"))
        } else {
            Ok(())
        }
    }
}

fn sink() -> VecSink {
    VecSink(Mutex::new(Vec::new()))
}

/// Journal text of one generation, as complete lines.
fn generation(root: &Path) -> String {
    run_cycle(&ctx(root), &FixtureApi::new()).unwrap();
    std::fs::read_to_string(journal_path(root)).unwrap()
}

fn set_rotation(root: &Path, bytes: u64, keep: usize) {
    std::fs::create_dir_all(root.join(".loom")).unwrap();
    std::fs::write(
        root.join(".loom/config.json"),
        format!(r#"{{"autonomous":{{"ciTelemetry":{{"journalRotateBytes":{bytes},"journalRotateKeep":{keep}}}}}}}"#),
    )
    .unwrap();
}

fn policy(keep: usize) -> RotationPolicy {
    RotationPolicy {
        threshold_bytes: 1,
        keep,
    }
}

fn valid_lines(text: &str) -> usize {
    text.lines().filter(|l| !l.trim().is_empty()).count()
}

/// Journal fully exported (no rotation yet) so the cursor sits at EOF.
fn exported_generation(root: &Path) -> (String, ExportCursor) {
    let text = generation(root);
    set_rotation(root, u64::MAX, 1);
    assert_eq!(export::backfill(root, &sink()), valid_lines(&text));
    let cursor = export::load_cursor(root);
    assert_eq!(cursor.byte_offset, text.len() as u64);
    (text, cursor)
}

#[test]
fn every_save_step_failure_is_reported_and_leaves_the_old_cursor_intact() {
    for step in STEPS {
        let dir = TempDir::new().unwrap();
        let old = ExportCursor {
            byte_offset: 11,
            exported: 3,
        };
        export::save_cursor_with(dir.path(), &old, &|_| Ok(())).unwrap();
        let new = ExportCursor {
            byte_offset: 99,
            exported: 9,
        };
        let result = export::save_cursor_with(dir.path(), &new, &failing(step));
        if step == SaveStep::SyncDir {
            // Renamed already: visible, but the failure is still reported.
            assert!(result.is_err());
            assert_eq!(export::load_cursor(dir.path()), new);
        } else {
            assert!(result.is_err(), "{step:?} failure must propagate");
            assert_eq!(export::load_cursor(dir.path()), old, "{step:?}");
        }
        let leftovers = std::fs::read_dir(state_dir(dir.path()))
            .map(|d| {
                d.filter_map(Result::ok)
                    .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(leftovers, 0, "{step:?} left a temporary file");
    }
}

#[test]
fn failed_durable_reset_stops_rotation_and_a_restart_exports_everything() {
    for keep in [0usize, 1] {
        for step in STEPS {
            let dir = TempDir::new().unwrap();
            let (text, mut cursor) = exported_generation(dir.path());
            let path = journal_path(dir.path());

            let result = export::rotate_if_due_with(
                dir.path(),
                &path,
                &mut cursor,
                policy(keep),
                &failing(step),
            );
            assert!(result.is_err(), "keep={keep} {step:?}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "journal untouched");
            assert!(!rotated_path(&path, 1).exists());

            // Restart: whatever the cursor says, the journal is intact and a
            // fresh pass rotates and loses nothing.
            let sink = sink();
            export::backfill(dir.path(), &sink);
            set_rotation(dir.path(), 1, keep);
            export::backfill(dir.path(), &sink);
            assert!(!path.exists());
            assert_eq!(export::load_cursor(dir.path()).byte_offset, 0);
        }
    }
}

#[test]
fn crash_after_durable_reset_never_skips_a_longer_new_generation() {
    for keep in [0usize, 1] {
        let dir = TempDir::new().unwrap();
        let (text, cursor) = exported_generation(dir.path());
        let path = journal_path(dir.path());

        // Interrupt right after the durable zero save, before the rename:
        // the process dies here.
        export::save_cursor_with(
            dir.path(),
            &ExportCursor {
                byte_offset: 0,
                exported: cursor.exported,
            },
            &|_| Ok(()),
        )
        .unwrap();
        // ...then the rotation completes in the "next process".
        rotation::rotate(&path, keep).unwrap();

        // New generation LONGER than the previous cursor offset.
        std::fs::write(&path, format!("{text}{text}")).unwrap();
        let sink = sink();
        let offered = export::backfill(dir.path(), &sink);
        assert_eq!(offered, valid_lines(&text) * 2, "keep={keep}");
        assert_eq!(sink.0.lock().unwrap().len(), valid_lines(&text) * 2);
    }
}

#[test]
fn successful_rotation_saves_a_zero_offset_and_keeps_exported() {
    let dir = TempDir::new().unwrap();
    let (text, mut cursor) = exported_generation(dir.path());
    let path = journal_path(dir.path());
    assert!(export::rotate_if_due(dir.path(), &path, &mut cursor, policy(1)).unwrap());
    let loaded = export::load_cursor(dir.path());
    assert_eq!(loaded.byte_offset, 0);
    assert_eq!(loaded.exported, valid_lines(&text) as u64);
    assert!(rotated_path(&path, 1).exists());
}

#[test]
fn legacy_missing_and_corrupt_cursors_stay_safe() {
    let dir = TempDir::new().unwrap();
    assert_eq!(export::load_cursor(dir.path()), ExportCursor::default());
    let state = state_dir(dir.path());
    std::fs::create_dir_all(&state).unwrap();
    let file = state.join("export-cursor.json");
    std::fs::write(&file, r#"{"byte_offset":5,"exported":2}"#).unwrap();
    assert_eq!(
        export::load_cursor(dir.path()),
        ExportCursor {
            byte_offset: 5,
            exported: 2
        }
    );
    std::fs::write(&file, "{not json").unwrap();
    assert_eq!(export::load_cursor(dir.path()), ExportCursor::default());
}
