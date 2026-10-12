#![allow(clippy::unwrap_used)]
use super::*;
use std::sync::{atomic::{AtomicBool, Ordering}, Mutex};

#[derive(Default)]
struct Queue {
    fail: AtomicBool,
    envelopes: Mutex<Vec<TelemetryEnvelope>>,
}
impl QueueSink for Queue {
    fn offer(&self, envelope: TelemetryEnvelope) { self.envelopes.lock().unwrap().push(envelope) }
    fn offer_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()> {
        if self.fail.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("transport queue unavailable"));
        }
        self.offer(envelope);
        Ok(())
    }
}

#[test]
fn attended_and_daemon_completion_export_once_and_retry_preserves_fact() {
    let dir = tempfile::tempdir().unwrap();
    let body = "<!-- loom:estimate difficulty=1 lines=7 files=1 doctor_risk=0.3 -->";
    let queue = Queue::default();
    queue.fail.store(true, Ordering::Relaxed);
    for (index, runtime) in ["codex", "claude"].into_iter().enumerate() {
        let id = uuid::Uuid::new_v4().to_string();
        let request = Completion { repo: "o/r", issue: 42, model: "curator-model",
            runtime, id: &id, host_id: "authoritative-host",
            at: Utc::now() + chrono::Duration::seconds(index as i64) };
        let envelope = record(dir.path(), body, &request).unwrap().unwrap();
        assert_eq!(existing(dir.path(), &request).unwrap().unwrap(), envelope);
        let TelemetryRecord::IssueEstimate(r) = envelope.record else { panic!("wrong kind") };
        assert_eq!(r.issue, 42);
        assert_eq!(r.repo, "o/r");
        assert_eq!(r.model.as_deref(), Some("curator-model"));
        assert_eq!(r.runtime.as_deref(), Some(runtime));
        assert_eq!(r.estimate.difficulty, 1);
        assert_eq!(r.estimate.lines, 7);
        assert_eq!(r.estimate.files, 1);
        assert_eq!(r.estimate.doctor_risk, 0.3);
        assert_eq!(r.estimated_at, request.at);
    }
    assert_eq!(backfill(dir.path(), &queue), 0);
    queue.fail.store(false, Ordering::Relaxed);
    assert_eq!(backfill(dir.path(), &queue), 2);
    assert_eq!(backfill(dir.path(), &queue), 0);
    let envelopes = queue.envelopes.lock().unwrap();
    assert_eq!(envelopes.len(), 2);
    assert_ne!(envelopes[0].emitted_at, envelopes[1].emitted_at);
}

#[test]
fn unknown_provenance_is_absent_and_never_inferred() {
    let dir = tempfile::tempdir().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let request = Completion { repo: "o/r", issue: 42, model: "unknown",
        runtime: "codex", id: &id, host_id: "h", at: Utc::now() };
    let saved = record(dir.path(), "<!-- loom:estimate difficulty=1 lines=7 files=1 doctor_risk=0 -->",
        &request).unwrap().unwrap();
    let TelemetryRecord::IssueEstimate(r) = saved.record else { panic!("wrong kind") };
    assert_eq!(r.model, None);
    assert_eq!(r.runtime.as_deref(), Some("codex"));
}
