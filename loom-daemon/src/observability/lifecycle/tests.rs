#![allow(clippy::unwrap_used)]
use super::*;

#[test]
fn completion_markers_preserve_each_repair_attempt_without_invented_duration() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::for_context(&dir.path().join("root.json"));
    let root = TraceContext::root(true);
    for (phase, attempt) in [
        ("builder-done", 1),
        ("judge-rejected", 1),
        ("doctor-done", 2),
        ("judge-done", 2),
        ("merge-done", 2),
    ] {
        checkpoint_observation(
            &journal,
            &root,
            None,
            18,
            phase,
            Some(attempt),
            Some("glm-5.3-flash"),
            Some(42),
            "checkpoint_write_observed",
        );
    }
    assert!(journal.has_checkpoint_observations().unwrap());
    let mut spans = Vec::new();
    assert_eq!(
        journal
            .drain(|s| {
                spans.push(s);
                Ok(())
            })
            .unwrap(),
        10
    );
    let judges: Vec<_> = spans
        .iter()
        .filter(|s| s.name == SpanName::RoleAttempt && s.attributes["loom.role"] == "judge")
        .collect();
    assert_eq!(judges.len(), 2);
    assert_eq!(judges[0].attributes["loom.judge_verdict"], "rejected");
    assert_eq!(judges[0].status, SpanStatus::Error);
    assert_eq!(judges[1].attributes["loom.judge_verdict"], "approved");
    assert_eq!(judges[1].status, SpanStatus::Ok);
    assert_ne!(judges[0].context.span_id, judges[1].context.span_id);
    for span in &spans {
        assert_eq!(span.started_at, span.ended_at);
        assert_eq!(span.context.trace_id, root.trace_id);
        assert_eq!(span.attributes["loom.pr_number"], "42");
    }
    assert_eq!(judges[0].attributes["loom.attempt"], "1");
    assert_eq!(judges[1].attributes["loom.attempt"], "2");
}

#[test]
fn checkpoint_completes_existing_started_attempt_without_double_counting() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::for_context(&dir.path().join("root.json"));
    let root = TraceContext::root(true);
    let attrs = attributes(&[("loom.role", "judge")]);
    let start = Utc::now() - chrono::Duration::seconds(10);
    let phase = journal
        .start(root.child(), Some(&root), SpanName::Phase, start, attrs.clone())
        .unwrap();
    let attempt = journal
        .start(
            phase.record.context.child(),
            Some(&phase.record.context),
            SpanName::RoleAttempt,
            start,
            attrs,
        )
        .unwrap();
    checkpoint_observation(
        &journal,
        &root,
        None,
        18,
        "judge-rejected",
        Some(1),
        None,
        Some(42),
        "checkpoint_write_observed",
    );
    let mut spans = Vec::new();
    assert_eq!(
        journal
            .drain(|s| {
                spans.push(s);
                Ok(())
            })
            .unwrap(),
        2
    );
    let observed = spans
        .iter()
        .find(|s| s.name == SpanName::RoleAttempt)
        .unwrap();
    assert_eq!(observed.context, attempt.record.context);
    assert_eq!(observed.started_at, start);
    assert!(observed.ended_at > start);
    assert_eq!(observed.status, SpanStatus::Error);
    assert_eq!(observed.attributes["loom.timing_source"], "owned_start_checkpoint_completion");
}

#[cfg(unix)]
#[test]
fn restart_closes_only_provably_gone_processes_with_unknown_execution_result() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::for_context(&dir.path().join("root.json"));
    let context = TraceContext::root(true);
    let span = journal
        .start(context.clone(), None, SpanName::RoleAttempt, Utc::now(), Default::default())
        .unwrap();
    recover_orphans(&journal);
    assert_eq!(journal.active().unwrap().len(), 1, "live owner is retained");
    let mut child = Command::new("/usr/bin/true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    assert!(process_gone(pid));
    journal.set_owner(&context, pid).unwrap();
    journal.set_supervisor(&context, pid).unwrap();
    recover_orphans(&journal);
    assert!(journal.active().unwrap().is_empty());
    journal
        .drain(|s| {
            assert_eq!(s.context, span.record.context);
            assert_eq!(s.status, SpanStatus::Unset);
            assert_eq!(s.attributes["loom.result"], "process_lost");
            assert_eq!(s.attributes["loom.recovered"], "true");
            Ok(())
        })
        .unwrap();
    assert!(!process_gone(0), "container/unknown PID namespaces cannot be guessed");
}

