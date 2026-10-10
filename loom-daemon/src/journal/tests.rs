//! Unit tests for the journal core. Every root is a fresh `TempDir`.

use std::cell::Cell;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use serde_json::json;
use tempfile::TempDir;

use super::envelope::{decode, encode, Decoded};
use super::segment::segment_name;
use super::*;

thread_local! {
    static DIR_SYNCS: Cell<usize> = const { Cell::new(0) };
}

pub(super) fn count_dir_sync() {
    DIR_SYNCS.with(|c| c.set(c.get() + 1));
}

fn dir_syncs() -> usize {
    DIR_SYNCS.with(Cell::get)
}

fn small(max_segment_bytes: u64) -> JournalOptions {
    JournalOptions {
        max_segment_bytes,
        lock_retry: Duration::from_millis(50),
        ..JournalOptions::default()
    }
}

fn read_all(stream: &Stream) -> (Vec<ReadRecord>, ReadStats) {
    let mut reader = stream.reader(Position::START);
    let records = reader.by_ref().map(Result::unwrap).collect();
    (records, reader.stats())
}

fn seqs(stream: &Stream) -> Vec<u64> {
    read_all(stream).0.iter().map(|r| r.envelope.seq).collect()
}

fn raw_append(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
}

fn first_segment(stream: &Stream) -> std::path::PathBuf {
    stream.dir().join(segment_name(1))
}

fn v1_line(stream: &str, seq: u64) -> Vec<u8> {
    let envelope = Envelope {
        v: ENVELOPE_MAJOR,
        stream: stream.to_owned(),
        seq,
        kind: "test".into(),
        ts: chrono::Utc::now(),
        writer: WriterIdentity::current(),
        key: None,
        data: json!({}),
    };
    encode(&envelope).unwrap()
}

// 1. Envelope ---------------------------------------------------------------

