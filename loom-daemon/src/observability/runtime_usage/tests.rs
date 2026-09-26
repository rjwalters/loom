//! Tests for the per-execution usage span and the session trace join
//! (Issue #8908).

use std::path::Path;
use std::sync::Arc;

use chrono::{Duration, Utc};

use super::join::{context_for_session_at, open_at, workspace_of, JoinEntry, JOIN_DIR};
use super::*;
use crate::activity::transcript_ingest::{ingest, IngestOptions};
use crate::activity::ActivityDb;
use crate::observability::queue::DurableQueue;
use crate::observability::session_summary::SessionSummarySink;
use crate::telemetry::TelemetryRecord;

fn row(input: i64, output: i64, read: i64, w5: i64, w1: i64) -> ModelUsageTotals {
    ModelUsageTotals {
        model: "m".into(),
        speed: "standard".into(),
        service_tier: "standard".into(),
        input,
        cache_read: read,
        cache_write_5m: w5,
        cache_write_1h: w1,
        output,
    }
}

#[test]
fn usage_sums_every_model_row_with_both_cache_write_buckets() {
    let usage = TokenUsage::from_models(&[row(10, 20, 30, 1, 2), row(5, 5, 0, 0, 7)]);
    assert_eq!(
        usage,
        TokenUsage {
            input: 15,
            output: 25,
            cache_read: 30,
            cache_write: 10
        }
    );
    assert_eq!(usage.total(), 80);
}

#[test]
fn the_usage_span_carries_only_allowlisted_counters_and_zero_stays_zero() {
    let parent = TraceContext::root(true);
    let at = Utc::now();
    let span = usage_span(&parent, at, at, TokenUsage::default(), Some("claude"));
    assert_eq!(span.name.as_str(), "loom.runtime.usage");
    assert_eq!(span.context.trace_id, parent.trace_id);
    assert_eq!(span.parent_span_id.as_ref(), Some(&parent.span_id));
    for key in [
        "loom.tokens.input",
        "loom.tokens.output",
        "loom.tokens.cache_read",
        "loom.tokens.cache_write",
        "loom.tokens.total",
    ] {
        assert_eq!(span.attributes[key], "0", "a measured zero is exported: {key}");
        assert!(crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&key));
    }
    assert_eq!(span.attributes.len(), 6, "{:?}", span.attributes);
    assert_eq!(span.clone().bounded(), span, "survives export policy unchanged");
}

/// A traced execution with one completed `loom.runtime.run` span.
fn traced_execution(root: &Path, execution: &str) -> (TraceContext, SpanRecord) {
    let store = TraceStore::new(root);
    let saved = store.load_or_create(root, execution).unwrap();
    let journal = Journal::for_context(&store.path(root, execution));
    let t0 = Utc::now() - Duration::minutes(30);
    let run = journal
        .start(
            saved.context.child(),
            Some(&saved.context),
            SpanName::RuntimeRun,
            t0,
            TraceAttributes::new(),
        )
        .unwrap();
    journal
        .finish(&run, t0 + Duration::minutes(20), SpanStatus::Ok, TraceAttributes::new())
        .unwrap();
    let run = journal
        .completed()
        .unwrap()
        .into_iter()
        .find(|s| s.name == SpanName::RuntimeRun)
        .unwrap();
    (saved.context, run)
}

#[test]
fn unknown_usage_journals_no_span() {
    let tmp = tempfile::tempdir().unwrap();
    traced_execution(tmp.path(), "sweep-1");
    let window = (Utc::now(), Utc::now());
    assert!(journal_usage(tmp.path(), "sweep-1", window, None, None)
        .unwrap()
        .is_none());
    let store = TraceStore::new(tmp.path());
    let spans = Journal::for_context(&store.path(tmp.path(), "sweep-1"))
        .completed()
        .unwrap();
    assert!(spans.iter().all(|s| s.name != SpanName::RuntimeUsage));
}

#[test]
fn an_untraced_execution_journals_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let window = (Utc::now(), Utc::now());
    let usage = Some(TokenUsage::default());
    assert!(journal_usage(tmp.path(), "never-traced", window, usage, None)
        .unwrap()
        .is_none());
    assert!(!tmp.path().join(".loom/logs/trace-context").exists());
}