#[cfg(target_os = "linux")]
#[test]
fn recycled_owner_is_recovered_without_mistaking_delayed_spawn_for_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::for_context(&dir.path().join("root.json"));
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
    journal
        .set_owner(&span.record.context, std::process::id())
        .unwrap();
    recover_orphans(&journal);
    assert_eq!(journal.active().unwrap().len(), 1, "a delayed spawn is still its real owner");
    // Replay an old journal whose former owner's PID now belongs to this much
    // newer process. The persisted owner observation, not span time, proves reuse.
    let mut records: Vec<serde_json::Value> = std::fs::read_to_string(journal.path())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for record in &mut records {
        if record["event"] == "Owner" {
            record["span"]["observed_at"] = serde_json::json!(semantic_start);
        } else if record["event"] == "Started" {
            record["span"]["supervisor_observed_at"] = serde_json::json!(semantic_start);
        }
    }
    std::fs::write(journal.path(), records.iter().map(|r| format!("{r}\n")).collect::<String>())
        .unwrap();
    recover_orphans(&journal);
    assert!(journal.active().unwrap().is_empty());
    let mut results = Vec::new();
    journal
        .drain(|record| {
            results.push(record);
            Ok(())
        })
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].attributes["loom.result"], "process_lost");
    assert_eq!(results[0].status, SpanStatus::Unset);
}

/// A role tick's trace is keyed on role + start instant (+ the repo key), never
/// a random id: the same tick recomputes to the same root, and two roles
/// starting in the same instant do not collide.
#[test]
#[serial_test::serial] // the repo key reads the process-global `LOOM_REPO`
fn role_invocation_ids_derive_from_role_and_start_instant() {
    let at = chrono::DateTime::parse_from_rfc3339("2026-09-26T12:00:00.123456789Z")
        .unwrap()
        .with_timezone(&Utc);
    let judge = role_execution_id("judge", at);
    assert_eq!(judge, "role-judge-2026-09-26T12:00:00.123456789Z");
    assert_eq!(judge, role_execution_id("judge", at), "recomputable");
    let curator = role_execution_id("curator", at);
    let ws = std::path::Path::new("/nonexistent/loom");
    let (a, b) = (
        TraceStore::root_context(ws, &judge, None),
        TraceStore::root_context(ws, &curator, None),
    );
    assert_eq!(a, TraceStore::root_context(ws, &judge, None));
    assert_ne!(a.trace_id, b.trace_id);
    assert_ne!(a.span_id, b.span_id);
}

/// Tool spans carry no role, so the tool name joins the child key: two tools
/// opening the same span name in one clock tick stay distinct.
#[test]
fn tool_spans_in_the_same_instant_key_on_tool_name() {
    let parent = TraceContext::derived("test", &["parent"]);
    let at = Utc::now();
    let read =
        child_context(&parent, SpanName::Tool, at, &attributes(&[("loom.tool.name", "read")]));
    let write =
        child_context(&parent, SpanName::Tool, at, &attributes(&[("loom.tool.name", "write")]));
    assert_ne!(read.span_id, write.span_id);
    assert_eq!(read.trace_id, write.trace_id);
    let untagged = child_context(&parent, SpanName::Tool, at, &TraceAttributes::new());
    assert_eq!(
        untagged,
        parent.derived_child(&[
            SpanName::Tool.as_str(),
            "",
            &crate::telemetry::trace::instant(at)
        ]),
        "spans without a tool name keep their original key"
    );
}