#[test]
fn envelope_round_trips_with_writer_identity() {
    let tmp = TempDir::new().unwrap();
    let stream = Journal::open(tmp.path()).stream("s").unwrap();
    stream
        .append("thing.made", Some("k1"), json!({"a": 1}))
        .unwrap();
    let (records, stats) = read_all(&stream);
    assert_eq!(records.len(), 1);
    let env = &records[0].envelope;
    assert_eq!(env.v, ENVELOPE_MAJOR);
    assert_eq!(env.stream, "s");
    assert_eq!(env.kind, "thing.made");
    assert_eq!(records[0].key(), Some("k1"));
    assert_eq!(env.data, json!({"a": 1}));
    assert_eq!(env.writer.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(env.writer.commit_full, crate::self_update::BUILT_COMMIT_FULL);
    assert_eq!(
        stats,
        ReadStats {
            records: 1,
            ..ReadStats::default()
        }
    );
}

#[test]
fn unknown_fields_ignored_and_unknown_kinds_pass_through() {
    let line = br#"{"v":1,"stream":"s","seq":3,"kind":"from.the.future","ts":"2026-01-01T00:00:00Z","writer":{"version":"9.9.9","commit_full":"abc"},"data":{},"extra":true}"#;
    match decode(line) {
        Decoded::Record(env) => {
            assert_eq!(env.kind, "from.the.future");
            assert_eq!(env.seq, 3);
        }
        other => panic!("expected a record, got {other:?}"),
    }
}

#[test]
fn unknown_major_is_not_corruption() {
    let line = br#"{"v":2,"seq":7,"whatever":"shape"}"#;
    assert!(matches!(decode(line), Decoded::UnknownMajor { v: 2, seq: Some(7) }));
    assert!(matches!(decode(b"not json"), Decoded::Corrupt));
    assert!(matches!(decode(br#"{"v":1,"seq":1}"#), Decoded::Corrupt));
}

#[test]
fn entry_cap_is_exact() {
    let mut envelope = Envelope {
        v: ENVELOPE_MAJOR,
        stream: "s".into(),
        seq: 1,
        kind: "k".into(),
        ts: "2026-01-01T00:00:00Z".parse().unwrap(),
        writer: WriterIdentity::current(),
        key: None,
        data: json!(""),
    };
    let base = encode(&envelope).unwrap().len();
    envelope.data = json!("x".repeat(MAX_ENTRY_BYTES - base));
    assert_eq!(encode(&envelope).unwrap().len(), MAX_ENTRY_BYTES, "at the cap is accepted");
    envelope.data = json!("x".repeat(MAX_ENTRY_BYTES - base + 1));
    assert!(matches!(
        encode(&envelope),
        Err(JournalError::EntryTooLarge { bytes }) if bytes == MAX_ENTRY_BYTES + 1
    ));
}

#[test]
fn oversize_append_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let stream = Journal::open(tmp.path()).stream("s").unwrap();
    stream.append("k", None, json!(1)).unwrap();
    let before = std::fs::read(first_segment(&stream)).unwrap();
    let err = stream
        .append("k", None, json!("x".repeat(MAX_ENTRY_BYTES)))
        .unwrap_err();
    assert!(matches!(err, JournalError::EntryTooLarge { .. }));
    assert_eq!(std::fs::read(first_segment(&stream)).unwrap(), before);
    assert_eq!(stream.append("k", None, json!(2)).unwrap().seq, 2, "no seq consumed");
}

#[test]
fn names_are_plain_components() {
    let journal = Journal::open("/nonexistent-root");
    for bad in ["", ".", "..", ".hidden", "a/b", "a\\b", &"x".repeat(65)] {
        assert!(journal.stream(bad).is_err(), "{bad:?} must be rejected");
    }
    let stream = journal.stream("ok-name_1.2").unwrap();
    assert!(stream.cursor("../x").is_err());
    assert!(!Path::new("/nonexistent-root").exists(), "open/stream do no I/O");
}

// 2. Monotonic seq ------------------------------------------------------------

#[test]
fn seq_is_monotonic_across_reopen() {
    let tmp = TempDir::new().unwrap();
    for _ in 0..3 {
        Journal::open(tmp.path())
            .stream("s")
            .unwrap()
            .append("k", None, json!(null))
            .unwrap();
    }
    let stream = Journal::open(tmp.path()).stream("s").unwrap();
    assert_eq!(stream.append("k", None, json!(null)).unwrap().seq, 4);
    assert_eq!(seqs(&stream), vec![1, 2, 3, 4]);
}

#[test]
fn seq_resumes_from_tail_even_if_tail_line_is_corrupt() {
    let tmp = TempDir::new().unwrap();
    let stream = Journal::open(tmp.path()).stream("s").unwrap();
    stream.append("k", None, json!(null)).unwrap();
    stream.append("k", None, json!(null)).unwrap();
    raw_append(&first_segment(&stream), b"garbage\n");
    assert_eq!(stream.append("k", None, json!(null)).unwrap().seq, 3);
}

// 3. Torn tail ----------------------------------------------------------------

#[test]
fn reader_stops_at_torn_tail_without_modifying_and_writer_repairs() {
    let tmp = TempDir::new().unwrap();
    let stream = Journal::open(tmp.path()).stream("s").unwrap();
    stream.append("k", None, json!(1)).unwrap();
    stream.append("k", None, json!(2)).unwrap();
    let path = first_segment(&stream);
    raw_append(&path, br#"{"v":1,"stream":"s","seq":3,"ki"#);
    let before = std::fs::read(&path).unwrap();

    let (records, stats) = read_all(&stream);
    assert_eq!(records.len(), 2);
    assert!(stats.torn_tail);
    assert_eq!(std::fs::read(&path).unwrap(), before, "readers never repair");

    let report = verify(tmp.path(), &VerifyOptions::default());
    assert!(report.streams[0].torn_tail);
    assert!(report.ok, "a torn tail alone is reported, not failed: {report:?}");

    assert_eq!(stream.append("k", None, json!(3)).unwrap().seq, 3);
    let (records, stats) = read_all(&stream);
    assert_eq!(records.iter().map(|r| r.envelope.seq).collect::<Vec<_>>(), vec![1, 2, 3]);
    assert!(!stats.torn_tail);
}

// 4. Corrupt middle line --------------------------------------------------------

#[test]
fn corrupt_middle_line_is_skipped_and_counted() {
    let tmp = TempDir::new().unwrap();
    let stream = Journal::open(tmp.path()).stream("s").unwrap();
    stream.append("k", None, json!(1)).unwrap();
    raw_append(&first_segment(&stream), b"{broken json\n");
    stream.append("k", None, json!(2)).unwrap();
    let (records, stats) = read_all(&stream);
    assert_eq!(records.iter().map(|r| r.envelope.seq).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(stats.corrupt_lines, 1);
}

#[test]
fn oversize_line_is_skipped_without_buffering() {
    let tmp = TempDir::new().unwrap();
    let stream = Journal::open(tmp.path()).stream("s").unwrap();
    stream.append("k", None, json!(1)).unwrap();
    let mut huge = vec![b'x'; MAX_ENTRY_BYTES * 3];
    huge.push(b'\n');
    raw_append(&first_segment(&stream), &huge);
    stream.append("k", None, json!(2)).unwrap();
    let (records, stats) = read_all(&stream);
    assert_eq!(records.len(), 2);
    assert_eq!(stats.corrupt_lines, 1);
}

// 5. Durability -------------------------------------------------------------------

#[test]
fn first_segment_creation_syncs_its_directory_once() {
    let tmp = TempDir::new().unwrap();
    let stream = Journal::open(tmp.path()).stream("s").unwrap();
    let start = dir_syncs();
    stream.append("k", None, json!(1)).unwrap();
    let after_first = dir_syncs();
    // Stream directory creation (one) + first record of the segment (one).
    assert_eq!(after_first - start, 2);
    stream.append("k", None, json!(2)).unwrap();
    assert_eq!(dir_syncs(), after_first, "later appends add no directory barrier");
}

#[test]
fn cursor_commit_is_atomic_and_ignores_leftover_temp_files() {
    let tmp = TempDir::new().unwrap();
    let stream = Journal::open(tmp.path()).stream("s").unwrap();
    let cursor = stream.cursor("consumer").unwrap();
    assert_eq!(cursor.load().unwrap(), None);
    let position = Position {
        segment: 1,
        offset: 10,
        seq: 1,
    };
    cursor.commit(position).unwrap();
    assert_eq!(cursor.load().unwrap(), Some(position));
    let dir = cursor.path().parent().unwrap();
    let names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, vec!["consumer.json"], "no temp file left behind");

    std::fs::write(dir.join(".cursor-crashed.tmp"), b"{half").unwrap();
    assert_eq!(cursor.load().unwrap(), Some(position));
    assert_eq!(cursor::all(stream.dir()).unwrap().len(), 1);
}

#[test]
fn cursor_resume_reads_only_unacknowledged_records() {
    let tmp = TempDir::new().unwrap();
    let stream = Journal::open(tmp.path()).stream("s").unwrap();
    for i in 0..3 {
        stream.append("k", None, json!(i)).unwrap();
    }
    let cursor = stream.cursor("c").unwrap();
    let second = read_all(&stream).0[1].next;
    cursor.commit(second).unwrap();
    let rest: Vec<u64> = stream
        .reader(cursor.load().unwrap().unwrap())
        .map(|r| r.unwrap().envelope.seq)
        .collect();
    assert_eq!(rest, vec![3]);
}

// 6. Rotation ----------------------------------------------------------------------

fn fill_segments(stream: &Stream, n: usize) {
    for i in 0..n {
        stream.append("k", None, json!(i)).unwrap();
    }
}

#[test]
fn segments_roll_at_the_bound() {
    let tmp = TempDir::new().unwrap();
    let stream = Journal::with_options(tmp.path(), small(1))
        .stream("s")
        .unwrap();
    fill_segments(&stream, 3);
    let segs = segment::list(stream.dir()).unwrap();
    assert_eq!(segs.iter().map(|s| s.first_seq).collect::<Vec<_>>(), vec![1, 2, 3]);
    assert_eq!(seqs(&stream), vec![1, 2, 3]);
}

#[test]
fn rotation_without_cursors_retains_everything() {
    let tmp = TempDir::new().unwrap();
    let stream = Journal::with_options(tmp.path(), small(1))
        .stream("s")
        .unwrap();
    fill_segments(&stream, 3);
    let outcome = stream.rotate().unwrap();
    assert!(outcome.deleted.is_empty());
    assert_eq!(segment::list(stream.dir()).unwrap().len(), 4, "rolled, kept all");
}

#[test]
fn rotation_keeps_the_segment_of_the_oldest_cursor() {
    let tmp = TempDir::new().unwrap();
    let stream = Journal::with_options(tmp.path(), small(1))
        .stream("s")
        .unwrap();
    fill_segments(&stream, 5);
    let records = read_all(&stream).0;
    // `slow` has acknowledged seq 2 (still sits in segment 2), `fast` seq 4.
    stream
        .cursor("slow")
        .unwrap()
        .commit(records[1].next)
        .unwrap();
    stream
        .cursor("fast")
        .unwrap()
        .commit(records[3].next)
        .unwrap();
    let outcome = stream.rotate().unwrap();
    assert_eq!(outcome.deleted, vec![1]);
    let left: Vec<u64> = segment::list(stream.dir())
        .unwrap()
        .iter()
        .map(|s| s.first_seq)
        .collect();
    assert_eq!(left.first(), Some(&2));
    // `slow` resumes cleanly after rotation.
    let rest: Vec<u64> = stream
        .reader(records[1].next)
        .map(|r| r.unwrap().envelope.seq)
        .collect();
    assert_eq!(rest, vec![3, 4, 5]);
}

#[test]
fn rotation_with_a_corrupt_cursor_retains_everything() {
    let tmp = TempDir::new().unwrap();
    let stream = Journal::with_options(tmp.path(), small(1))
        .stream("s")
        .unwrap();
    fill_segments(&stream, 4);
    let records = read_all(&stream).0;
    stream
        .cursor("good")
        .unwrap()
        .commit(records[3].next)
        .unwrap();
    std::fs::write(stream.dir().join("cursors/bad.json"), b"{not a cursor").unwrap();
    let outcome = stream.rotate().unwrap();
    assert!(outcome.deleted.is_empty());
    assert!(outcome
        .retained_reason
        .unwrap()
        .contains("unreadable cursor"));
    assert_eq!(seqs(&stream), vec![1, 2, 3, 4]);
}

#[test]
fn rotation_never_deletes_the_active_segment() {
    let tmp = TempDir::new().unwrap();
    let stream = Journal::with_options(tmp.path(), small(1))
        .stream("s")
        .unwrap();
    fill_segments(&stream, 2);
    let end = read_all(&stream).0[1].next;
    // A cursor past every segment: everything before the active one goes.
    let far = Position {
        segment: 1000,
        ..end
    };
    stream.cursor("c").unwrap().commit(far).unwrap();
    let outcome = stream.rotate().unwrap();
    assert!(outcome.rolled);
    assert_eq!(outcome.deleted, vec![1, 2]);
    let left: Vec<u64> = segment::list(stream.dir())
        .unwrap()
        .iter()
        .map(|s| s.first_seq)
        .collect();
    assert_eq!(left, vec![3], "the (empty) active segment survives");
    assert_eq!(stream.append("k", None, json!(3)).unwrap().seq, 3, "seq continues");
}

// 7. Natural-key replay ---------------------------------------------------------------

#[test]
fn replay_after_restart_does_not_duplicate_natural_keys() {
    let tmp = TempDir::new().unwrap();
    let first = Journal::open(tmp.path()).stream("s").unwrap();
    assert!(matches!(
        first
            .append_if_new("claim.taken", "issue-7", json!(1))
            .unwrap(),
        AppendOutcome::Appended(Appended { seq: 1, .. })
    ));
    assert_eq!(
        first
            .append_if_new("claim.taken", "issue-7", json!(1))
            .unwrap(),
        AppendOutcome::Duplicate { seq: 1 }
    );
    let reopened = Journal::open(tmp.path()).stream("s").unwrap();
    assert_eq!(
        reopened
            .append_if_new("claim.taken", "issue-7", json!(1))
            .unwrap(),
        AppendOutcome::Duplicate { seq: 1 }
    );
    // Same key, different kind is a different natural key.
    assert!(matches!(
        reopened
            .append_if_new("claim.released", "issue-7", json!(1))
            .unwrap(),
        AppendOutcome::Appended(Appended { seq: 2, .. })
    ));
    assert_eq!(seqs(&reopened), vec![1, 2]);
}

#[test]
fn natural_key_window_is_the_documented_bound() {
    let tmp = TempDir::new().unwrap();
    let options = JournalOptions {
        dedup_window_segments: 2,
        ..small(1)
    };
    let stream = Journal::with_options(tmp.path(), options)
        .stream("s")
        .unwrap();
    stream.append_if_new("k", "old", json!(0)).unwrap();
    fill_segments(&stream, 2);
    // `old` is now three segments back: outside the window, appended again.
    assert!(matches!(
        stream.append_if_new("k", "old", json!(0)).unwrap(),
        AppendOutcome::Appended(_)
    ));
}

// 8. Verify ------------------------------------------------------------------------------

fn write_segment(root: &Path, stream: &str, first_seq: u64, lines: &[Vec<u8>]) {
    let dir = root.join(stream);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(segment_name(first_seq)), lines.concat()).unwrap();
}

#[test]
fn verify_clean_journal_is_green() {
    let tmp = TempDir::new().unwrap();
    let stream = Journal::open(tmp.path()).stream("s").unwrap();
    fill_segments(&stream, 3);
    let report = verify(tmp.path(), &VerifyOptions::default());
    assert!(report.ok, "{report:?}");
    assert_eq!(report.streams[0].records, 3);
    assert_eq!(report.streams[0].last_seq, Some(3));
}

#[test]
fn verify_reports_gap_regression_corruption_and_unknown_major() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write_segment(root, "gap", 1, &[v1_line("gap", 1), v1_line("gap", 3)]);
    write_segment(root, "reg", 1, &[v1_line("reg", 1), v1_line("reg", 2), v1_line("reg", 2)]);
    write_segment(root, "bad", 1, &[v1_line("bad", 1), b"nope\n".to_vec(), v1_line("bad", 2)]);
    write_segment(
        root,
        "skew",
        1,
        &[
            v1_line("skew", 1),
            br#"{"v":2,"seq":2}"#.to_vec(),
            b"\n".to_vec(),
        ],
    );
    let report = verify(root, &VerifyOptions::default());
    assert!(!report.ok);
    let by = |n: &str| {
        report
            .streams
            .iter()
            .find(|s| s.stream == n)
            .unwrap()
            .clone()
    };
    assert_eq!(by("gap").gaps, 1);
    assert_eq!(by("reg").regressions, 1);
    assert!(by("reg").needs_rebuild);
    assert_eq!(by("bad").corrupt_lines, 1);
    assert_eq!(by("bad").gaps, 0);
    assert_eq!(by("skew").unknown_major, 1);
    assert_eq!(by("skew").unknown_majors, vec![2]);
    assert_eq!(by("skew").corrupt_lines, 0);
    for name in ["gap", "reg", "bad", "skew"] {
        assert!(!by(name).is_clean(), "{name} must not be green");
    }
}

#[test]
fn verify_missing_or_empty_root_is_an_error() {
    let tmp = TempDir::new().unwrap();
    let missing = verify(&tmp.path().join("absent"), &VerifyOptions::default());
    assert!(!missing.ok && !missing.errors.is_empty());
    let empty = verify(tmp.path(), &VerifyOptions::default());
    assert!(!empty.ok && !empty.errors.is_empty());
    let unknown = verify(
        tmp.path(),
        &VerifyOptions {
            streams: vec!["nope".into()],
        },
    );
    assert!(!unknown.ok);
    assert!(!unknown.streams[0].errors.is_empty());
}

#[cfg(unix)]
#[test]
fn verify_unreadable_stream_directory_is_not_green() {
    use std::os::unix::fs::PermissionsExt;
    // Root bypasses permission bits; the check is meaningless there.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let stream = Journal::open(tmp.path()).stream("s").unwrap();
    fill_segments(&stream, 1);
    std::fs::set_permissions(stream.dir(), std::fs::Permissions::from_mode(0o000)).unwrap();
    let report = verify(tmp.path(), &VerifyOptions::default());
    std::fs::set_permissions(stream.dir(), std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!report.ok);
    assert!(!report.streams[0].errors.is_empty(), "{report:?}");
}

// 12. Downgrade / schema skew -------------------------------------------------------------

#[test]
fn v1_reader_skips_and_counts_v2_records_without_panicking() {
    let tmp = TempDir::new().unwrap();
    let stream = Journal::open(tmp.path()).stream("s").unwrap();
    stream.append("k", None, json!(1)).unwrap();
    raw_append(
        &first_segment(&stream),
        br#"{"v":2,"stream":"s","seq":2,"shape":{"totally":"new"}}
"#,
    );
    let (records, stats) = read_all(&stream);
    assert_eq!(records.len(), 1);
    assert_eq!(stats.unknown_major, 1);
    assert_eq!(stats.corrupt_lines, 0);
    // A v1 writer continues after the newer writer's seq, never reusing it.
    assert_eq!(stream.append("k", None, json!(3)).unwrap().seq, 3);
}

// Lock -------------------------------------------------------------------------------------

#[test]
fn a_held_lock_makes_append_busy_and_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let stream = Journal::with_options(tmp.path(), small(1 << 20))
        .stream("s")
        .unwrap();
    stream.append("k", None, json!(1)).unwrap();
    let guard = stream.lock().unwrap();
    let started = std::time::Instant::now();
    assert!(matches!(stream.append("k", None, json!(2)), Err(JournalError::Busy)));
    assert!(started.elapsed() < Duration::from_secs(2));
    drop(guard);
    assert_eq!(seqs(&stream), vec![1]);
    assert_eq!(stream.append("k", None, json!(2)).unwrap().seq, 2);
}
