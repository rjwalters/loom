//! `loom-daemon usage-record`: one role attempt's token usage, journalled
//! into the issue's story trace (Issue #9303).
//!
//! In-session sweeps (an operator's `/loom:sweep`, hand-driven role
//! subagents) never reach the daemon's terminal transition, so no execution
//! usage span is ever written for them — and without an inherited trace
//! context their checkpoint writes journal no `loom.role_attempt` either. The
//! sweep prompt therefore runs `usage-record` after each checkpoint write, with
//! the role subagent's `agent-id`. This:
//!
//! 1. finds that subagent's transcript
//!    (`<claude projects>/<slug(workspace)>/*/subagents/agent-<id>.jsonl`, or
//!    `--transcript`) and reads it with the same deduped reader the execution
//!    spans use ([`sum_transcript_usage_by_model`]), so Σattempt and the
//!    execution total count messages identically;
//! 2. picks the attempt span to parent to:
//!    - **inherited context** (a daemon-spawned `claude -p` child): the newest
//!      completed `loom.role_attempt` for the role in that execution's journal
//!      — the one its checkpoint write just finished — else a new attempt span
//!      under the execution root;
//!    - **no context** (an operator session): the issue's D32 story
//!      ([`crate::observability::tracing::resolve_story`]); a completed
//!      `loom.role_attempt` is journalled under the story root, keyed by
//!      `(run id, role, attempt, agent id)` and spanning the transcript's
//!      first..last timestamps;
//! 3. journals one `loom.runtime.usage` span per model under it with
//!    `loom.usage.scope=attempt` ([`super::spans`]).
//!
//! The journal lives under `.loom/logs/trace-context/` and is drained by the
//! daemon's next backfill pass, so no running daemon is needed now. **Fail
//! open**: nothing here can fail a sweep — every problem is an
//! [`Outcome`] the CLI prints and exits 0 on. Unknown usage (no transcript, no
//! usage blocks) journals nothing.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use super::cost::Pricing;
use super::spans::{append_new, model_usage_spans, UsageScope};
use crate::observability::tracing::StoryRef;
use crate::script_helpers::sweep_experiment::{sum_transcript_usage_by_model, ModelUsageTotals};
use crate::script_helpers::transcript_usage::UsageFold;
use crate::telemetry::trace::journal::Journal;
use crate::telemetry::trace::store::TraceStore;
use crate::telemetry::trace::{
    SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext, STORY_KEY_VERSION,
};

/// What to record.
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub issue: u32,
    pub role: String,
    pub attempt: Option<u32>,
    /// The role subagent's `agent-id` (Task-result metadata).
    pub agent_id: Option<String>,
    /// The in-session sweep's `RUN_ID` (`--task-id`).
    pub task_id: Option<String>,
    /// An explicit transcript, instead of resolving `agent_id`.
    pub transcript: Option<PathBuf>,
}

/// What happened. Every variant exits 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Spans journalled (count), or 0 when all were already there.
    Recorded(usize),
    /// Nothing to do, and why (tracing off, no transcript, no usage, no story).
    Skipped(&'static str),
    /// Something failed; the sweep carries on.
    Failed(String),
}

/// Where the attempt span goes.
pub enum Parent {
    /// A daemon child's execution journal and its root context.
    Inherited {
        journal: Journal,
        root: TraceContext,
    },
    /// An operator session: the issue's story.
    Story(StoryRef),
}

/// The CLI entry: resolve everything from the environment, never fail.
#[must_use]
pub fn record(cwd: &Path, request: &Request) -> Outcome {
    let workspace = primary_workspace(cwd);
    if !crate::observability::tracing::enabled(&workspace) {
        return Outcome::Skipped("tracing is not enabled for this workspace");
    }
    let Some(transcript) = request.transcript.clone().or_else(|| {
        let id = request.agent_id.as_deref()?;
        let projects = crate::transcript_tokens::claude_projects_dir()?;
        find_agent_transcript(&projects, &[workspace.as_path(), cwd], id)
    }) else {
        return Outcome::Skipped("no transcript for this agent id");
    };
    let parent = match crate::observability::lifecycle::inherited_context(&workspace) {
        Some((journal, root, _launcher)) => Parent::Inherited { journal, root },
        None => match crate::observability::tracing::resolve_story(&workspace, request.issue) {
            Some(story) => Parent::Story(story),
            None => return Outcome::Skipped("the issue's story trace is unresolvable"),
        },
    };
    record_transcript(&workspace, request, &transcript, parent, &Pricing::active())
}

