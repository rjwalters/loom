use super::*;
use std::sync::Arc;

fn start(journal: &Journal, parent: &TraceContext) -> ActiveSpan {
    journal
        .start(
            parent.child(),
            Some(parent),
            SpanName::RoleAttempt,
            Utc::now(),
            [
                ("loom.role".into(), "judge".into()),
                ("api_key".into(), "PRIVATE_SENTINEL".into()),
            ]
            .into(),
        )
        .unwrap()
}

#[test]
fn restart_preserves_ids_attempts_privacy_and_failed_delivery_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("execution.json");
    let journal = Journal::for_context(&path);
    let root = TraceContext::root(true);
    let first = start(&journal, &root);
    journal
        .finish(
            &first,
            Utc::now(),
            SpanStatus::Error,
            [("loom.judge_verdict".into(), "rejected".into())].into(),
        )
        .unwrap();
    let reopened = Journal::for_context(&path);
    let second = start(&reopened, &root);
    assert_eq!(second.record.attributes["loom.attempt"], "2");
    assert_eq!(reopened.active().unwrap().len(), 1);
    reopened
        .finish(&second, Utc::now(), SpanStatus::Ok, Default::default())
        .unwrap();
    assert!(reopened
        .drain(|_| anyhow::bail!("queue unavailable"))
        .is_err());
    let mut spans = Vec::new();
    assert_eq!(
        reopened
            .drain(|span| {
                spans.push(span);
                Ok(())
            })
            .unwrap(),
        2
    );
    assert_eq!(spans[0].context, first.record.context);
    assert_eq!(spans[1].context, second.record.context);
    assert_eq!(spans[0].status, SpanStatus::Error);
    assert_eq!(spans[1].status, SpanStatus::Ok);
    assert_eq!(
        reopened
            .drain(|_| panic!("cursor must skip delivered records"))
            .unwrap(),
        0
    );
    assert!(!std::fs::read_to_string(reopened.path())
        .unwrap()
        .contains("PRIVATE_SENTINEL"));
}

#[test]
fn concurrent_process_style_writers_use_production_retry_without_lost_completions() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::for_context(&dir.path().join("execution.json")));
    let root = TraceContext::root(true);
    let started = Arc::new(std::sync::Barrier::new(4));
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let journal = journal.clone();
            let parent = root.clone();
            let started = started.clone();
            std::thread::spawn(move || {
                started.wait();
                let active = journal
                    .start(
                        parent.child(),
                        Some(&parent),
                        SpanName::Tool,
                        Utc::now(),
                        Default::default(),
                    )
                    .unwrap();
                journal
                    .finish(&active, Utc::now(), SpanStatus::Ok, Default::default())
                    .unwrap();
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert!(journal.active().unwrap().is_empty());
    let mut ids = std::collections::BTreeSet::new();
    assert_eq!(
        journal
            .drain(|s| {
                assert!(ids.insert(s.context.span_id.as_str().to_owned()));
                Ok(())
            })
            .unwrap(),
        4
    );
}

#[test]
fn slow_queue_acceptance_does_not_hold_the_worker_append_lock() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::for_context(&dir.path().join("execution.json"));
    let root = TraceContext::root(true);
    let initial = start(&journal, &root);
    journal
        .finish(&initial, Utc::now(), SpanStatus::Ok, Default::default())
        .unwrap();
    let mut next = None;
    assert_eq!(
        journal
            .drain(|_| {
                let active = start(&journal, &root);
                journal.finish(&active, Utc::now(), SpanStatus::Ok, Default::default())?;
                next = Some(active.record.context);
                Ok(())
            })
            .unwrap(),
        1
    );
    assert_eq!(
        journal
            .drain(|s| {
                assert_eq!(Some(s.context), next);
                Ok(())
            })
            .unwrap(),
        1
    );
}

#[test]
fn owner_transfer_persists_its_own_time_independent_of_semantic_start() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::for_context(&dir.path().join("execution.json"));
    let semantic_start = Utc::now() - chrono::Duration::hours(6);
    let span = journal
        .start(
            TraceContext::root(true),
            None,
            SpanName::Sweep,
            semantic_start,
            Default::default(),
        )
        .unwrap();
    let before_transfer = Utc::now();
    journal.set_owner(&span.record.context, 42).unwrap();
    let restored = Journal::for_context(&dir.path().join("execution.json"))
        .active()
        .unwrap();
    assert_eq!(restored[0].record.started_at, semantic_start);
    assert_eq!(restored[0].owner_pid, 42);
    assert!(restored[0].owner_observed_at.unwrap() >= before_transfer);
    // Old journals have no owner observation clock; decoding must not invent one.
    let mut legacy = serde_json::to_value(&span).unwrap();
    legacy.as_object_mut().unwrap().remove("owner_observed_at");
    assert!(serde_json::from_value::<ActiveSpan>(legacy)
        .unwrap()
        .owner_observed_at
        .is_none());
}

