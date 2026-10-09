//! Journal memory bound and rotation (#11045).
//!
//! The 5.4 GB fleet-captain journal was read whole, several times and
//! concurrently, at every daemon start. These tests pin the replacement:
//! the torn-tail repair reads only the tail, export streams from the cursor,
//! the crash-recovery replay streams, the journal rotates behind the export
//! cursor under `poll.lock`, and the export pass and a poll cycle never run
//! at once. The source-scan half of the guard lives in
//! `loom-daemon/tests/ci_telemetry_journal_streaming.rs`.

use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::super::export::{self, ExportCursor};
use super::super::ledger::{repair_torn_tail, SCAN_BLOCK_BYTES};
use super::super::rotation::{self, rotated_path, RotationPolicy};
use super::*;

/// Counts offers without keeping them, so a large export stays O(1).
#[derive(Default)]
struct CountingSink(AtomicUsize);

impl crate::observability::queue::QueueSink for CountingSink {
    fn offer(&self, _envelope: TelemetryEnvelope) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn offer_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()> {
        self.offer(envelope);
        Ok(())
    }
}

/// Accepts `room` offers, then refuses every one after.
struct RefusingSink {
    room: AtomicUsize,
    taken: Mutex<Vec<TelemetryEnvelope>>,
}

impl crate::observability::queue::QueueSink for RefusingSink {
    fn offer(&self, envelope: TelemetryEnvelope) {
        self.taken.lock().unwrap().push(envelope);
    }
    fn offer_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()> {
        if self
            .room
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_err()
        {
            return Err(std::io::Error::other("queue full"));
        }
        self.offer(envelope);
        Ok(())
    }
}

fn sink() -> VecSink {
    VecSink(Mutex::new(Vec::new()))
}

