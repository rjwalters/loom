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

/// harness-ops `internal/storyid/testdata/vectors.json`, copied verbatim: the
/// cross-language D32 v1 conformance vectors (#9068).
const D32_VECTORS: &str = include_str!("../../../tests/fixtures/story_vectors_d32_v1.json");

#[test]
fn story_ids_match_every_d32_v1_reference_vector() {
    let fixture: serde_json::Value = serde_json::from_str(D32_VECTORS).unwrap();
    let number = |v: &serde_json::Value| u32::try_from(v["number"].as_u64().unwrap()).unwrap();
    let vectors = fixture["vectors"].as_array().unwrap();
    assert_eq!(vectors.len(), 3);
    for v in vectors {
        let repo_id = v["repo_id"].as_u64().unwrap();
        assert_eq!(story_input(repo_id, number(v)), v["input"].as_str().unwrap());
        let story = story_context(repo_id, number(v)).unwrap();
        assert_eq!(story.trace_id.as_str(), v["trace_id"].as_str().unwrap(), "{}", v["story"]);
        assert_eq!(story.span_id.as_str(), v["root_span"].as_str().unwrap(), "{}", v["story"]);
        // Sampled, W3C random flag unset.
        assert_eq!(story.flags, 1);
    }
    let spans = fixture["span_vectors"].as_array().unwrap();
    assert_eq!(spans.len(), 2);
    for v in spans {
        let span = story_span_id(
            v["repo_id"].as_u64().unwrap(),
            number(v),
            v["kind"].as_str().unwrap(),
            v["source_event_id"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(span.as_str(), v["span_id"].as_str().unwrap(), "{}", v["story"]);
    }
    let kinds: Vec<&str> = fixture["span_kinds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap())
        .collect();
    assert_eq!(kinds, STORY_SPAN_KINDS);
    assert_eq!(fixture["source_event_id_pattern"], "^[A-Za-z0-9._-]{1,256}$");
}

#[test]
fn story_context_is_keyed_on_repo_id_not_name() {
    // rjwalters/loom#9027 (repo_id 1073994527), pinned from D32's table.
    let story = story_context(1_073_994_527, 9027).unwrap();
    assert_eq!(story.trace_id.as_str(), "4e99cc89953fb6bd604a06896b83dc81");
    assert_eq!(story.span_id.as_str(), "9bf80a36c4f1c38f");
    assert!(story.sampled());
    assert_ne!(story_context(1_073_994_527, 9028).unwrap().trace_id, story.trace_id);
    assert_ne!(story_context(1_073_994_528, 9027).unwrap().trace_id, story.trace_id);
    // The pre-#9068 name-derived key (NUL-joined `loom.story.trace`) is gone.
    assert_ne!(
        story.trace_id,
        TraceId::derived(&["loom.story.trace", "rjwalters/loom", "9027"])
    );
}

#[test]
fn story_span_id_refuses_what_d32_refuses() {
    let ok = |kind: &str, event: &str| story_span_id(1, 1, kind, event);
    assert!(ok("story.merge", "a.B_c-9").is_ok());
    assert!(ok("story.merge", &"x".repeat(256)).is_ok());
    for kind in ["loom.story", "ci.run", "story.Merge", "story.merge ", ""] {
        assert_eq!(ok(kind, "1"), Err(StoryIdError::UnknownKind), "{kind:?}");
    }
    let too_long = "x".repeat(257);
    for event in ["", "a:b", "a b", "é", "a/b", too_long.as_str()] {
        assert_eq!(ok("story.merge", event), Err(StoryIdError::InvalidEventId), "{event:?}");
    }
}

#[test]
fn derived_ci_ids_are_unchanged_by_the_shared_derivation() {
    use crate::ci_telemetry::records::{job_context, run_context};
    // Pinned from the pre-#9038 private CI derivation: CI trace ids must not move.
    let run = run_context("2AMLogic/loom", 42, 1);
    assert_eq!(run.trace_id.as_str(), "3468ca8ebf11663d1c7bc17c8cdbbe5a");
    assert_eq!(run.span_id.as_str(), "566ba435fd9bed16");
    let job = job_context("2AMLogic/loom", 42, 1, 7);
    assert_eq!(job.trace_id, run.trace_id);
    assert_eq!(job.span_id.as_str(), "47ab46a6e7148128");
}

#[test]
fn story_executions_share_the_story_trace_with_distinct_roots() {
    let dir = tempfile::tempdir().unwrap();
    let store = TraceStore::new(dir.path());
    let story = story_context(1_073_994_527, 9038).unwrap();
    let first = store
        .load_or_create_story(dir.path(), "sweep-1", Some(&story))
        .unwrap();
    let retry = store
        .load_or_create_story(dir.path(), "sweep-2", Some(&story))
        .unwrap();
    for saved in [&first, &retry] {
        assert_eq!(saved.context.trace_id, story.trace_id);
        assert_ne!(saved.context.span_id, story.span_id);
        assert_eq!(saved.story.as_ref(), Some(&story));
    }
    assert_ne!(first.context.span_id, retry.context.span_id);
    // Reopening keeps the persisted context rather than re-deriving it.
    assert_eq!(store.load_or_create(dir.path(), "sweep-1").unwrap(), first);
    let plain = store.load_or_create(dir.path(), "tick").unwrap();
    assert_ne!(plain.context.trace_id, story.trace_id);
    assert_eq!(plain.story, None);
}

#[test]
fn context_persisted_before_stories_still_loads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.json");
    let context = TraceContext::root(true);
    let legacy = serde_json::json!({
        "identity": "legacy",
        "context": context,
        "started_at": Utc::now(),
    });
    std::fs::write(&path, legacy.to_string()).unwrap();
    let saved = TraceStore::load(&path).unwrap();
    assert_eq!(saved.context, context);
    assert_eq!(saved.story, None);
    assert!(!serde_json::to_string(&saved).unwrap().contains("story"));
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
