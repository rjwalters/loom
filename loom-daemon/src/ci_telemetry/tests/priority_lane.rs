//! Priority export lane for `ci.run` / `ci.job` facts (#12792).
//!
//! The journal is dominated by per-test/step/suite spans, so a strict FIFO
//! cursor left fresh run/job facts queued behind millions of older spans. The
//! export pass now offers run/job-class lines first through a second cursor,
//! without dropping or sampling anything.

use std::sync::Arc;

use super::super::export::{self, is_priority, ExportCursor};
use super::super::rotation::RotationPolicy;
use super::journal_view::journal;
use super::*;
use crate::observability::queue::DurableQueue;

/// Rewrite the journal as every bulk (span/log) line first, then every
/// priority line — the shape a long span backlog followed by a fresh poll
/// produces. Returns `(priority, bulk)` envelope counts.
fn reorder_bulk_first(root: &Path) -> (usize, usize) {
    let all = journal(root);
    let (priority, bulk): (Vec<_>, Vec<_>) = all
        .iter()
        .partition(|env| is_priority(serde_json::to_string(env).unwrap().as_bytes()));
    let mut text = String::new();
    for env in bulk.iter().chain(priority.iter()) {
        text.push_str(&serde_json::to_string(env).unwrap());
        text.push('\n');
    }
    std::fs::write(journal_path(root), text).unwrap();
    (priority.len(), bulk.len())
}

fn seeded_reordered(root: &Path) -> (usize, usize) {
    run_cycle(&ctx(root), &FixtureApi::new()).unwrap();
    reorder_bulk_first(root)
}

fn queue(dir: &Path, capacity: usize) -> Arc<DurableQueue> {
    Arc::new(DurableQueue::open(dir.join("q.jsonl"), capacity))
}

fn drain(queue: &DurableQueue) -> Vec<TelemetryEnvelope> {
    let snapshot = queue.peek_snapshot(usize::MAX);
    queue.ack_snapshot(&snapshot, snapshot.envelopes.len());
    snapshot.envelopes
}

fn is_priority_envelope(env: &TelemetryEnvelope) -> bool {
    is_priority(serde_json::to_string(env).unwrap().as_bytes())
}

#[test]
fn classification_covers_run_job_duration_and_their_spans_only() {
    let dir = TempDir::new().unwrap();
    run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
    let mut seen_priority_kinds = BTreeSet::new();
    for env in journal(dir.path()) {
        let line = serde_json::to_string(&env).unwrap();
        let expected = match &env.record {
            TelemetryRecord::CiRun(_)
            | TelemetryRecord::CiJob(_)
            | TelemetryRecord::CiDuration(_) => true,
            TelemetryRecord::Span(span) => matches!(
                span.name,
                crate::telemetry::trace::SpanName::CiRun | crate::telemetry::trace::SpanName::CiJob
            ),
            _ => false,
        };
        assert_eq!(is_priority(line.as_bytes()), expected, "{line}");
        if expected {
            seen_priority_kinds.insert(line.contains("\"trace.span\""));
        }
    }
    assert_eq!(seen_priority_kinds.len(), 2, "fixture has both log and span priority lines");
    assert!(!is_priority(b"not json"));
}

#[test]
fn fresh_run_job_lines_are_offered_before_any_older_span() {
    let dir = TempDir::new().unwrap();
    let (priority, bulk) = seeded_reordered(dir.path());
    assert!(priority > 6 && bulk > 6, "fixture must exceed the small queue");
    let q = queue(dir.path(), 8); // backfill limit 6
    assert_eq!(export::backfill(dir.path(), q.as_ref()), 6);
    let first = drain(&q);
    // The fast lane takes the larger half of the headroom (#13688) and goes
    // first; the rest of the pass is the bulk lane's reserved share.
    assert!(first.iter().take(3).all(is_priority_envelope), "priority lines go first");
    assert_eq!(first.iter().filter(|env| is_priority_envelope(env)).count(), 3);
    let cursor = export::load_cursor(dir.path());
    assert!(cursor.byte_offset > 0, "the bulk lane got its share");
    assert!(cursor.fast_offset.is_some_and(|offset| offset > 0));
    assert_eq!(cursor.exported, 6);
    assert_eq!(export::pending_count(dir.path()), priority + bulk - 6);
}

#[test]
fn every_line_is_eventually_offered_and_the_journal_then_rotates() {
    let dir = TempDir::new().unwrap();
    let (priority, bulk) = seeded_reordered(dir.path());
    let q = queue(dir.path(), 8);
    let mut delivered = Vec::new();
    for _ in 0..1000 {
        export::backfill(dir.path(), q.as_ref());
        delivered.extend(drain(&q));
        if export::pending_count(dir.path()) == 0 {
            break;
        }
    }
    assert_eq!(export::pending_count(dir.path()), 0);
    assert_eq!(delivered.len(), priority + bulk, "each line exactly once on a clean run");
    let ids: BTreeSet<_> = delivered.iter().filter_map(envelope_identity).collect();
    let expected: BTreeSet<_> = journal(dir.path())
        .iter()
        .filter_map(envelope_identity)
        .collect();
    assert_eq!(ids, expected);
    let cursor = export::load_cursor(dir.path());
    assert_eq!(cursor.fast_offset, None, "lanes coincide at EOF");
    assert_eq!(cursor.byte_offset, std::fs::metadata(journal_path(dir.path())).unwrap().len());
}