#[test]
fn the_usage_span_is_a_child_of_the_runtime_run_span_over_its_interval() {
    let tmp = tempfile::tempdir().unwrap();
    let (root_context, run) = traced_execution(tmp.path(), "sweep-2");
    let window = (Utc::now() - Duration::hours(1), Utc::now());
    let usage = TokenUsage {
        input: 3,
        output: 4,
        cache_read: 5,
        cache_write: 6,
    };
    let span = journal_usage(tmp.path(), "sweep-2", window, Some(usage), Some("opencode"))
        .unwrap()
        .unwrap();
    assert_eq!(span.context.trace_id, root_context.trace_id);
    assert_eq!(span.parent_span_id.as_ref(), Some(&run.context.span_id));
    assert_eq!((span.started_at, span.ended_at), (run.started_at, run.ended_at));
    assert_eq!(span.attributes["loom.tokens.total"], "18");
    assert_eq!(span.attributes["loom.runtime"], "opencode");

    // It drains to the export queue like every other journalled span.
    let store = TraceStore::new(tmp.path());
    let journal = Journal::for_context(&store.path(tmp.path(), "sweep-2"));
    let mut drained = Vec::new();
    journal
        .drain(|record| {
            drained.push(record);
            Ok(())
        })
        .unwrap();
    assert!(drained.iter().any(|s| s.name == SpanName::RuntimeUsage));
}

fn entry(issue: u32, started: i64, ended: Option<i64>) -> JoinEntry {
    let base = Utc::now();
    JoinEntry {
        issue,
        context: TraceContext::root(true),
        started_at: base + Duration::minutes(started),
        ended_at: ended.map(|m| base + Duration::minutes(m)),
    }
}

fn plant(root: &Path, entry: &JoinEntry) {
    let dir = root.join(JOIN_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{}.json", entry.context.trace_id.as_str())),
        serde_json::to_vec(entry).unwrap(),
    )
    .unwrap();
}

#[test]
fn a_session_joins_only_when_exactly_one_entry_names_its_issue_and_covers_its_start() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let cwd = root.to_string_lossy().into_owned();
    let now = Utc::now();
    let one = entry(8908, -60, Some(-10));
    plant(root, &one);
    plant(root, &entry(8909, -60, None));
    let at = |m: i64| Some(now + Duration::minutes(m));

    let joined = context_for_session_at(Some(&cwd), Some(8908), at(-30), now);
    assert_eq!(joined, Some(one.context.clone()));
    // An issue worktree resolves to the same workspace.
    let worktree = format!("{cwd}/.loom/worktrees/issue-8908");
    assert_eq!(workspace_of(Path::new(&worktree)), root);
    assert!(context_for_session_at(Some(&worktree), Some(8908), at(-30), now).is_some());
    // Outside the window, another issue, or no issue: unjoined.
    assert_eq!(context_for_session_at(Some(&cwd), Some(8908), at(-5), now), None);
    assert_eq!(context_for_session_at(Some(&cwd), Some(8908), at(-90), now), None);
    assert_eq!(context_for_session_at(Some(&cwd), Some(1), at(-30), now), None);
    assert_eq!(context_for_session_at(Some(&cwd), None, at(-30), now), None);
    // Ambiguous (a second covering entry for the same issue): never guessed.
    plant(root, &entry(8908, -45, None));
    assert_eq!(context_for_session_at(Some(&cwd), Some(8908), at(-30), now), None);
}

#[test]
fn close_ends_and_renames_an_entry_opened_under_the_pre_story_name() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let context = TraceStore::new(root)
        .load_or_create(root, "in-flight")
        .unwrap()
        .context;
    let legacy = root
        .join(JOIN_DIR)
        .join(format!("{}.json", context.trace_id.as_str()));
    let opened = JoinEntry {
        issue: 9038,
        context: context.clone(),
        started_at: Utc::now() - Duration::minutes(5),
        ended_at: None,
    };
    std::fs::create_dir_all(root.join(JOIN_DIR)).unwrap();
    std::fs::write(&legacy, serde_json::to_vec(&opened).unwrap()).unwrap();
    super::join::close(root, "in-flight", Utc::now());
    assert!(!legacy.exists());
    let current = root.join(JOIN_DIR).join(format!(
        "{}-{}.json",
        context.trace_id.as_str(),
        context.span_id.as_str()
    ));
    let closed: JoinEntry = serde_json::from_slice(&std::fs::read(current).unwrap()).unwrap();
    assert_eq!(closed.context, context);
    assert!(closed.ended_at.is_some());
}

