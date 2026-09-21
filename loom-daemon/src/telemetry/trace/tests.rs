#![allow(clippy::unwrap_used)]
use super::store::TraceStore;
use super::*;
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};
use chrono::Utc;

#[test]
fn context_roundtrip_rejects_zero_uppercase_and_future_versions() {
    let context = TraceContext::root(true);
    assert_eq!(TraceContext::parse(&context.traceparent()).unwrap(), context);
    assert_eq!(
        serde_json::from_str::<TraceContext>(&serde_json::to_string(&context).unwrap()).unwrap(),
        context
    );
    for value in [
        "00-00000000000000000000000000000000-0123456789012345-01",
        "00-01234567890123456789012345678901-0000000000000000-01",
        "00-ABCDEF01234567890123456789012345-0123456789012345-01",
        "ff-01234567890123456789012345678901-0123456789012345-01",
        "00-01234567890123456789012345678901-0123456789012345-01-extra",
    ] {
        assert!(TraceContext::parse(value).is_err());
    }
    let child = context.child();
    assert_eq!(child.trace_id, context.trace_id);
    assert_ne!(child.span_id, context.span_id);
    assert!(!TraceContext::root(false).child().sampled());
}

#[test]
fn persistent_context_is_stable_across_reopen_and_distinct_per_execution_repo() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let store = TraceStore::new(a.path());
    let first = store
        .load_or_create(a.path(), "issue-18-attempt-1")
        .unwrap();
    let reopened = TraceStore::new(a.path())
        .load_or_create(a.path(), "issue-18-attempt-1")
        .unwrap();
    assert_eq!(first, reopened);
    let other_repo = TraceStore::new(b.path())
        .load_or_create(b.path(), "issue-18-attempt-1")
        .unwrap();
    let other_attempt = store
        .load_or_create(a.path(), "issue-18-attempt-2")
        .unwrap();
    assert_ne!(first.context.trace_id, other_repo.context.trace_id);
    assert_ne!(first.context.trace_id, other_attempt.context.trace_id);
    assert_ne!(first.identity, other_repo.identity);
    store.complete(a.path(), "issue-18-attempt-2").unwrap();
    assert!(!store.path(a.path(), "issue-18-attempt-2").exists());
    assert_eq!(TraceStore::load(&store.path(a.path(), "issue-18-attempt-1")).unwrap(), first);
}

#[test]
fn corrupt_or_oversize_store_does_not_silently_replace_identity() {
    let dir = tempfile::tempdir().unwrap();
    let store = TraceStore::new(dir.path());
    store.load_or_create(dir.path(), "sweep").unwrap();
    let path = store.path(dir.path(), "sweep");
    std::fs::write(&path, b"broken").unwrap();
    assert!(store.load_or_create(dir.path(), "sweep").is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"broken");
    std::fs::write(&path, vec![b' '; 4097]).unwrap();
    assert!(TraceStore::load(&path).is_err());
}

#[test]
fn concurrent_creators_observe_one_identity() {
    let dir = tempfile::tempdir().unwrap();
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let root = dir.path().to_path_buf();
            std::thread::spawn(move || {
                let store = TraceStore::new(&root);
                for _ in 0..200 {
                    if let Ok(context) = store.load_or_create(&root, "same-execution") {
                        return context;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                panic!("context store never became available");
            })
        })
        .collect();
    let values: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(values.iter().all(|v| v == &values[0]));
}

pub(crate) fn span(context: TraceContext) -> SpanRecord {
    let now = Utc::now();
    SpanRecord {
        context,
        parent_span_id: None,
        name: SpanName::Sweep,
        started_at: now,
        ended_at: now,
        status: SpanStatus::Ok,
        attributes: TraceAttributes::new(),
        events: vec![],
        links: vec![],
    }
}

#[test]
fn completed_child_survives_queue_restart_without_completed_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = TraceContext::root(true);
    let mut child = span(root.child());
    child.parent_span_id = Some(root.span_id);
    child.name = SpanName::RoleAttempt;
    let envelope = TelemetryEnvelope::new("host", TelemetryRecord::Span(child));
    assert_eq!(envelope.schema_version, 3);
    let path = dir.path().join("queue.jsonl");
    let queue = crate::observability::queue::DurableQueue::open(path.clone(), 5);
    queue.push(envelope.clone());
    drop(queue);
    let recovered = crate::observability::queue::DurableQueue::open(path, 5);
    assert_eq!(recovered.peek_batch(5), vec![envelope]);
}

#[test]
fn span_content_is_allowlisted_and_bounded() {
    let mut record = span(TraceContext::root(true));
    record.attributes.insert("loom.runtime".into(), "pi".into());
    record
        .attributes
        .insert("loom.prompt".into(), "SECRET_PROMPT".into());
    record
        .attributes
        .insert("authorization".into(), "SECRET_KEY".into());
    record.events = (0..50)
        .map(|_| SpanEvent {
            name: "retry".into(),
            at: record.started_at,
            attributes: record.attributes.clone(),
        })
        .collect();
    record.events.push(SpanEvent {
        name: "SECRET_TOOL_OUTPUT".into(),
        at: record.started_at,
        attributes: TraceAttributes::new(),
    });
    let bounded = record.bounded();
    assert_eq!(bounded.events.len(), 32);
    let json = serde_json::to_string(&bounded).unwrap();
    assert!(!json.contains("SECRET"));
    assert_eq!(bounded.attributes.get("loom.runtime").unwrap(), "pi");
}

#[test]
fn terminal_context_is_retained_when_durable_queue_offer_fails() {
    let dir = tempfile::tempdir().unwrap();
    let store = TraceStore::new(dir.path());
    let execution = store.load_or_create(dir.path(), "pending").unwrap();
    let blocked = dir.path().join("not-a-directory");
    std::fs::write(&blocked, b"occupied").unwrap();
    let queue = crate::observability::queue::DurableQueue::open(blocked.join("queue.jsonl"), 5);
    let envelope = TelemetryEnvelope::new("host", TelemetryRecord::Span(span(execution.context)));
    assert!(queue.push_durable(envelope.clone()).is_err());
    assert!(store.path(dir.path(), "pending").exists());
    let path = dir.path().join("queue.jsonl");
    let queue = crate::observability::queue::DurableQueue::open(path.clone(), 5);
    queue.push_durable(envelope.clone()).unwrap();
    store.complete(dir.path(), "pending").unwrap();
    assert_eq!(
        crate::observability::queue::DurableQueue::open(path, 5).peek_batch(5),
        vec![envelope]
    );
    assert!(!store.path(dir.path(), "pending").exists());
}
