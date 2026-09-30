//! Tests for the per-execution usage span and the session trace join
//! (Issue #8908).

use std::path::Path;
use std::sync::Arc;

use chrono::{Duration, Utc};

use super::join::{
    context_for_session_at, open_at, open_keyed_at, workspace_of, JoinEntry, JoinKey, JOIN_DIR,
};
use super::*;
use crate::activity::transcript_ingest::{ingest, IngestOptions};
use crate::activity::ActivityDb;
use crate::observability::queue::DurableQueue;
use crate::observability::session_summary::SessionSummarySink;
use crate::telemetry::trace::{SpanStatus, TraceContext};
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
    let mut zero = row(0, 0, 0, 0, 0);
    zero.model = "claude-sonnet-5".into();
    let mut common = TraceAttributes::new();
    common.insert("loom.runtime".into(), "claude".into());
    let spans = spans::model_usage_spans(
        &parent,
        (at, at),
        &[zero],
        spans::UsageScope::Execution,
        &common,
        &cost::Pricing::with(None),
    );
    let [span] = spans.as_slice() else {
        panic!("one model, one span: {spans:?}");
    };
    assert_eq!(span.name.as_str(), "loom.runtime.usage");
    assert_eq!(span.context.trace_id, parent.trace_id);
    assert_eq!(span.parent_span_id.as_ref(), Some(&parent.span_id));
    for key in [
        "loom.tokens.input",
        "loom.tokens.output",
        "loom.tokens.cache_read",
        "loom.tokens.cache_write",
        "loom.tokens.total",
        "loom.tokens.cache_write_5m",
        "loom.tokens.cache_write_1h",
        "gen_ai.usage.input_tokens",
        "gen_ai.usage.output_tokens",
        "gen_ai.usage.cache_read_input_tokens",
        "gen_ai.usage.cache_creation_input_tokens",
    ] {
        assert_eq!(span.attributes[key], "0", "a measured zero is exported: {key}");
        assert!(crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&key));
    }
    assert_eq!(span.attributes["loom.cost.usd_estimate"], "0.000000");
    assert_eq!(span.attributes["loom.usage.scope"], "execution");
    assert_eq!(span.attributes["loom.model"], "claude-sonnet-5");
    // 11 counters, 4 cost keys, runtime, model, scope, and 3 provenance keys.
    assert_eq!(span.attributes.len(), 21, "{:?}", span.attributes);
    assert_eq!(span.clone().bounded(), *span, "survives export policy unchanged");
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
        .is_empty());
    let empty: &[ModelUsageTotals] = &[];
    assert!(journal_usage(tmp.path(), "sweep-1", window, Some(empty), None)
        .unwrap()
        .is_empty());
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
    let rows = [row(1, 1, 1, 1, 1)];
    assert!(journal_usage(tmp.path(), "never-traced", window, Some(&rows), None)
        .unwrap()
        .is_empty());
    assert!(!tmp.path().join(".loom/logs/trace-context").exists());
}