#[test]
fn rotation_waits_for_the_fast_lane_and_the_bulk_lane() {
    let dir = TempDir::new().unwrap();
    seeded_reordered(dir.path());
    let q = queue(dir.path(), 8);
    let policy = RotationPolicy {
        threshold_bytes: 0,
        keep: 1,
    };
    let path = journal_path(dir.path());
    export::backfill(dir.path(), q.as_ref());
    let mut cursor = export::load_cursor(dir.path());
    // The bulk cursor has moved (#13688), so the pass may rotate and carry the
    // unexported tail; nothing is lost either way.
    export::rotate_if_due(dir.path(), &path, &mut cursor, policy).unwrap();
    // Drain to completion.
    for _ in 0..1000 {
        drain(&q);
        export::backfill(dir.path(), q.as_ref());
        if !path.exists() {
            break;
        }
    }
    assert_eq!(export::pending_count(dir.path()), 0);
}

#[test]
fn a_pre_priority_lane_cursor_file_starts_the_fast_lane_at_the_bulk_cursor() {
    let dir = TempDir::new().unwrap();
    let (priority, bulk) = seeded_reordered(dir.path());
    // An old-format cursor (no `fast_offset`) one third of the way in.
    let text = std::fs::read_to_string(journal_path(dir.path())).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let skip = lines.len() / 3;
    let offset: usize = lines[..skip].iter().map(|l| l.len() + 1).sum();
    std::fs::create_dir_all(state_dir(dir.path())).unwrap();
    std::fs::write(
        state_dir(dir.path()).join("export-cursor.json"),
        format!(r#"{{"byte_offset":{offset},"exported":{skip}}}"#),
    )
    .unwrap();
    let loaded = export::load_cursor(dir.path());
    assert_eq!(
        loaded,
        ExportCursor {
            byte_offset: offset as u64,
            exported: skip as u64,
            fast_offset: None
        }
    );
    assert_eq!(export::pending_count(dir.path()), priority + bulk - skip);
}

/// Refuses every priority (run/job) record forever, accepts the rest: the
/// fast lane can never pass its first priority line (#13685).
struct PriorityRefusingSink {
    taken: std::sync::Mutex<Vec<TelemetryEnvelope>>,
}

impl crate::observability::queue::QueueSink for PriorityRefusingSink {
    fn offer(&self, envelope: TelemetryEnvelope) {
        self.taken.lock().unwrap().push(envelope);
    }
    fn offer_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()> {
        if is_priority_envelope(&envelope) {
            return Err(std::io::Error::other("refused"));
        }
        self.offer(envelope);
        Ok(())
    }
}

#[test]
fn a_stuck_fast_lane_cannot_starve_the_bulk_lane_or_rotation() {
    let dir = TempDir::new().unwrap();
    let (_, bulk) = seeded_reordered(dir.path()); // bulk lines first, priority last
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    std::fs::write(
        dir.path().join(".loom/config.json"),
        r#"{"autonomous":{"ciTelemetry":{"journalRotateBytes":1,"journalRotateKeep":2}}}"#,
    )
    .unwrap();
    let path = journal_path(dir.path());
    let sink = PriorityRefusingSink {
        taken: std::sync::Mutex::new(Vec::new()),
    };

    // One pass: the fast lane stops at the first (refused) priority line, yet
    // the bulk lane still consumes every bulk line before it.
    export::backfill(dir.path(), &sink);
    assert_eq!(sink.taken.lock().unwrap().len(), bulk, "all bulk lines offered");
    // Past the threshold, so the pass rotated, carrying the unexported tail.
    assert!(
        super::super::rotation::rotated_path(&path, 1).exists(),
        "rotation fired although the fast lane never reached EOF"
    );
    let cursor = export::load_cursor(dir.path());
    assert_eq!(cursor.byte_offset, 0, "cursor reset for the new generation");
    assert_eq!(cursor.exported, bulk as u64);
}

#[test]
fn a_sustained_fast_lane_backlog_still_lets_the_bulk_lane_advance_each_pass() {
    // #13688: priority backlog alone exceeds the backfill limit, and the queue
    // drains between passes (as the exporter does). Previously the fast lane
    // refilled the queue to its limit each pass and the bulk cursor stayed 0.
    let dir = TempDir::new().unwrap();
    let (priority, bulk) = seeded_reordered(dir.path());
    assert!(priority > 6 && bulk > 6, "fixture must exceed the small queue");
    let q = queue(dir.path(), 8); // backfill limit 6, headroom 6 per pass
    let mut last = 0;
    for pass in 0..2 {
        export::backfill(dir.path(), q.as_ref());
        let cursor = export::load_cursor(dir.path());
        assert!(cursor.byte_offset > last, "pass {pass}: bulk cursor must advance");
        last = cursor.byte_offset;
        drain(&q);
    }
}
