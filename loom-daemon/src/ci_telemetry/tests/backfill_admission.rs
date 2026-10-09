//! Bounded exporter-aware backfill admission (#11115).
//!
//! A journal replay larger than an exporter queue must neither evict queued
//! records nor spin: it is admitted up to the queue's backfill limit, then
//! the unadmitted tail stays behind the persisted cursor. Delivery across a
//! crash or a partial fan-out is at-least-once (a duplicate, never a gap).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::super::export;
use super::journal_view::journal;
use super::*;
use crate::observability::queue::{
    backfill_limit, Admission, DurableQueue, FanoutQueue, QueueSink,
};
use crate::observability::ExportStatus;

fn seeded(root: &Path) -> Vec<TelemetryEnvelope> {
    run_cycle(&ctx(root), &FixtureApi::new()).unwrap();
    let all = journal(root);
    assert!(all.len() > 12, "fixture must exceed the small test queues");
    all
}

fn ids(envelopes: &[TelemetryEnvelope]) -> BTreeSet<String> {
    envelopes.iter().filter_map(envelope_identity).collect()
}

fn queue(dir: &Path, name: &str, capacity: usize) -> Arc<DurableQueue> {
    Arc::new(DurableQueue::open(dir.join(name), capacity))
}

/// A healthy sender: acknowledges everything queued, returning it.
fn drain(queue: &DurableQueue) -> Vec<TelemetryEnvelope> {
    let snapshot = queue.peek_snapshot(usize::MAX);
    let count = snapshot.envelopes.len();
    queue.ack_snapshot(&snapshot, count);
    snapshot.envelopes
}

/// Run backfill/drain rounds until the journal is fully exported.
fn replay(
    root: &Path,
    sink: &dyn QueueSink,
    queues: &[Arc<DurableQueue>],
) -> Vec<Vec<TelemetryEnvelope>> {
    let mut delivered = vec![Vec::new(); queues.len()];
    for _ in 0..1000 {
        export::backfill(root, sink);
        for (index, queue) in queues.iter().enumerate() {
            assert!(queue.len() <= backfill_limit(queue.pressure().capacity as usize));
            delivered[index].extend(drain(queue));
        }
        if export::pending_count(root) == 0 {
            return delivered;
        }
    }
    panic!("replay did not converge");
}

#[test]
fn replay_larger_than_capacity_is_fully_delivered_without_eviction() {
    let dir = TempDir::new().unwrap();
    let all = seeded(dir.path());
    let q = queue(dir.path(), "q.jsonl", 8);
    let delivered = replay(dir.path(), q.as_ref(), std::slice::from_ref(&q));
    assert_eq!(delivered[0].len(), all.len(), "exactly once on a clean replay");
    assert_eq!(ids(&delivered[0]), ids(&all));
    assert_eq!(q.dropped_total(), 0, "backfill must never evict");
    assert!(q.deferred_total() > 0, "the replay exceeded capacity, so it deferred");
}

#[test]
fn full_queue_defers_without_advancing_past_the_unadmitted_record() {
    let dir = TempDir::new().unwrap();
    let all = seeded(dir.path());
    let q = queue(dir.path(), "q.jsonl", 8); // backfill limit 6
                                             // A previously queued lifecycle record must survive the replay.
    let lifecycle = all[0].clone();
    q.push(lifecycle.clone());
    let offered = export::backfill(dir.path(), q.as_ref());
    assert_eq!(offered, 5, "limit 6 minus the 1 pre-queued record");
    assert_eq!(q.len(), 6);
    assert_eq!(q.dropped_total(), 0);
    assert_eq!(q.peek_batch(1), vec![lifecycle]);
    assert_eq!(export::pending_count(dir.path()), all.len() - 5);
    assert_eq!(export::load_cursor(dir.path()).exported, 5);
    // A second pass with the sender still stuck is a cheap no-op, not a spin.
    assert_eq!(export::backfill(dir.path(), q.as_ref()), 0);
    assert_eq!(q.len(), 6);
    assert!(q.deferred_total() >= 2);
    // Ordinary producers keep the 25% headroom and still evict nothing.
    q.push(all[1].clone());
    q.push(all[2].clone());
    assert_eq!((q.len(), q.dropped_total()), (8, 0));
    // Ordinary overflow keeps its drop-oldest contract.
    q.push(all[3].clone());
    assert_eq!((q.len(), q.dropped_total()), (8, 1));
}