fn set_rotation(root: &Path, bytes: u64, keep: usize) {
    std::fs::create_dir_all(root.join(".loom")).unwrap();
    std::fs::write(
        root.join(".loom/config.json"),
        format!(r#"{{"autonomous":{{"ciTelemetry":{{"journalRotateBytes":{bytes},"journalRotateKeep":{keep}}}}}}}"#),
    )
    .unwrap();
}

fn identities(envelopes: &[TelemetryEnvelope]) -> BTreeSet<String> {
    envelopes.iter().filter_map(envelope_identity).collect()
}

fn len(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

/// Every envelope in the journal and all its rotations.
fn journal_and_rotations(root: &Path) -> Vec<TelemetryEnvelope> {
    let path = journal_path(root);
    let mut all = Vec::new();
    for file in rotation::existing_rotations(&path)
        .into_iter()
        .rev()
        .chain(std::iter::once(path))
    {
        all.extend(Journal::reader(file).read_all().unwrap());
    }
    all
}

// ---------------------------------------------------------------------------
// Torn-tail repair reads only the tail
// ---------------------------------------------------------------------------

#[test]
fn torn_tail_repair_truncates_only_the_trailing_fragment() {
    let dir = TempDir::new().unwrap();
    let path = journal_path(dir.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();

    // A fragment longer than one scan block: the backwards scan must cross
    // block boundaries to find the last newline.
    let whole = "{\"line\":1}\n{\"line\":2}\n";
    let fragment = "x".repeat(SCAN_BLOCK_BYTES * 2 + 17);
    std::fs::write(&path, format!("{whole}{fragment}")).unwrap();
    assert!(repair_torn_tail(&path).unwrap());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), whole);
    assert!(!repair_torn_tail(&path).unwrap(), "a clean tail is left alone");

    // A file with no newline at all is all fragment.
    std::fs::write(&path, "no newline here").unwrap();
    Journal::open(path.clone()).unwrap();
    assert_eq!(len(&path), 0);

    // A missing file is fine.
    std::fs::remove_file(&path).unwrap();
    assert!(!repair_torn_tail(&path).unwrap());
}

// ---------------------------------------------------------------------------
// Export streams from the cursor
// ---------------------------------------------------------------------------

#[test]
fn export_resumes_from_a_mid_file_cursor_without_reoffering() {
    let dir = TempDir::new().unwrap();
    run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
    let all = journal(dir.path());
    let total = all.len();
    assert!(total > 10);

    // The queue fills after 7 offers: the cursor stops mid-file, exactly
    // after the 7th line.
    let first = RefusingSink {
        room: AtomicUsize::new(7),
        taken: Mutex::new(Vec::new()),
    };
    assert_eq!(export::backfill(dir.path(), &first), 7);
    let cursor = export::load_cursor(dir.path());
    assert_eq!(cursor.exported, 7);
    assert!(cursor.byte_offset > 0 && cursor.byte_offset < len(&journal_path(dir.path())));
    assert_eq!(export::pending_count(dir.path()), total - 7);

    // The next pass starts at the cursor: only the rest, each exactly once.
    let rest = sink();
    assert_eq!(export::backfill(dir.path(), &rest), total - 7);
    let mut offered = first.taken.into_inner().unwrap();
    offered.extend(rest.0.into_inner().unwrap());
    assert_eq!(offered.len(), total);
    assert_eq!(identities(&offered), identities(&all));
    assert_eq!(export::load_cursor(dir.path()).exported, total as u64);
}

#[test]
fn a_journal_shorter_than_the_cursor_restarts_from_zero() {
    let dir = TempDir::new().unwrap();
    run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
    let all = journal(dir.path());
    assert_eq!(export::backfill(dir.path(), &sink()), all.len());

    // The operator replaced the journal with a short one (#11045's own
    // mitigation): the cursor is now past its end.
    let short: String = all[..3]
        .iter()
        .map(|env| serde_json::to_string(env).unwrap() + "\n")
        .collect();
    std::fs::write(journal_path(dir.path()), short).unwrap();
    assert_eq!(export::pending_count(dir.path()), 3);
    let again = sink();
    assert_eq!(export::backfill(dir.path(), &again), 3);
    assert_eq!(export::load_cursor(dir.path()).byte_offset, len(&journal_path(dir.path())));
}

// ---------------------------------------------------------------------------
// Rotation
// ---------------------------------------------------------------------------

#[test]
fn rotation_policy_resolves_config_and_defaults() {
    assert_eq!(
        RotationPolicy::resolve(&CiTelemetryConfig::default()),
        RotationPolicy {
            threshold_bytes: rotation::DEFAULT_ROTATE_BYTES,
            keep: rotation::DEFAULT_ROTATE_KEEP,
        }
    );
    let dir = TempDir::new().unwrap();
    set_rotation(dir.path(), 4096, 5);
    let config = read_config(dir.path());
    assert_eq!(config.journal_rotate_bytes, Some(4096));
    assert_eq!(config.journal_rotate_keep, Some(5));
    if std::env::var(rotation::ROTATE_BYTES_ENV).is_err()
        && std::env::var(rotation::ROTATE_KEEP_ENV).is_err()
    {
        assert_eq!(
            RotationPolicy::resolve(&config),
            RotationPolicy {
                threshold_bytes: 4096,
                keep: 5
            }
        );
    }
}

#[test]
fn no_rotation_below_the_threshold() {
    let dir = TempDir::new().unwrap();
    run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
    let size = len(&journal_path(dir.path()));
    set_rotation(dir.path(), size + 1, 2);
    export::backfill(dir.path(), &sink());
    assert_eq!(len(&journal_path(dir.path())), size);
    assert!(!rotated_path(&journal_path(dir.path()), 1).exists());
    assert_eq!(export::load_cursor(dir.path()).byte_offset, size);
}

#[test]
fn rotation_waits_until_every_line_is_exported() {
    let dir = TempDir::new().unwrap();
    run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
    set_rotation(dir.path(), 1, 2);
    let path = journal_path(dir.path());

    // The queue refuses part-way: the cursor is past the threshold but not
    // at the end, so nothing rotates.
    let partial = RefusingSink {
        room: AtomicUsize::new(5),
        taken: Mutex::new(Vec::new()),
    };
    export::backfill(dir.path(), &partial);
    assert!(path.exists() && !rotated_path(&path, 1).exists());

    // A torn tail past the cursor also holds rotation back...
    let total = journal(dir.path()).len();
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend_from_slice(b"{\"schema_version\":8,");
    std::fs::write(&path, bytes).unwrap();
    assert_eq!(export::backfill(dir.path(), &sink()), total - 5);
    assert!(path.exists() && !rotated_path(&path, 1).exists());

    // ...until a writer repairs it; then the next pass rotates.
    Journal::open(path.clone()).unwrap();
    assert_eq!(export::backfill(dir.path(), &sink()), 0);
    assert!(!path.exists() && rotated_path(&path, 1).exists());
    assert_eq!(
        export::load_cursor(dir.path()),
        ExportCursor {
            byte_offset: 0,
            exported: total as u64
        }
    );
}

#[test]
fn rotation_keeps_at_most_n_files_and_loses_no_line() {
    let dir = TempDir::new().unwrap();
    set_rotation(dir.path(), 1, 2);
    let path = journal_path(dir.path());
    let journal = Journal::open(path.clone()).unwrap();
    let template = {
        run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
        let all = super::journal(dir.path());
        std::fs::remove_file(&path).unwrap();
        all
    };

    let exported = sink();
    let mut written = 0;
    for generation in 0..4 {
        // Each generation's batch is a distinct slice, so a lost or repeated
        // line shows up in the totals.
        let batch = &template[generation * 5..generation * 5 + 5];
        journal.append(batch).unwrap();
        written += batch.len();
        assert_eq!(export::backfill(dir.path(), &exported), batch.len());
        assert!(!path.exists(), "generation {generation} rotated");
        assert_eq!(export::load_cursor(dir.path()).byte_offset, 0);
    }
    // Four rotations, keep 2: only the newest two survive.
    assert!(rotated_path(&path, 1).exists());
    assert!(rotated_path(&path, 2).exists());
    assert!(!rotated_path(&path, 3).exists());
    let newest = Journal::reader(rotated_path(&path, 1)).read_all().unwrap();
    assert_eq!(identities(&newest), identities(&template[15..20]));

    let offered = exported.0.into_inner().unwrap();
    assert_eq!(offered.len(), written);
    assert_eq!(identities(&offered), identities(&template[..20]));
    assert_eq!(export::load_cursor(dir.path()).exported, written as u64);

    // Lowering keep prunes the surplus on the next rotation; keep 0 deletes.
    std::fs::write(rotated_path(&path, 7), "stale\n").unwrap();
    journal.append(&template[20..21]).unwrap();
    rotation::rotate(&path, 0).unwrap();
    assert!(rotation::existing_rotations(&path).is_empty());
    assert!(!path.exists());
}

#[test]
fn seen_dedupe_holds_across_rotation() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    run_cycle(&ctx(dir.path()), &api).unwrap();
    let first = journal(dir.path());
    set_rotation(dir.path(), 1, 2);
    assert_eq!(export::backfill(dir.path(), &sink()), first.len());
    assert!(!journal_path(dir.path()).exists());

    // The same runs are listed again: the ledger, not the journal, dedupes.
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!((report.summary.runs_emitted, report.summary.jobs_emitted), (0, 0));
    assert!(journal(dir.path()).is_empty());
    assert_eq!(export::backfill(dir.path(), &sink()), 0);
}