/// [`record`] with the transcript and parent already resolved.
#[must_use]
pub fn record_transcript(
    workspace: &Path,
    request: &Request,
    transcript: &Path,
    parent: Parent,
    pricing: &Pricing<'_>,
) -> Outcome {
    if std::fs::metadata(transcript)
        .is_ok_and(|m| m.len() > crate::transcript_tokens::MAX_TRANSCRIPT_BYTES)
    {
        return Outcome::Skipped("transcript is oversized");
    }
    let rows = sum_transcript_usage_by_model(transcript);
    if rows.is_empty() {
        return Outcome::Skipped("the transcript carries no usage");
    }
    let window = transcript_window(transcript);
    match journal_attempt(workspace, request, &rows, window, parent, pricing) {
        Ok(n) => Outcome::Recorded(n),
        Err(error) => {
            log::warn!("usage-record: attempt usage not journalled: {error}");
            Outcome::Failed(error.to_string())
        }
    }
}

/// Journal the attempt (when it has to be created) and its per-model usage.
fn journal_attempt(
    workspace: &Path,
    request: &Request,
    rows: &[ModelUsageTotals],
    window: (DateTime<Utc>, DateTime<Utc>),
    parent: Parent,
    pricing: &Pricing<'_>,
) -> anyhow::Result<usize> {
    let mut common = TraceAttributes::new();
    common.insert("loom.role".into(), request.role.clone());
    common.insert("loom.issue".into(), request.issue.to_string());
    if let Some(attempt) = request.attempt {
        common.insert("loom.attempt".into(), attempt.to_string());
    }
    let (journal, attempt, created) = match parent {
        Parent::Inherited { journal, root } => {
            let sweep_id = sweep_id_of(&journal, &root).or_else(|| request.task_id.clone());
            if let Some(id) = &sweep_id {
                common.insert("loom.sweep_id".into(), id.clone());
            }
            match newest_attempt(&journal, &root, &request.role)? {
                Some(existing) => (journal, existing.context, None),
                None => {
                    let span = attempt_span(&root, request, sweep_id.as_deref(), window, &common);
                    (journal, span.context.clone(), Some(span))
                }
            }
        }
        Parent::Story(story) => {
            let sweep_id = request
                .task_id
                .clone()
                .unwrap_or_else(|| format!("insession-issue-{}", request.issue));
            common.insert("loom.sweep_id".into(), sweep_id.clone());
            let execution = format!("insession-{sweep_id}");
            let store = TraceStore::new(workspace);
            store.load_or_create_story(workspace, &execution, Some(&story.context))?;
            let journal = Journal::for_context(&store.path(workspace, &execution));
            let mut attributes = common.clone();
            attributes.insert("loom.repo".into(), story.repo.clone());
            attributes.insert("loom.story".into(), story.story.clone());
            attributes.insert("loom.story.key_version".into(), STORY_KEY_VERSION.into());
            let span = attempt_span(&story.context, request, Some(&sweep_id), window, &attributes);
            (journal, span.context.clone(), Some(span))
        }
    };
    let mut spans: Vec<SpanRecord> = created.into_iter().collect();
    spans.extend(model_usage_spans(&attempt, window, rows, UsageScope::Attempt, &common, pricing));
    Ok(append_new(&journal, spans)?.len())
}

/// A completed `loom.role_attempt` under `parent`, keyed by everything that
/// names this attempt so a re-run yields the same id.
fn attempt_span(
    parent: &TraceContext,
    request: &Request,
    sweep_id: Option<&str>,
    window: (DateTime<Utc>, DateTime<Utc>),
    common: &TraceAttributes,
) -> SpanRecord {
    let attempt = request.attempt.map(|a| a.to_string()).unwrap_or_default();
    let context = parent.derived_child(&[
        SpanName::RoleAttempt.as_str(),
        sweep_id.unwrap_or(""),
        &request.role,
        &attempt,
        request.agent_id.as_deref().unwrap_or(""),
    ]);
    let mut attributes = common.clone();
    attributes.insert("loom.phase".into(), request.role.clone());
    attributes.insert("loom.timing_source".into(), "transcript_window".into());
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    SpanRecord {
        context,
        parent_span_id: Some(parent.span_id.clone()),
        name: SpanName::RoleAttempt,
        started_at: window.0,
        ended_at: window.1.max(window.0),
        status: SpanStatus::Unset,
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    }
    .bounded()
}