#[test]
fn unequal_fanout_capacities_never_omit_for_the_larger_queue() {
    let dir = TempDir::new().unwrap();
    let all = seeded(dir.path());
    let small = queue(dir.path(), "small.jsonl", 4); // limit 3
    let large = queue(dir.path(), "large.jsonl", 400);
    let fanout = FanoutQueue::new(vec![small.clone(), large.clone()]);
    // First pass: only the small queue's limit is admitted by both.
    assert_eq!(export::backfill(dir.path(), &fanout), 3);
    assert_eq!((small.len(), large.len()), (3, 3));
    assert_eq!(small.pressure().backfill_deferred_total, 1);
    assert_eq!(large.pressure().backfill_deferred_total, 0, "pressure is per exporter");
    let delivered = replay(dir.path(), &fanout, &[small.clone(), large.clone()]);
    // The pass above left 3 queued in each; add them back.
    let mut first = ids(&all[..3]);
    first.extend(ids(&delivered[0]));
    assert_eq!(first, ids(&all));
    assert_eq!(small.dropped_total() + large.dropped_total(), 0);
}

/// A sink whose backend is unreachable: every offer errors.
struct DownSink;

impl QueueSink for DownSink {
    fn offer(&self, _envelope: TelemetryEnvelope) {}
    fn offer_durable(&self, _envelope: TelemetryEnvelope) -> std::io::Result<()> {
        Err(std::io::Error::other("sink down"))
    }
}

#[test]
fn a_failed_sink_leaves_every_record_retryable_behind_the_cursor() {
    let dir = TempDir::new().unwrap();
    let all = seeded(dir.path());
    assert_eq!(export::backfill(dir.path(), &DownSink), 0);
    assert_eq!(export::load_cursor(dir.path()).byte_offset, 0);
    assert_eq!(export::pending_count(dir.path()), all.len());
    // The sink recovers: everything is delivered.
    let q = queue(dir.path(), "q.jsonl", 8);
    let delivered = replay(dir.path(), q.as_ref(), std::slice::from_ref(&q));
    assert_eq!(ids(&delivered[0]), ids(&all));
}

#[test]
fn restart_resumes_from_the_cursor_and_keeps_the_queued_backlog() {
    let dir = TempDir::new().unwrap();
    let all = seeded(dir.path());
    let first = queue(dir.path(), "q.jsonl", 8);
    assert_eq!(export::backfill(dir.path(), first.as_ref()), 6);
    drop(first); // daemon restart with the sender never having drained
    let reopened = queue(dir.path(), "q.jsonl", 8);
    assert_eq!(reopened.len(), 6, "the offline queue survived the restart");
    let mut delivered = replay(dir.path(), reopened.as_ref(), std::slice::from_ref(&reopened));
    let delivered = delivered.remove(0);
    assert_eq!(delivered.len(), all.len(), "no duplicate across a clean restart");
    assert_eq!(ids(&delivered), ids(&all));
}

#[test]
fn a_crash_between_offer_and_cursor_save_redelivers_at_least_once() {
    let dir = TempDir::new().unwrap();
    let all = seeded(dir.path());
    let q = queue(dir.path(), "q.jsonl", 8);
    export::backfill(dir.path(), q.as_ref());
    let cursor_file = dir.path().join(".loom/ci-telemetry/export-cursor.json");
    // Simulate the crash: the queue holds the records but the cursor is lost.
    let cursor_file = if cursor_file.exists() {
        cursor_file
    } else {
        super::super::state_dir(dir.path()).join("export-cursor.json")
    };
    std::fs::remove_file(cursor_file).unwrap();
    let mut delivered = replay(dir.path(), q.as_ref(), std::slice::from_ref(&q));
    let delivered = delivered.remove(0);
    assert_eq!(ids(&delivered), ids(&all), "no gap");
    assert!(delivered.len() > all.len(), "the re-offered prefix is duplicated");
}