#[test]
fn the_usage_span_is_a_child_of_the_runtime_run_span_over_its_interval() {
    let tmp = tempfile::tempdir().unwrap();
    let (root_context, run) = traced_execution(tmp.path(), "sweep-2");
    let window = (Utc::now() - Duration::hours(1), Utc::now());
    let rows = [row(3, 4, 5, 6, 0)];
    let spans =
        journal_usage(tmp.path(), "sweep-2", window, Some(&rows), Some("opencode")).unwrap();
    let [span] = spans.as_slice() else {
        panic!("one model, one span: {spans:?}");
    };
    assert_eq!(span.context.trace_id, root_context.trace_id);
    assert_eq!(span.parent_span_id.as_ref(), Some(&run.context.span_id));
    assert_eq!((span.started_at, span.ended_at), (run.started_at, run.ended_at));
    assert_eq!(span.attributes["loom.tokens.total"], "18");
    assert_eq!(span.attributes["loom.sweep_id"], "sweep-2");
    assert_eq!(span.attributes["loom.usage.scope"], "execution");
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

fn keyed_entry(key: JoinKey<'_>, started: i64, ended: Option<i64>) -> JoinEntry {
    let base = Utc::now();
    let mut entry =
        JoinEntry::new(key, TraceContext::root(true), base + Duration::minutes(started));
    entry.ended_at = ended.map(|m| base + Duration::minutes(m));
    entry
}

fn entry(issue: u32, started: i64, ended: Option<i64>) -> JoinEntry {
    keyed_entry(JoinKey::Issue(issue), started, ended)
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

    let joined = context_for_session_at(Some(&cwd), Some(8908), None, at(-30), now);
    assert_eq!(joined, Some(one.context.clone()));
    // An issue worktree resolves to the same workspace.
    let worktree = format!("{cwd}/.loom/worktrees/issue-8908");
    assert_eq!(workspace_of(Path::new(&worktree)), root);
    assert!(context_for_session_at(Some(&worktree), Some(8908), None, at(-30), now).is_some());
    // Outside the window, another issue, or no issue: unjoined.
    assert_eq!(context_for_session_at(Some(&cwd), Some(8908), None, at(-5), now), None);
    assert_eq!(context_for_session_at(Some(&cwd), Some(8908), None, at(-90), now), None);
    assert_eq!(context_for_session_at(Some(&cwd), Some(1), None, at(-30), now), None);
    assert_eq!(context_for_session_at(Some(&cwd), None, None, at(-30), now), None);
    // Ambiguous (a second covering entry for the same issue): never guessed.
    plant(root, &entry(8908, -45, None));
    assert_eq!(context_for_session_at(Some(&cwd), Some(8908), None, at(-30), now), None);
}

/// #9013 item 2: past `MAX_ENTRIES` candidates, a second match could be
/// sitting unread — report ambiguous (unjoined) rather than risk an
/// incorrect single match.
#[test]
fn a_capped_directory_reports_ambiguous_even_with_one_real_covering_entry() {
    use super::join::MAX_ENTRIES;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let cwd = root.to_string_lossy().into_owned();
    let now = Utc::now();
    let at = |m: i64| Some(now + Duration::minutes(m));

    // The one entry that would otherwise join cleanly.
    let real = entry(8908, -60, Some(-10));
    plant(root, &real);
    // Below the cap: still joins normally.
    assert_eq!(
        context_for_session_at(Some(&cwd), Some(8908), None, at(-30), now),
        Some(real.context.clone())
    );

    // Push the directory past the cap with filler entries for other issues.
    for issue in 0..(MAX_ENTRIES as u32 + 10) {
        plant(root, &entry(90_000 + issue, -60, Some(-10)));
    }
    assert_eq!(
        context_for_session_at(Some(&cwd), Some(8908), None, at(-30), now),
        None,
        "capped: never guess a single match past the read limit"
    );
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
    let opened =
        JoinEntry::new(JoinKey::Issue(9038), context.clone(), Utc::now() - Duration::minutes(5));
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

/// #9013 item 3: an entry left open across a restart (no live process wrote
/// it since) is closed by the reconciliation pass instead of surviving the
/// full 7-day open-retention window — bounded down to ordinary closed-entry
/// retention, same as if the execution had exited cleanly.
#[test]
fn restart_reconciliation_closes_entries_still_open() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let cwd = root.to_string_lossy().into_owned();
    let now = Utc::now();

    // Orphaned: still open, no live process behind it.
    let orphan = entry(9013, -60, None);
    plant(root, &orphan);
    // Already closed: reconciliation must leave its `ended_at` alone.
    let already_closed = entry(1, -600, Some(-590));
    plant(root, &already_closed);

    let closed = super::join::close_orphaned_entries_at(root, now);
    assert_eq!(closed, 1, "exactly the one still-open entry");

    // The orphan is now closed at `now`, so a session starting well after it
    // no longer matches — the whole point of bounding its retention down.
    let at = Some(now + Duration::minutes(5));
    assert_eq!(context_for_session_at(Some(&cwd), Some(9013), None, at, now), None);
    // A session inside its (now-bounded) window still joins.
    let inside = Some(orphan.started_at + Duration::minutes(1));
    assert_eq!(
        context_for_session_at(Some(&cwd), Some(9013), None, inside, now),
        Some(orphan.context)
    );

    // The already-closed entry survived untouched.
    let path = root
        .join(JOIN_DIR)
        .join(format!("{}.json", already_closed.context.trace_id.as_str()));
    let unchanged: JoinEntry = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(unchanged.ended_at, already_closed.ended_at);

    // Idempotent: nothing left open to close on a second pass.
    assert_eq!(super::join::close_orphaned_entries_at(root, now), 0);
}

#[test]
fn expired_entries_are_pruned() {
    let tmp = tempfile::tempdir().unwrap();
    let stale = entry(1, -3 * 24 * 60, Some(-2 * 24 * 60));
    plant(tmp.path(), &stale);
    let cwd = tmp.path().to_string_lossy().into_owned();
    let at = Some(stale.started_at + Duration::minutes(1));
    assert_eq!(context_for_session_at(Some(&cwd), Some(1), None, at, Utc::now()), None);
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
    let spans = journal_usage(
        &workspace,
        "sweep-issue-8908",
        (dispatched, Utc::now()),
        Some(&[row(11, 22, 33, 44, 0)]),
        None,
    )
    .unwrap();
    let span = &spans[0];
    assert_eq!(span.context.trace_id, log_trace.trace_id);
    assert_eq!(span.attributes["loom.tokens.total"], "110");
}

/// #9013 item 5 (documented case): a subagent's own transcript names no
/// issue in its head (it never restates the parent's `/loom:sweep N`
/// command), so `context_for_session` never even reaches the trace-join
/// lookup for it — `summary.issue` is `None`, and `issue?` short-circuits —
/// even though the very same trace-join entry the sweep opened, covering the
/// very same window, joins the *parent* transcript's own `session.summary`
/// without ambiguity. The gap named by the module doc ("A subagent
/// transcript whose head names no issue is therefore unjoined") is this
/// module's documented, intentional fail-safe, not a bug: never guess an
/// issue from a subagent's own prose.
#[test]
fn a_subagent_transcript_naming_no_issue_stays_unjoined_while_its_parent_joins() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    traced_execution(&workspace, "sweep-issue-8908");
    let dispatched = Utc::now() - Duration::minutes(40);
    open_at(&workspace, "sweep-issue-8908", 8908, dispatched).unwrap();

    let cwd = workspace.to_string_lossy().into_owned();
    let ts = (dispatched + Duration::minutes(1)).to_rfc3339();
    let projects = tmp.path().join("projects");
    let project = projects.join(crate::transcript_tokens::project_slug(&workspace));
    std::fs::create_dir_all(&project).unwrap();

    // The parent session: its head names the issue.
    let parent_lines = [
        serde_json::json!({"type": "user", "sessionId": "uuid-parent", "cwd": cwd,
            "timestamp": ts, "message": {"role": "user", "content":
            "<command-name>/loom:sweep</command-name>\n<command-args>8908</command-args>"}}),
        serde_json::json!({"type": "assistant", "sessionId": "uuid-parent", "cwd": cwd,
            "timestamp": ts, "message": {"model": "claude-sonnet-5", "id": "msg_1",
            "content": [{"type": "text", "text": "parent body"}],
            "usage": {"input_tokens": 1, "output_tokens": 1}}}),
    ];
    let parent_body: String = parent_lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(project.join("uuid-parent.jsonl"), parent_body).unwrap();

    // A subagent transcript of that same session: its own first user
    // message is ordinary task text, no `/loom:` slash command anywhere.
    let subagents_dir = project.join("uuid-parent").join("subagents");
    std::fs::create_dir_all(&subagents_dir).unwrap();
    let subagent_lines = [
        serde_json::json!({"type": "user", "sessionId": "uuid-subagent", "cwd": cwd,
            "timestamp": ts, "message": {"role": "user", "content":
            "Investigate the flaky test and report back."}}),
        serde_json::json!({"type": "assistant", "sessionId": "uuid-subagent", "cwd": cwd,
            "timestamp": ts, "message": {"model": "claude-sonnet-5", "id": "msg_2",
            "content": [{"type": "text", "text": "subagent body"}],
            "usage": {"input_tokens": 2, "output_tokens": 2}}}),
    ];
    let subagent_body: String = subagent_lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(subagents_dir.join("builder.jsonl"), subagent_body).unwrap();

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
    let summaries: Vec<_> = envelopes
        .iter()
        .filter_map(|e| match &e.record {
            TelemetryRecord::SessionSummary(s) => Some((s, e.trace_context.as_ref())),
            _ => None,
        })
        .collect();
    assert_eq!(summaries.len(), 2, "one summary per transcript");

    let (parent_summary, parent_trace) = summaries
        .iter()
        .find(|(s, _)| s.session_id == "uuid-parent")
        .expect("parent summary");
    assert_eq!(parent_summary.issue, Some(8908));
    assert!(parent_trace.is_some(), "the parent joins the sweep's trace");

    let (subagent_summary, subagent_trace) = summaries
        .iter()
        .find(|(s, _)| s.session_id != "uuid-parent")
        .expect("subagent summary");
    assert_eq!(subagent_summary.issue, None, "no slash command in the subagent's own head");
    assert!(subagent_trace.is_none(), "unattributed issue never guesses a join");
}