#[test]
fn expired_entries_are_pruned() {
    let tmp = tempfile::tempdir().unwrap();
    let stale = entry(1, -3 * 24 * 60, Some(-2 * 24 * 60));
    plant(tmp.path(), &stale);
    let cwd = tmp.path().to_string_lossy().into_owned();
    let at = Some(stale.started_at + Duration::minutes(1));
    assert_eq!(context_for_session_at(Some(&cwd), Some(1), at, Utc::now()), None);
    assert_eq!(
        std::fs::read_dir(tmp.path().join(JOIN_DIR))
            .unwrap()
            .count(),
        0
    );
}

/// #8908 acceptance: for a traced sweep, the `session.summary` log the
/// transcript-ingest pass emits and the execution's usage span share one
/// trace id — fixture transcript + real trace store + real ingest pass.
#[test]
fn a_traced_sweeps_session_summary_and_usage_span_share_one_trace_id() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let (root_context, _run) = traced_execution(&workspace, "sweep-issue-8908");
    let dispatched = Utc::now() - Duration::minutes(40);
    open_at(&workspace, "sweep-issue-8908", 8908, dispatched).unwrap();

    // The sweep's Claude transcript, written in the workspace root.
    let cwd = workspace.to_string_lossy().into_owned();
    let ts = (dispatched + Duration::minutes(1)).to_rfc3339();
    let lines = [
        serde_json::json!({"type": "user", "sessionId": "uuid-8908", "cwd": cwd,
            "timestamp": ts, "message": {"role": "user", "content":
            "<command-name>/loom:sweep</command-name>\n<command-args>8908</command-args>"}}),
        serde_json::json!({"type": "assistant", "sessionId": "uuid-8908", "cwd": cwd,
            "timestamp": ts, "message": {"model": "claude-sonnet-5", "id": "msg_1",
            "content": [{"type": "text", "text": "PRIVATE transcript body"}],
            "usage": {"input_tokens": 11, "output_tokens": 22,
                      "cache_read_input_tokens": 33, "cache_creation_input_tokens": 44}}}),
    ];
    let projects = tmp.path().join("projects");
    let project = projects.join(crate::transcript_tokens::project_slug(&workspace));
    std::fs::create_dir_all(&project).unwrap();
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(project.join("uuid-8908.jsonl"), body).unwrap();

    let queue_path = tmp.path().join("queue.jsonl");
    let db = ActivityDb::new(tmp.path().join("activity.db")).unwrap();
    ingest(
        &db,
        &IngestOptions {
            projects_dir: projects,
            summary_sink: Some(SessionSummarySink::new(
                Arc::new(DurableQueue::open(queue_path.clone(), 100)),
                "host-test",
            )),
            ..IngestOptions::default()
        },
    )
    .unwrap();
    let envelopes = DurableQueue::open(queue_path, 100).peek_batch(10);
    let summary = envelopes
        .iter()
        .find(|e| matches!(e.record, TelemetryRecord::SessionSummary(_)))
        .expect("one session.summary");
    let log_trace = summary
        .trace_context
        .as_ref()
        .expect("joined to the sweep's trace");
    assert_eq!(log_trace.trace_id, root_context.trace_id);
    let wire = serde_json::to_string(summary).unwrap();
    assert!(!wire.contains("PRIVATE"), "no transcript body is exported");

    // The terminal transition journals the usage span into the same trace.
    let span = journal_usage(
        &workspace,
        "sweep-issue-8908",
        (dispatched, Utc::now()),
        Some(TokenUsage::from_models(&[row(11, 22, 33, 44, 0)])),
        None,
    )
    .unwrap()
    .unwrap();
    assert_eq!(span.context.trace_id, log_trace.trace_id);
    assert_eq!(span.attributes["loom.tokens.total"], "110");
}