#[test]
fn recovery_after_a_rotation_does_not_rejournal_rotated_lines() {
    let dir = TempDir::new().unwrap();
    // Commit run 1002's units and journal them, then "die" before the
    // ledger records the emit.
    let mut ledger = Ledger::open(state_dir(dir.path()).join("seen.jsonl")).unwrap();
    let committed = ledger.commit(fixture_units("alpha", 1002)).unwrap();
    let envelopes: Vec<_> = committed.iter().flat_map(|u| u.envelopes.clone()).collect();
    Journal::open(journal_path(dir.path()))
        .unwrap()
        .append(&envelopes)
        .unwrap();
    drop(ledger);

    // The export pass rotates those lines away before the next cycle.
    set_rotation(dir.path(), 1, 2);
    assert_eq!(export::backfill(dir.path(), &sink()), envelopes.len());
    assert!(rotated_path(&journal_path(dir.path()), 1).exists());

    let report = run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
    assert!(report.summary.recovered_units > 0);
    // Every identity appears once across the journal and its rotation.
    let all = journal_and_rotations(dir.path());
    assert_eq!(identities(&all).len(), all.len(), "a rotated line was re-journaled");
    assert_eq!(kind_counts_all(&all).0, 6);
}

fn kind_counts_all(all: &[TelemetryEnvelope]) -> (usize, usize) {
    let runs = all
        .iter()
        .filter(|e| matches!(e.record, TelemetryRecord::CiRun(_)))
        .count();
    let jobs = all
        .iter()
        .filter(|e| matches!(e.record, TelemetryRecord::CiJob(_)))
        .count();
    (runs, jobs)
}

// ---------------------------------------------------------------------------
// Export and poll are serialized by `poll.lock`
// ---------------------------------------------------------------------------

#[test]
fn export_skips_while_a_cycle_holds_the_lock() {
    let dir = TempDir::new().unwrap();
    run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
    let total = journal(dir.path()).len();
    let held = state::CycleLock::try_acquire(&state_dir(dir.path()))
        .unwrap()
        .unwrap();
    assert_eq!(export::backfill(dir.path(), &sink()), 0);
    assert_eq!(export::load_cursor(dir.path()), ExportCursor::default());
    drop(held);
    assert_eq!(export::backfill(dir.path(), &sink()), total);
}

/// Runs a poll cycle from inside the export pass, recording its outcome.
struct CycleProbe<'a> {
    root: &'a Path,
    busy: AtomicUsize,
}