// ---------------------------------------------------------------------------
// Role-runner ticks (Issue #9231): the same span + join treatment #8908 gave a
// sweep, keyed on the role a tick launched as rather than on an issue it never
// names.
// ---------------------------------------------------------------------------

fn role_entry(role: &str, started: i64, ended: Option<i64>) -> JoinEntry {
    keyed_entry(JoinKey::Role(role), started, ended)
}

/// #9231 acceptance: a **role-runner tick** — not a sweep, and naming no issue
/// anywhere — produces a `loom.runtime.usage` span and a `session.summary`
/// record that share one trace id. Fixture transcript, real trace store, real
/// ingest pass, real usage journalling; the only thing stood in for is the
/// enablement check (`tracing::enabled` is `cfg!(feature = "otlp")`-gated), so
/// the test calls the same functions the wired path calls, one layer in.
#[test]
fn a_traced_role_ticks_session_summary_and_usage_span_share_one_trace_id() {
    use crate::observability::lifecycle::{role_execution_id, RoleTrace};

    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    // Dispatch, exactly as `lifecycle::role_invocation` does it: an execution
    // keyed by `role_execution_id` (the tick's own identity — role + start
    // instant, NOT the per-role log's `tick_anchor`), its persisted root
    // context, and a role-keyed join entry opened for it.
    let started_at = Utc::now() - Duration::minutes(10);
    let execution = role_execution_id("judge", started_at);
    let root_context = TraceStore::new(&workspace)
        .load_or_create(&workspace, &execution)
        .unwrap()
        .context;
    open_keyed_at(&workspace, &execution, JoinKey::Role("judge"), started_at).unwrap();

    // The tick's own Claude transcript. Its head is a bare `/loom:judge` with
    // NO argument; its cwd is the workspace root, never an `issue-<N>`
    // worktree; and there is no `feature/issue-<N>` branch — so all three of
    // `session_context`'s issue sources resolve to `None`, which is precisely
    // why an issue-keyed entry could never have joined this log.
    let cwd = workspace.to_string_lossy().into_owned();
    let ts = (started_at + Duration::minutes(1)).to_rfc3339();
    let lines = [
        serde_json::json!({"type": "user", "sessionId": "uuid-judge-tick", "cwd": cwd,
            "timestamp": ts, "message": {"role": "user", "content":
            "<command-name>/loom:judge</command-name>\n<command-args></command-args>"}}),
        serde_json::json!({"type": "assistant", "sessionId": "uuid-judge-tick", "cwd": cwd,
            "timestamp": ts, "message": {"model": "claude-sonnet-5", "id": "msg_1",
            "content": [{"type": "text", "text": "PRIVATE transcript body"}],
            "usage": {"input_tokens": 11, "output_tokens": 22,
                      "cache_read_input_tokens": 33, "cache_creation_input_tokens": 44}}}),
    ];
    let projects = tmp.path().join("projects");
    let project = projects.join(crate::transcript_tokens::project_slug(&workspace));
    std::fs::create_dir_all(&project).unwrap();
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    std::fs::write(project.join("uuid-judge-tick.jsonl"), body).unwrap();

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
    let (summary, log_trace) = envelopes
        .iter()
        .find_map(|e| match &e.record {
            TelemetryRecord::SessionSummary(s) => Some((s, e.trace_context.as_ref())),
            _ => None,
        })
        .expect("one session.summary");
    assert_eq!(summary.issue, None, "a role tick names no issue — that is the whole point");
    let log_trace = log_trace.expect("joined to the tick's trace on its role key");
    assert_eq!(log_trace.trace_id, root_context.trace_id);
    let wire = serde_json::to_string(&envelopes[0]).unwrap();
    assert!(!wire.contains("PRIVATE"), "no transcript body is exported");

    // Terminal transition, exactly as `role_tick_telemetry::emit_correlated`
    // does it: the per-model usage spans, then the join close.
    let ended_at = Utc::now();
    let trace = RoleTrace {
        context: root_context.clone(),
        execution: execution.clone(),
        started_at,
    };
    crate::role_tick_telemetry::usage::journal_execution(
        &workspace,
        &trace,
        "judge",
        ended_at,
        Some("claude"),
        &[row(11, 22, 33, 44, 0)],
    );
    let store = TraceStore::new(&workspace);
    let spans = Journal::for_context(&store.path(&workspace, &execution))
        .completed()
        .unwrap();
    let span = spans
        .iter()
        .find(|s| s.name == SpanName::RuntimeUsage)
        .expect("the tick journalled a loom.runtime.usage span");
    assert_eq!(span.name.as_str(), "loom.runtime.usage");
    assert_eq!(span.context.trace_id, log_trace.trace_id, "one trace id for log and span");
    assert_eq!(span.attributes["loom.tokens.total"], "110");
    assert_eq!(span.attributes["loom.role"], "judge");
    assert_eq!(span.attributes["loom.sweep_id"], execution);

    super::join::close(&workspace, &execution, ended_at);
    let closed: JoinEntry = serde_json::from_slice(
        &std::fs::read(workspace.join(JOIN_DIR).join(format!(
            "{}-{}.json",
            root_context.trace_id.as_str(),
            root_context.span_id.as_str()
        )))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(closed.role.as_deref(), Some("judge"));
    assert_eq!(closed.issue, None);
    assert_eq!(closed.ended_at, Some(ended_at), "the terminal transition closed the entry");
}

/// #9231: the role key joins the role's own tick and nothing else. Champion
/// and Doctor ticks name no issue either, so this is the ordinary case for
/// every role-runner role — and the cases it must refuse are what keep it from
/// stealing a sweep phase's log.
#[test]
fn a_role_keyed_entry_joins_only_a_session_that_launched_as_that_role() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let cwd = root.to_string_lossy().into_owned();
    let now = Utc::now();
    let at = |m: i64| Some(now + Duration::minutes(m));

    let champion = role_entry("champion", -60, Some(-10));
    plant(root, &champion);
    plant(root, &role_entry("doctor", -60, None));

    // The tick's own session: no issue, head named `/loom:champion`.
    assert_eq!(
        context_for_session_at(Some(&cwd), None, Some("champion"), at(-30), now),
        Some(champion.context.clone())
    );
    // Case-insensitive, like every other role comparison in the daemon.
    assert_eq!(
        context_for_session_at(Some(&cwd), None, Some("Champion"), at(-30), now),
        Some(champion.context.clone())
    );
    // Another role, outside the window, or no key at all: unjoined.
    assert_eq!(context_for_session_at(Some(&cwd), None, Some("judge"), at(-30), now), None);
    assert_eq!(context_for_session_at(Some(&cwd), None, Some("champion"), at(-5), now), None);
    assert_eq!(context_for_session_at(Some(&cwd), None, None, at(-30), now), None);

    // A session that names an issue joins that issue's execution or nothing —
    // it never falls through to a role key. Otherwise a sweep phase whose own
    // execution is untraced would attach itself to whatever role-runner tick
    // happened to be open.
    assert_eq!(
        context_for_session_at(Some(&cwd), Some(9231), Some("champion"), at(-30), now),
        None
    );

    // Two covering entries for the same role (an operator hand-running the
    // same slash command in the tick's window): ambiguous, never guessed.
    plant(root, &role_entry("champion", -45, None));
    assert_eq!(context_for_session_at(Some(&cwd), None, Some("champion"), at(-30), now), None);
}