#[test]
fn held_lock_has_a_bounded_production_retry_budget() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::for_context(&dir.path().join("execution.json"));
    let _held = journal.lock().unwrap();
    let started = std::time::Instant::now();
    assert!(journal
        .start(TraceContext::root(true), None, SpanName::Sweep, Utc::now(), Default::default())
        .is_err());
    assert!(started.elapsed() >= std::time::Duration::from_millis(1000));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "must not wait indefinitely"
    );
}

#[test]
fn incomplete_tail_waits_for_locked_restart_recovery_and_new_completion() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::for_context(&dir.path().join("execution.json"));
    let root = TraceContext::root(true);
    let active = start(&journal, &root);
    journal
        .finish(&active, Utc::now(), SpanStatus::Ok, Default::default())
        .unwrap();
    let complete_bytes = std::fs::read(journal.path()).unwrap();
    OpenOptions::new()
        .append(true)
        .open(journal.path())
        .unwrap()
        .write_all(b"{\"event\":")
        .unwrap();
    assert_eq!(journal.drain(|_| Ok(())).unwrap(), 1);
    assert!(
        std::fs::metadata(journal.path()).unwrap().len() > complete_bytes.len() as u64,
        "drain does not acknowledge a partial record"
    );
    let reopened = Journal::for_context(&dir.path().join("execution.json"));
    let next = start(&reopened, &root);
    reopened
        .finish(&next, Utc::now(), SpanStatus::Error, Default::default())
        .unwrap();
    let recovered = std::fs::read(journal.path()).unwrap();
    assert!(recovered.starts_with(&complete_bytes));
    let mut delivered = Vec::new();
    assert_eq!(
        reopened
            .drain(|s| {
                delivered.push(s);
                Ok(())
            })
            .unwrap(),
        1
    );
    assert_eq!(delivered[0].context, next.record.context);
    assert_eq!(delivered[0].status, SpanStatus::Error);
    assert!(reopened.active().unwrap().is_empty());
}

#[test]
fn crash_between_queue_and_cursor_replays_stable_ids() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::for_context(&dir.path().join("execution.json"));
    let active = start(&journal, &TraceContext::root(true));
    journal
        .finish(&active, Utc::now(), SpanStatus::Ok, Default::default())
        .unwrap();
    let mut first = None;
    journal
        .drain(|s| {
            first = Some(s.context);
            Ok(())
        })
        .unwrap();
    std::fs::remove_file(journal.path().with_extension("cursor")).unwrap();
    journal
        .drain(|s| {
            assert_eq!(Some(s.context), first);
            Ok(())
        })
        .unwrap();
}

#[test]
fn representative_journal_overhead_is_measured() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::for_context(&dir.path().join("execution.json"));
    let root = TraceContext::root(true);
    let began = std::time::Instant::now();
    for _ in 0..100 {
        let active = start(&journal, &root);
        journal
            .finish(&active, Utc::now(), SpanStatus::Ok, Default::default())
            .unwrap();
    }
    eprintln!(
        "100 persisted trace starts/completions: {:?}, {} bytes",
        began.elapsed(),
        std::fs::metadata(journal.path()).unwrap().len()
    );
    assert_eq!(journal.drain(|_| Ok(())).unwrap(), 100);
}

#[test]
fn terminal_context_is_retired_only_after_durable_acceptance_and_all_children_finish() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("root.json");
    std::fs::write(&path, "context").unwrap();
    let journal = Journal::for_context(&path);
    let root = journal
        .start(TraceContext::root(true), None, SpanName::Sweep, Utc::now(), Default::default())
        .unwrap();
    let child = start(&journal, &root.record.context);
    journal.set_owner(&root.record.context, 42).unwrap();
    assert!(journal
        .active()
        .unwrap()
        .iter()
        .any(|s| s.record.context == root.record.context && s.owner_pid == 42));
    journal
        .finish(&root, Utc::now(), SpanStatus::Ok, Default::default())
        .unwrap();
    journal.drain(|_| Ok(())).unwrap();
    assert!(!journal.retire_if_drained().unwrap());
    assert!(path.exists());
    journal
        .finish(&child, Utc::now(), SpanStatus::Ok, Default::default())
        .unwrap();
    assert!(!journal.retire_if_drained().unwrap());
    journal.drain(|_| Ok(())).unwrap();
    assert!(journal.retire_if_drained().unwrap());
    assert!(!path.exists());
    assert!(!journal.path().exists());
}
