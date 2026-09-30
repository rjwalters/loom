//! `loom-daemon usage-record` (Issue #9303).

use chrono::{Duration, Utc};

use super::*;
use crate::observability::runtime_usage::{journal_usage, TokenUsage};
use crate::script_helpers::transcript_usage::merge_rows;
use crate::telemetry::trace::story_context;

const REPO_ID: u64 = 4242;

fn story(issue: u32) -> StoryRef {
    StoryRef {
        repo: "owner/repo".into(),
        issue,
        story: format!("owner/repo#{issue}"),
        context: story_context(REPO_ID, issue).unwrap(),
    }
}

/// A subagent transcript: `models` each answer one streamed (twice-written)
/// message.
fn transcript(dir: &Path, name: &str, models: &[(&str, i64)]) -> PathBuf {
    let mut lines = Vec::new();
    for (i, (model, n)) in models.iter().enumerate() {
        for _chunk in 0..2 {
            lines.push(
                serde_json::json!({"type": "assistant",
                    "timestamp": format!("2026-09-28T02:0{i}:00Z"),
                    "message": {"model": model, "id": format!("msg_{name}_{i}"),
                        "usage": {"input_tokens": n, "output_tokens": n * 2,
                            "cache_read_input_tokens": n * 10,
                            "cache_creation": {"ephemeral_5m_input_tokens": n,
                                               "ephemeral_1h_input_tokens": n * 3}}}})
                .to_string(),
            );
        }
    }
    let path = dir.join(format!("agent-{name}.jsonl"));
    std::fs::write(&path, lines.join("\n")).unwrap();
    path
}

fn request(role: &str, attempt: Option<u32>, agent: &str) -> Request {
    Request {
        issue: 9303,
        role: role.into(),
        attempt,
        agent_id: Some(agent.into()),
        task_id: Some("sweep-20260928T020000Z-1-abc".into()),
        transcript: None,
    }
}

fn journalled(workspace: &Path, execution: &str) -> Vec<SpanRecord> {
    let store = TraceStore::new(workspace);
    Journal::for_context(&store.path(workspace, execution))
        .completed()
        .unwrap()
}

fn total(spans: &[SpanRecord]) -> i64 {
    spans
        .iter()
        .filter(|s| s.name == SpanName::RuntimeUsage)
        .map(|s| s.attributes["loom.tokens.total"].parse::<i64>().unwrap())
        .sum()
}

#[test]
fn an_in_session_record_journals_an_attempt_and_per_model_usage_into_the_story_trace() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let path = transcript(ws, "a1", &[("claude-opus-5", 100), ("claude-haiku-4-5", 10)]);
    let req = request("builder", Some(1), "a1");
    let outcome =
        record_transcript(ws, &req, &path, Parent::Story(story(9303)), &Pricing::with(None));
    assert_eq!(outcome, Outcome::Recorded(3));

    let spans = journalled(ws, "insession-sweep-20260928T020000Z-1-abc");
    let story_root = story_context(REPO_ID, 9303).unwrap();
    let attempt = spans
        .iter()
        .find(|s| s.name == SpanName::RoleAttempt)
        .unwrap();
    assert_eq!(attempt.context.trace_id, story_root.trace_id, "the story's trace");
    assert_eq!(attempt.parent_span_id.as_ref(), Some(&story_root.span_id));
    assert_eq!(attempt.attributes["loom.role"], "builder");
    assert_eq!(attempt.attributes["loom.story"], "owner/repo#9303");
    assert_eq!(attempt.attributes["loom.sweep_id"], "sweep-20260928T020000Z-1-abc");
    assert_eq!(attempt.started_at.to_rfc3339(), "2026-09-28T02:00:00+00:00");
    assert_eq!(attempt.ended_at.to_rfc3339(), "2026-09-28T02:01:00+00:00");

    let usage: Vec<_> = spans
        .iter()
        .filter(|s| s.name == SpanName::RuntimeUsage)
        .collect();
    assert_eq!(usage.len(), 2, "one per model");
    for span in &usage {
        assert_eq!(span.parent_span_id.as_ref(), Some(&attempt.context.span_id));
        assert_eq!(span.context.trace_id, story_root.trace_id);
        assert_eq!(span.attributes["loom.usage.scope"], "attempt");
        assert_eq!(span.attributes["loom.attempt"], "1");
        assert_eq!(span.attributes["loom.issue"], "9303");
        assert!(span.attributes.contains_key("loom.cost.usd_estimate"));
    }
    // Deduped: each streamed message once (100 + 200 + 1000 + 100 + 300 = 1700).
    assert_eq!(total(&spans), 1700 + 170);

    // A re-run journals nothing new.
    let again =
        record_transcript(ws, &req, &path, Parent::Story(story(9303)), &Pricing::with(None));
    assert_eq!(again, Outcome::Recorded(0));
}

