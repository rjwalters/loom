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
    assert!(first.iter().all(is_priority_envelope), "only priority lines go first");
    // The bulk cursor has not moved: the span backlog is untouched.
    let cursor = export::load_cursor(dir.path());
    assert_eq!(cursor.byte_offset, 0);
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
    assert!(!export::rotate_if_due(dir.path(), &path, &mut cursor, policy).unwrap());
    assert!(path.exists());
    // Drain to completion; only then does a pass rotate.
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