#[test]
fn concurrent_producer_and_sender_acks_lose_nothing() {
    let dir = TempDir::new().unwrap();
    let all = seeded(dir.path());
    let q = queue(dir.path(), "q.jsonl", 40); // limit 30, headroom 10
    let stop = Arc::new(AtomicBool::new(false));
    let delivered = Arc::new(Mutex::new(Vec::new()));
    let sender = {
        let (q, stop, delivered) = (q.clone(), stop.clone(), delivered.clone());
        std::thread::spawn(move || loop {
            let batch = drain(&q);
            delivered.lock().unwrap().extend(batch);
            if stop.load(Ordering::SeqCst) && q.is_empty() {
                return;
            }
            std::thread::yield_now();
        })
    };
    let ordinary: Vec<_> = all.iter().take(10).cloned().collect();
    let producer = {
        let q = q.clone();
        std::thread::spawn(move || {
            for envelope in ordinary {
                q.push(envelope);
                std::thread::yield_now();
            }
        })
    };
    for _ in 0..1000 {
        export::backfill(dir.path(), q.as_ref());
        if export::pending_count(dir.path()) == 0 {
            break;
        }
        std::thread::yield_now();
    }
    producer.join().unwrap();
    stop.store(true, Ordering::SeqCst);
    sender.join().unwrap();
    assert_eq!(q.dropped_total(), 0);
    assert_eq!(ids(&delivered.lock().unwrap()), ids(&all));
}

#[test]
fn partial_transport_acks_keep_the_unacked_suffix_and_lose_nothing() {
    let dir = TempDir::new().unwrap();
    let all = seeded(dir.path());
    let q = queue(dir.path(), "q.jsonl", 8);
    let mut delivered = Vec::new();
    for _ in 0..1000 {
        export::backfill(dir.path(), q.as_ref());
        // The transport accepts only half of each batch.
        let snapshot = q.peek_snapshot(usize::MAX);
        let half = snapshot.envelopes.len().div_ceil(2);
        q.ack_snapshot(&snapshot, half);
        delivered.extend(snapshot.envelopes[..half].iter().cloned());
        if export::pending_count(dir.path()) == 0 {
            delivered.extend(drain(&q));
            break;
        }
    }
    assert_eq!(delivered.len(), all.len());
    assert_eq!(ids(&delivered), ids(&all));
    assert_eq!(q.dropped_total(), 0);
}

#[test]
fn pressure_counters_separate_deferral_from_eviction_per_exporter() {
    let dir = TempDir::new().unwrap();
    let all = seeded(dir.path());
    let q = queue(dir.path(), "q.jsonl", 4);
    let status = ExportStatus::started("host", "https://example.invalid", "https", 30);
    assert!(status.snapshot().queue.is_none(), "unattached until startup wires it");
    status.attach_queue(q.clone());
    export::backfill(dir.path(), q.as_ref());
    export::backfill(dir.path(), q.as_ref());
    let pressure = status.snapshot().queue.unwrap();
    assert_eq!((pressure.depth, pressure.capacity, pressure.backfill_limit), (3, 4, 3));
    assert_eq!(pressure.dropped_total, 0);
    assert_eq!(pressure.backfill_deferred_total, 2);
    q.push(all[0].clone());
    q.push(all[1].clone());
    let pressure = status.snapshot().queue.unwrap();
    assert_eq!(pressure.dropped_total, 1, "ordinary overflow is real eviction");
    assert_eq!(pressure.backfill_deferred_total, 2);
    assert_eq!(q.offer_backfill(all[2].clone()).unwrap(), Admission::Deferred);
}