#[test]
fn a_doctor_retry_gives_two_attempts_whose_sum_is_the_execution_total() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let first = transcript(ws, "d2", &[("claude-sonnet-5", 40)]);
    let second = transcript(ws, "d3", &[("claude-sonnet-5", 7), ("claude-haiku-4-5", 5)]);
    let pricing = Pricing::with(None);
    for (attempt, agent, path) in [(2, "d2", &first), (3, "d3", &second)] {
        let req = request("doctor", Some(attempt), agent);
        let outcome = record_transcript(ws, &req, path, Parent::Story(story(9303)), &pricing);
        assert_eq!(outcome, Outcome::Recorded(if attempt == 2 { 2 } else { 3 }));
    }
    let spans = journalled(ws, "insession-sweep-20260928T020000Z-1-abc");
    let attempts: Vec<_> = spans
        .iter()
        .filter(|s| s.name == SpanName::RoleAttempt)
        .collect();
    assert_eq!(attempts.len(), 2);
    assert_ne!(attempts[0].context.span_id, attempts[1].context.span_id);

    // The execution total over the same session (whose parent transcript
    // carries no usage) is the two subagents' rows together.
    let mut merged = std::collections::BTreeMap::new();
    merge_rows(&mut merged, sum_transcript_usage_by_model(&first));
    merge_rows(&mut merged, sum_transcript_usage_by_model(&second));
    let rows: Vec<_> = merged.into_values().collect();
    let store = TraceStore::new(ws);
    store.load_or_create(ws, "sweep-issue-9303").unwrap();
    let window = (Utc::now() - Duration::minutes(5), Utc::now());
    let execution = journal_usage(ws, "sweep-issue-9303", window, Some(&rows), None).unwrap();
    assert_eq!(total(&spans), total(&execution));
    assert_eq!(total(&execution), TokenUsage::from_models(&rows).total());
}

#[test]
fn an_inherited_context_parents_usage_to_the_roles_newest_attempt() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let store = TraceStore::new(ws);
    let saved = store.load_or_create(ws, "sweep-issue-1").unwrap();
    let journal = Journal::for_context(&store.path(ws, "sweep-issue-1"));
    let mut root_attributes = TraceAttributes::new();
    root_attributes.insert("loom.sweep_id".into(), "sweep-issue-1".into());
    journal
        .start(saved.context.clone(), None, SpanName::Sweep, Utc::now(), root_attributes)
        .unwrap();
    let mut role = TraceAttributes::new();
    role.insert("loom.role".into(), "judge".into());
    let older = saved.context.derived_child(&["a"]);
    let newer = saved.context.derived_child(&["b"]);
    for (context, ended) in [(&older, 1), (&newer, 2)] {
        let at = Utc::now() - Duration::minutes(10 - ended);
        journal
            .append_completed(SpanRecord {
                context: context.clone(),
                parent_span_id: Some(saved.context.span_id.clone()),
                name: SpanName::RoleAttempt,
                started_at: at,
                ended_at: at,
                status: SpanStatus::Ok,
                attributes: role.clone(),
                events: Vec::new(),
                links: Vec::new(),
            })
            .unwrap();
    }
    let path = transcript(ws, "j1", &[("claude-opus-5", 3)]);
    let parent = Parent::Inherited {
        journal: journal.clone(),
        root: saved.context.clone(),
    };
    let outcome =
        record_transcript(ws, &request("judge", None, "j1"), &path, parent, &Pricing::with(None));
    assert_eq!(outcome, Outcome::Recorded(1), "usage only; the attempt already exists");
    let usage = journal
        .completed()
        .unwrap()
        .into_iter()
        .find(|s| s.name == SpanName::RuntimeUsage)
        .unwrap();
    assert_eq!(usage.parent_span_id.as_ref(), Some(&newer.span_id));
    assert_eq!(usage.attributes["loom.sweep_id"], "sweep-issue-1");
}

#[test]
fn unknown_usage_journals_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let req = request("builder", Some(1), "none");
    let missing = ws.join("agent-missing.jsonl");
    let empty = ws.join("agent-empty.jsonl");
    std::fs::write(&empty, "{\"type\":\"user\"}\n").unwrap();
    for path in [missing, empty] {
        let outcome =
            record_transcript(ws, &req, &path, Parent::Story(story(1)), &Pricing::with(None));
        assert!(matches!(outcome, Outcome::Skipped(_)), "{outcome:?}");
    }
    assert!(!ws.join(".loom/logs/trace-context").exists());
}

#[test]
fn a_recorder_failure_is_an_outcome_not_a_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    std::fs::create_dir_all(ws.join(".loom/logs")).unwrap();
    // The trace store's directory is a plain file: nothing can be journalled.
    std::fs::write(ws.join(".loom/logs/trace-context"), "not a directory").unwrap();
    let path = transcript(ws, "f1", &[("claude-opus-5", 1)]);
    let outcome = record_transcript(
        ws,
        &request("builder", Some(1), "f1"),
        &path,
        Parent::Story(story(1)),
        &Pricing::with(None),
    );
    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
}

#[test]
fn agent_transcripts_resolve_under_the_workspaces_project_and_ids_are_validated() {
    let tmp = tempfile::tempdir().unwrap();
    let projects = tmp.path().join("projects");
    let ws = tmp.path().join("ws");
    let session = projects
        .join(crate::transcript_tokens::project_slug(&ws))
        .join("uuid-1")
        .join("subagents");
    std::fs::create_dir_all(&session).unwrap();
    std::fs::write(session.join("agent-abc123.jsonl"), "{}\n").unwrap();
    let found = find_agent_transcript(&projects, &[ws.as_path()], "abc123").unwrap();
    assert_eq!(found, session.join("agent-abc123.jsonl"));
    assert_eq!(find_agent_transcript(&projects, &[ws.as_path()], "agent-abc123"), Some(found));
    assert!(find_agent_transcript(&projects, &[ws.as_path()], "../x").is_none());
    assert!(find_agent_transcript(&projects, &[ws.as_path()], "nope").is_none());
}