impl crate::observability::queue::QueueSink for CycleProbe<'_> {
    fn offer(&self, _envelope: TelemetryEnvelope) {
        if matches!(run_cycle(&ctx(self.root), &FixtureApi::new()), Err(CycleError::Busy)) {
            self.busy.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn offer_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()> {
        self.offer(envelope);
        Ok(())
    }
}

#[test]
fn a_cycle_is_busy_while_the_export_pass_runs() {
    let dir = TempDir::new().unwrap();
    run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
    let probe = CycleProbe {
        root: dir.path(),
        busy: AtomicUsize::new(0),
    };
    let offered = export::backfill(dir.path(), &probe);
    assert!(offered > 0);
    assert_eq!(probe.busy.load(Ordering::SeqCst), offered);
}

#[test]
fn export_of_a_missing_journal_creates_no_state() {
    let dir = TempDir::new().unwrap();
    assert_eq!(export::backfill(dir.path(), &sink()), 0);
    assert!(!state_dir(dir.path()).exists());
}

// ---------------------------------------------------------------------------
// A large journal: open, replay and export without loading it
// ---------------------------------------------------------------------------

/// Build a ~`target` byte journal by repeating real fixture lines, ending
/// with a torn fragment. Returns the complete lines written and one `ci.run`
/// envelope the journal holds.
fn large_journal(root: &Path, target: u64) -> (usize, TelemetryEnvelope) {
    run_cycle(&ctx(root), &FixtureApi::new()).unwrap();
    let sample = journal(root)
        .into_iter()
        .find(|env| matches!(env.record, TelemetryRecord::CiRun(_)))
        .unwrap();
    let path = journal_path(root);
    let template = std::fs::read(&path).unwrap();
    let per_copy = template.iter().filter(|b| **b == b'\n').count();
    let mut out = BufWriter::new(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap(),
    );
    let mut written = template.len() as u64;
    let mut lines = per_copy;
    while written < target {
        out.write_all(&template).unwrap();
        written += template.len() as u64;
        lines += per_copy;
    }
    out.write_all(b"{\"schema_version\":8,\"torn").unwrap();
    out.flush().unwrap();
    (lines, sample)
}

#[test]
fn a_large_journal_opens_replays_and_exports_by_streaming() {
    const TARGET: u64 = 64 * 1024 * 1024;
    let dir = TempDir::new().unwrap();
    let (lines, present) = large_journal(dir.path(), TARGET);
    let path = journal_path(dir.path());
    let torn_len = len(&path);
    assert!(torn_len >= TARGET);

    // Open repairs the tail only.
    Journal::open(path.clone()).unwrap();
    let size = len(&path);
    assert!(size < torn_len && size >= TARGET);

    // Replay: an envelope already in the journal is found; one that is not
    // is reported missing — after a streamed scan of all 64 MiB.
    let mut absent = present.clone();
    if let TelemetryRecord::CiRun(run) = &mut absent.record {
        run.run_id = u64::MAX;
    }
    let missing = Journal::reader(path.clone())
        .missing(&[present, absent.clone()])
        .unwrap();
    assert_eq!(identities(&missing), identities(&[absent]));

    // Export from a cursor near the end reads only the tail.
    let tail_lines = 3;
    let mut tail_start = size;
    {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(&path).unwrap();
        let mut seen = 0;
        let mut byte = [0_u8; 1];
        let mut at = size - 1; // the final '\n'
        while seen < tail_lines {
            at -= 1;
            file.seek(SeekFrom::Start(at)).unwrap();
            file.read_exact(&mut byte).unwrap();
            if byte[0] == b'\n' {
                seen += 1;
                tail_start = at + 1;
            }
        }
    }
    std::fs::create_dir_all(state_dir(dir.path())).unwrap();
    std::fs::write(
        state_dir(dir.path()).join("export-cursor.json"),
        serde_json::to_string(&ExportCursor {
            byte_offset: tail_start,
            exported: 0,
        })
        .unwrap(),
    )
    .unwrap();
    assert_eq!(export::pending_count(dir.path()), tail_lines);
    let counted = CountingSink::default();
    assert_eq!(export::backfill(dir.path(), &counted), tail_lines);

    // A full export from zero streams every line, then rotates.
    set_rotation(dir.path(), TARGET / 2, 1);
    std::fs::remove_file(state_dir(dir.path()).join("export-cursor.json")).unwrap();
    let everything = CountingSink::default();
    assert_eq!(export::backfill(dir.path(), &everything), lines);
    assert!(!path.exists());
    assert_eq!(len(&rotated_path(&path, 1)), size);
}
