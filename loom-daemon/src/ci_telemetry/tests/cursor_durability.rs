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
            ..Default::default()
        };
        export::save_cursor_with(dir.path(), &old, &|_| Ok(())).unwrap();
        let new = ExportCursor {
            byte_offset: 99,
            exported: 9,
            ..Default::default()
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
                ..Default::default()
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
            exported: 2,
            ..Default::default()
        }
    );
    std::fs::write(&file, "{not json").unwrap();
    assert_eq!(export::load_cursor(dir.path()), ExportCursor::default());
}

/// Identities of every envelope the sink received (order-insensitive).
fn offered_lines(sink: &VecSink) -> Vec<String> {
    let mut lines: Vec<String> = sink
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|e| serde_json::to_string(e).unwrap())
        .collect();
    lines.sort();
    lines
}

/// Journal text with a cursor that trails EOF by all but the first line, plus
/// the staged tail, as a tail-carrying rotation would have them.
fn tail_scenario(root: &Path) -> (String, String, PathBuf) {
    let text = generation(root);
    assert!(valid_lines(&text) >= 2, "fixture needs a tail");
    let first_end = text.find('\n').unwrap() + 1;
    let path = journal_path(root);
    let staged = rotation::carry_tail(&path, first_end as u64).unwrap();
    (text.clone(), text[first_end..].to_string(), staged)
}

/// #13636: a crash between the two renames of a tail-carrying rotation leaves
/// no journal; the next export pass must put the tail back and offer it.
#[test]
fn tail_carry_crash_between_renames_loses_no_line() {
    let dir = TempDir::new().unwrap();
    let (text, tail, staged) = tail_scenario(dir.path());
    let path = journal_path(dir.path());
    // The durable cursor reset happened, then the crash: journal moved to
    // `.1`, tail not yet in place.
    export::save_cursor_with(dir.path(), &ExportCursor::default(), &|_| Ok(())).unwrap();
    rotation::rotate(&path, 1).unwrap();
    assert!(!path.exists() && staged.exists());
    assert_eq!(std::fs::read_to_string(rotated_path(&path, 1)).unwrap(), text);

    let sink = sink();
    export::backfill(dir.path(), &sink);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), tail, "tail restored as the journal");
    assert!(!staged.exists());
    let expected: Vec<String> = {
        let mut v: Vec<String> = tail
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::to_string(&serde_json::from_str::<TelemetryEnvelope>(l).unwrap())
                    .unwrap()
            })
            .collect();
        v.sort();
        v
    };
    assert_eq!(offered_lines(&sink), expected, "every carried line was offered");
}

/// #13636: the poller recreated the journal before the next export pass: the
/// staged tail is appended to it, not lost and not overwritten.
#[test]
fn tail_carry_recovery_appends_to_a_recreated_journal() {
    let dir = TempDir::new().unwrap();
    let (_, tail, staged) = tail_scenario(dir.path());
    let path = journal_path(dir.path());
    rotation::rotate(&path, 1).unwrap();
    std::fs::write(&path, "{\"fresh\":true}\n").unwrap();
    assert!(rotation::recover_carry(&path).unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{{\"fresh\":true}}\n{tail}"));
    assert!(!staged.exists());
    assert!(!rotation::recover_carry(&path).unwrap(), "idempotent");
}

/// #13636: a crash after staging but before the rotation leaves the full old
/// journal; recovery just drops the staging file (no duplicated tail).
#[test]
fn tail_carry_recovery_drops_a_stale_staging_file() {
    let dir = TempDir::new().unwrap();
    let (text, _, staged) = tail_scenario(dir.path());
    let path = journal_path(dir.path());
    assert!(rotation::recover_carry(&path).unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    assert!(!staged.exists());
}

/// #13636 (criterion 4): a failure at each step of the cursor reset with a
/// non-empty tail leaves the journal intact, and a restart loses no line.
#[test]
fn failed_durable_reset_with_a_tail_loses_no_line() {
    for step in STEPS {
        let dir = TempDir::new().unwrap();
        let text = generation(dir.path());
        let path = journal_path(dir.path());
        let first_end = text.find('\n').unwrap() as u64 + 1;
        let mut cursor = ExportCursor {
            byte_offset: first_end,
            fast_offset: None,
            exported: 1,
        };
        let result =
            export::rotate_if_due_with(dir.path(), &path, &mut cursor, policy(1), &failing(step));
        assert!(result.is_err(), "{step:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "{step:?}: journal intact");
        assert!(!rotated_path(&path, 1).exists(), "{step:?}");
        // The staging file is either gone or recoverable without loss.
        let sink = sink();
        set_rotation(dir.path(), u64::MAX, 1);
        export::backfill(dir.path(), &sink);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "{step:?}");
        assert!(!rotation::carry_path(&path).exists(), "{step:?}");
    }
}

/// #13636: a rotation that carries a tail leaves cursor 0 and the tail first.
#[test]
fn tail_carry_rotation_resets_the_cursor_and_puts_the_tail_first() {
    let dir = TempDir::new().unwrap();
    let (text, _) = exported_generation(dir.path());
    let path = journal_path(dir.path());
    let first_end = text.find('\n').unwrap() as u64 + 1;
    let mut cursor = ExportCursor {
        byte_offset: first_end,
        fast_offset: Some(first_end + 1),
        exported: 1,
    };
    assert!(export::rotate_if_due(dir.path(), &path, &mut cursor, policy(1)).unwrap());
    assert_eq!(cursor.byte_offset, 0);
    assert_eq!(cursor.fast_offset, None);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text[first_end as usize..]);
    assert_eq!(std::fs::read_to_string(rotated_path(&path, 1)).unwrap(), text);
}