/// The newest completed `loom.role_attempt` for `role` in `root`'s trace.
fn newest_attempt(
    journal: &Journal,
    root: &TraceContext,
    role: &str,
) -> anyhow::Result<Option<SpanRecord>> {
    Ok(journal
        .completed()?
        .into_iter()
        .filter(|span| {
            span.name == SpanName::RoleAttempt
                && span.context.trace_id == root.trace_id
                && span.attributes.get("loom.role").is_some_and(|r| r == role)
        })
        .max_by_key(|span| span.ended_at))
}

/// The execution's `loom.sweep_id`, read off its root span.
fn sweep_id_of(journal: &Journal, root: &TraceContext) -> Option<String> {
    let from = |attributes: &TraceAttributes| attributes.get("loom.sweep_id").cloned();
    journal
        .active()
        .ok()?
        .into_iter()
        .find(|span| span.record.context == *root)
        .and_then(|span| from(&span.record.attributes))
        .or_else(|| {
            journal
                .completed()
                .ok()?
                .into_iter()
                .find(|span| span.context == *root)
                .and_then(|span| from(&span.attributes))
        })
}

/// The transcript's first..last usage timestamps, else now..now.
fn transcript_window(path: &Path) -> (DateTime<Utc>, DateTime<Utc>) {
    let now = Utc::now();
    let mut fold = UsageFold::default();
    if let Ok(text) = std::fs::read_to_string(path) {
        fold.add_text(&text);
    }
    let parse = |ts: Option<&String>| {
        ts.and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc))
    };
    match (parse(fold.first_timestamp.as_ref()), parse(fold.last_timestamp.as_ref())) {
        (Some(first), Some(last)) => (first.min(last), last.max(first)),
        _ => (now, now),
    }
}

/// `agent-<id>.jsonl` under any session of the project dirs for `roots`, the
/// newest when several match. `None` for an id that is not a plain token.
#[must_use]
pub fn find_agent_transcript(projects: &Path, roots: &[&Path], agent_id: &str) -> Option<PathBuf> {
    let id = agent_id.strip_prefix("agent-").unwrap_or(agent_id);
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return None;
    }
    let file = format!("agent-{id}.jsonl");
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for root in roots {
        let project = projects.join(crate::transcript_tokens::project_slug(root));
        if !seen.insert(project.clone()) {
            continue;
        }
        let Ok(sessions) = std::fs::read_dir(&project) else {
            continue;
        };
        for session in sessions.flatten() {
            let candidate = session.path().join("subagents").join(&file);
            if let Ok(meta) = std::fs::metadata(&candidate) {
                let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                found.push((modified, candidate));
            }
        }
    }
    found
        .into_iter()
        .max_by_key(|(at, _)| *at)
        .map(|(_, path)| path)
}

/// The primary checkout for `cwd`: `LOOM_WORKSPACE` when it is this checkout's
/// own, else the parent of git's common dir (so an issue worktree maps to its
/// primary clone), else `cwd`.
fn primary_workspace(cwd: &Path) -> PathBuf {
    let checked = crate::observability::lifecycle::checkpoint_workspace(cwd);
    if checked != cwd {
        return checked;
    }
    let mut command = std::process::Command::new("git");
    command
        .current_dir(cwd)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    if let Ok(crate::proc_exec::Completion::Exited(output)) =
        crate::proc_exec::run_bounded(command, std::time::Duration::from_secs(2))
    {
        if output.status.success() {
            if let Some(parent) = std::str::from_utf8(&output.stdout)
                .ok()
                .and_then(|p| Path::new(p.trim()).parent().map(Path::to_path_buf))
            {
                return parent;
            }
        }
    }
    cwd.to_path_buf()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "record_tests.rs"]
mod tests;
