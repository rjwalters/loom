#![allow(clippy::unwrap_used)]
use super::store::TraceStore;
use super::*;
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};
use chrono::Utc;

#[test]
fn dispatch_disposition_span_name_round_trips_next_to_admission() {
    for name in [SpanName::DispatchAdmission, SpanName::DispatchDisposition] {
        let json = serde_json::to_string(&name).unwrap();
        assert_eq!(json, format!("\"{}\"", name.as_str()));
        let back: SpanName = serde_json::from_str(&json).unwrap();
        assert_eq!(back.as_str(), name.as_str());
    }
    assert_eq!(SpanName::DispatchDisposition.as_str(), "loom.dispatch.disposition");
}

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

// `#[serial]`: the repo key reads the process-global `LOOM_REPO`, which
// other `#[serial]` tests set.
#[test]
#[serial_test::serial]
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
fn every_execution_is_its_own_root_trace() {
    let dir = tempfile::tempdir().unwrap();
    let store = TraceStore::new(dir.path());
    let first = store.load_or_create(dir.path(), "sweep-1").unwrap();
    let retry = store.load_or_create(dir.path(), "sweep-2").unwrap();
    assert_ne!(first.context.trace_id, retry.context.trace_id);
    // Reopening keeps the persisted context rather than re-deriving it.
    assert_eq!(store.load_or_create(dir.path(), "sweep-1").unwrap(), first);
}

#[test]
fn context_persisted_with_a_retired_story_field_still_loads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.json");
    let context = TraceContext::root(true);
    let legacy = serde_json::json!({
        "identity": "legacy",
        "context": context,
        "started_at": Utc::now(),
        // An execution in flight across the upgrade still carries the field.
        "story": TraceContext::root(true),
    });
    std::fs::write(&path, legacy.to_string()).unwrap();
    let saved = TraceStore::load(&path).unwrap();
    assert_eq!(saved.context, context);
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

/// A second open file description holding the store's directory lock, the
/// way a forked child holds it between `fork` and `exec`.
fn hold_store_lock(root: &std::path::Path) -> std::fs::File {
    let dir = root.join(".loom/logs/trace-context");
    std::fs::create_dir_all(&dir).unwrap();
    let held = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(".lock"))
        .unwrap();
    held.try_lock().unwrap();
    held
}

/// #10955: a lock that is released a moment later (an inherited descriptor
/// closing at `exec`) is waited out instead of failing the caller.
#[test]
fn a_briefly_held_store_lock_is_waited_out() {
    let dir = tempfile::tempdir().unwrap();
    let held = hold_store_lock(dir.path());
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(60));
        drop(held);
    });
    let store = TraceStore::new(dir.path());
    store
        .load_or_create(dir.path(), "sweep-issue-10955")
        .unwrap();
    releaser.join().unwrap();
}

/// … and the wait is bounded: a lock that stays held is still an error, soon.
#[test]
fn a_store_lock_that_stays_held_is_reported_busy_within_the_bound() {
    let dir = tempfile::tempdir().unwrap();
    let _held = hold_store_lock(dir.path());
    let store = TraceStore::new(dir.path());
    let started = std::time::Instant::now();
    let error = store
        .load_or_create(dir.path(), "sweep-issue-10955")
        .unwrap_err();
    assert_eq!(error.to_string(), "trace context store busy");
    assert!(started.elapsed() >= std::time::Duration::from_millis(400), "it waited");
    assert!(started.elapsed() < std::time::Duration::from_secs(10), "it gave up");
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

#[test]
fn derived_ids_are_deterministic_keyed_and_valid() {
    let a = TraceContext::derived("execution", &["rjwalters/loom", "sweep-issue-42-1790000000"]);
    assert_eq!(
        a,
        TraceContext::derived("execution", &["rjwalters/loom", "sweep-issue-42-1790000000"])
    );
    assert!(a.sampled());
    assert_eq!(TraceContext::parse(&a.traceparent()).unwrap(), a);
    // Any key part, and the part boundaries themselves, change the identity.
    for other in [
        TraceContext::derived("execution", &["rjwalters/loom", "sweep-issue-42-1790000001"]),
        TraceContext::derived("execution", &["other/loom", "sweep-issue-42-1790000000"]),
        TraceContext::derived("execution", &["rjwalters/loomsweep-issue-42-1790000000"]),
        TraceContext::derived("dispatch", &["rjwalters/loom", "sweep-issue-42-1790000000"]),
    ] {
        assert_ne!(other.trace_id, a.trace_id);
    }

    let at = Utc::now();
    let child = a.child_at("loom.phase", at);
    assert_eq!(child.trace_id, a.trace_id);
    assert_ne!(child.span_id, a.span_id);
    assert_eq!(child, a.child_at("loom.phase", at));
    assert_ne!(child, a.child_at("loom.role.attempt", at));
    assert_ne!(child, a.child_at("loom.phase", at + chrono::Duration::nanoseconds(1)));
}

#[test]
fn execution_root_is_derived_from_repo_and_sweep_id() {
    let dir = tempfile::tempdir().unwrap();
    let store = TraceStore::new(dir.path());
    let saved = store.load_or_create(dir.path(), "sweep-issue-7-1").unwrap();
    assert_eq!(saved.context, TraceStore::root_context(dir.path(), "sweep-issue-7-1"));
    store.complete(dir.path(), "sweep-issue-7-1").unwrap();
    assert_eq!(
        store
            .load_or_create(dir.path(), "sweep-issue-7-1")
            .unwrap()
            .context,
        saved.context,
        "recreating the execution recomputes the same trace"
    );
}

/// #10637: `loom.repo` on a span keyed by [`TraceStore::fallback_repo`] is the
/// same repo spelled as GitHub spells it, and its ASCII lowercase is always
/// exactly the key. A canonical record naming the repo supplies the spelling;
/// with no record, or one naming a different repo (a renamed or transferred
/// remote), the name stays as written. `LOOM_GH_BIN` is the fake forge's
/// (the `loom_config_env` key); the default group guards `LOOM_REPO`.
#[test]
#[serial_test::serial(loom_config_env)]
fn repo_attribute_spells_the_key_as_github_does() {
    repo_attribute_body();
}

#[serial_test::serial]
fn repo_attribute_body() {
    use crate::forge_repo_facts::test_support::{Env, Forge};
    std::env::remove_var("LOOM_REPO");
    let check = |remote: &str, forge_says: &str, facts: &str, expected: &str| {
        let env = Env::new(&[("LOOM_REPO_FACTS", facts)]);
        let url = format!("https://github.com/{remote}.git");
        let root = env.repo("r", &[("origin", url.as_str())]);
        let forge = Forge::new(env.tmp.path(), forge_says);
        std::env::set_var("LOOM_GH_BIN", &forge.gh);
        let attribute = TraceStore::repo_attribute(&root);
        std::env::remove_var("LOOM_GH_BIN");
        assert_eq!(attribute, expected, "origin {remote}, forge {forge_says}, facts {facts}");
        assert_eq!(attribute.to_ascii_lowercase(), TraceStore::fallback_repo(&root));
    };
    check("twoam-fixture/loom-ui", "TwoAM-Fixture/Loom-UI", "1", "TwoAM-Fixture/Loom-UI");
    check("TwoAM-Fixture/Loom-UI", "TwoAM-Fixture/Loom-UI", "0", "TwoAM-Fixture/Loom-UI");
    check("twoam-fixture/loom-ui", "TwoAM-Fixture/Loom-UI", "0", "twoam-fixture/loom-ui");
    check("oldco-fixture/app", "NewCo-Fixture/App", "1", "oldco-fixture/app");
}