/// #9231: an issue-keyed and a role-keyed entry are distinct keys, not two
/// spellings of one. Neither matches the other's session even with identical
/// windows in the same workspace.
#[test]
fn issue_keyed_and_role_keyed_entries_never_match_each_others_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let cwd = root.to_string_lossy().into_owned();
    let now = Utc::now();
    let at = Some(now - Duration::minutes(30));

    let sweep = entry(9231, -60, None);
    let tick = role_entry("judge", -60, None);
    plant(root, &sweep);
    plant(root, &tick);

    assert_eq!(
        context_for_session_at(Some(&cwd), Some(9231), None, at, now),
        Some(sweep.context)
    );
    assert_eq!(
        context_for_session_at(Some(&cwd), None, Some("judge"), at, now),
        Some(tick.context)
    );
}

/// #9231: a pre-upgrade entry — written as `{"issue": N, …}` with no `role`
/// field — still deserializes and still joins as issue-keyed. An execution in
/// flight across the upgrade must not lose its join.
#[test]
fn a_pre_role_key_entry_still_deserializes_and_joins_on_its_issue() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let context = TraceContext::root(true);
    let started_at = Utc::now() - Duration::minutes(30);
    let legacy = serde_json::json!({
        "issue": 9231,
        "context": context,
        "started_at": started_at,
    });
    let dir = root.join(JOIN_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{}.json", context.trace_id.as_str())),
        serde_json::to_vec(&legacy).unwrap(),
    )
    .unwrap();

    let cwd = root.to_string_lossy().into_owned();
    let now = Utc::now();
    let decoded: JoinEntry =
        serde_json::from_value(legacy).expect("a pre-#9231 entry still decodes");
    assert_eq!(decoded.issue, Some(9231));
    assert_eq!(decoded.role, None);
    assert_eq!(
        context_for_session_at(
            Some(&cwd),
            Some(9231),
            None,
            Some(started_at + Duration::minutes(1)),
            now
        ),
        Some(context)
    );
}

/// #9231: the precedence rule, stated once as a unit.
#[test]
fn a_sessions_join_key_prefers_its_issue_and_falls_back_to_its_slash_role() {
    use super::join::session_key;

    assert_eq!(session_key(Some(42), None), Some(JoinKey::Issue(42)));
    assert_eq!(session_key(Some(42), Some("judge")), Some(JoinKey::Issue(42)));
    assert_eq!(session_key(None, Some("judge")), Some(JoinKey::Role("judge")));
    assert_eq!(session_key(None, None), None);
}
