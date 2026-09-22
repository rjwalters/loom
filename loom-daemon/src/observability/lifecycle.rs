//! Content-free lifecycle spans at Loom-owned boundaries.
use crate::telemetry::{
    trace::{
        journal::{ActiveSpan, Journal},
        store::{TraceStore, CONTEXT_FILE_ENV, TRACEPARENT_ENV},
        SpanName, SpanStatus, TraceAttributes, TraceContext,
    },
    TelemetryEnvelope, TelemetryRecord,
};
use chrono::Utc;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

pub struct Span {
    journal: Journal,
    active: ActiveSpan,
}
impl Span {
    pub fn child(&self, name: SpanName, attributes: TraceAttributes) -> Option<Self> {
        self.journal
            .start(
                self.active.record.context.child(),
                Some(&self.active.record.context),
                name,
                Utc::now(),
                attributes,
            )
            .ok()
            .map(|active| Self {
                journal: self.journal.clone(),
                active,
            })
    }
    pub fn context(&self) -> &TraceContext {
        &self.active.record.context
    }
    pub fn command(&self, command: &mut Command) {
        command.env(TRACEPARENT_ENV, self.context().traceparent());
        command.env(CONTEXT_FILE_ENV, self.journal.path().with_extension("json"));
    }
    pub fn finish(&self, result: &str, status: SpanStatus) {
        self.finish_attributes(status, attributes(&[("loom.result", result)]));
    }
    /// A bash tool may launch several commands. Its final status does not prove
    /// every child's exit status, so only the observation time is propagated.
    pub fn finish_linked(&self) {
        let Ok(active) = self.journal.active() else {
            return;
        };
        let mut closed = std::collections::BTreeSet::new();
        for span in &active {
            if span
                .record
                .links
                .iter()
                .any(|link| link.context == *self.context())
            {
                closed.insert(span.record.context.span_id.as_str().to_owned());
            }
        }
        loop {
            let before = closed.len();
            for span in &active {
                if span
                    .record
                    .parent_span_id
                    .as_ref()
                    .is_some_and(|id| closed.contains(id.as_str()))
                {
                    closed.insert(span.record.context.span_id.as_str().to_owned());
                }
            }
            if before == closed.len() {
                break;
            }
        }
        for span in active {
            if closed.contains(span.record.context.span_id.as_str())
                && !matches!(span.record.name, SpanName::Phase | SpanName::RoleAttempt)
            {
                let _ = self.journal.finish(
                    &span,
                    Utc::now(),
                    SpanStatus::Unset,
                    attributes(&[
                        ("loom.result", "exit_unobserved"),
                        ("loom.timing_source", "launcher_return_observed"),
                    ]),
                );
            }
        }
    }
    pub fn finish_attempt(&self, result: &str, status: SpanStatus) {
        self.finish(result, status);
        if let Ok(active) = self.journal.active() {
            for phase in active {
                if phase.record.name == SpanName::Phase
                    && self.active.record.parent_span_id.as_ref()
                        == Some(&phase.record.context.span_id)
                {
                    let _ = self.journal.finish(
                        &phase,
                        Utc::now(),
                        status,
                        attributes(&[("loom.result", result)]),
                    );
                }
            }
        }
    }
    pub fn finish_attributes(&self, status: SpanStatus, attributes: TraceAttributes) {
        if self
            .journal
            .finish(&self.active, Utc::now(), status, attributes)
            .is_err()
        {
            log::warn!("observability: trace completion retained for recovery");
        }
    }
}

pub fn attributes(values: &[(&str, &str)]) -> TraceAttributes {
    values
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

pub fn begin(
    root: &Path,
    execution: &str,
    name: SpanName,
    attributes: TraceAttributes,
) -> Option<Span> {
    if !super::tracing::enabled(root) {
        return None;
    }
    let store = TraceStore::new(root);
    let saved = store.load_or_create(root, execution).ok()?;
    let journal = Journal::for_context(&store.path(root, execution));
    let active = journal
        .start(saved.context, None, name, saved.started_at, attributes)
        .ok()?;
    Some(Span { journal, active })
}

/// Context file must belong to this workspace and agree with inherited W3C IDs.
/// An unrelated shell's ambient traceparent cannot create a causal relationship.
pub fn inherited(root: &Path, name: SpanName, attributes: TraceAttributes) -> Option<Span> {
    if !super::tracing::enabled(root) {
        return None;
    }
    let (journal, _, parent) = inherited_context(root)?;
    let active = journal
        .start(parent.child(), Some(&parent), name, Utc::now(), attributes)
        .ok()?;
    Some(Span { journal, active })
}

fn inherited_context(root: &Path) -> Option<(Journal, TraceContext, TraceContext)> {
    let path = PathBuf::from(std::env::var_os(CONTEXT_FILE_ENV)?)
        .canonicalize()
        .ok()?;
    let directory = root.join(".loom/logs/trace-context").canonicalize().ok()?;
    if path.parent()? != directory {
        return None;
    }
    let saved = TraceStore::load(&path).ok()?;
    let parent = TraceContext::parse(&std::env::var(TRACEPARENT_ENV).ok()?).ok()?;
    if saved.context.trace_id != parent.trace_id {
        return None;
    }
    let journal = Journal::for_context(&path);
    Some((journal, saved.context, parent))
}

/// Semantic phase parenting is separate from the launcher tool's causal link.
pub fn worker_attempt(root: &Path, role: &str) -> Option<Span> {
    if !super::tracing::enabled(root)
        || !matches!(role, "curator" | "builder" | "judge" | "doctor" | "merge")
    {
        return None;
    }
    let (journal, root_context, launcher) = inherited_context(root)?;
    if let Some(active) = journal
        .active()
        .ok()?
        .into_iter()
        .find(|a| a.record.context == root_context && a.record.name == SpanName::RoleAttempt)
    {
        return Some(Span { journal, active });
    }
    let metadata = attributes(&[
        ("loom.role", role),
        ("loom.phase", role),
        ("loom.timing_source", "owned_boundary"),
    ]);
    let phase = journal
        .start_linked(
            root_context.child(),
            Some(&root_context),
            SpanName::Phase,
            Utc::now(),
            metadata.clone(),
            vec![crate::telemetry::trace::SpanLink { context: launcher }],
        )
        .ok()?;
    Span {
        journal,
        active: phase,
    }
    .child(SpanName::RoleAttempt, metadata)
}

pub fn checkpoint_completed(
    root: &Path,
    issue: u32,
    phase: &str,
    attempt: Option<u32>,
    model: Option<&str>,
    pr: Option<u32>,
) {
    let primary = checkpoint_workspace(root);
    let root = primary.as_path();
    if !super::tracing::enabled(root) {
        return;
    }
    let Some((journal, root_context, launcher)) = inherited_context(root) else {
        return;
    };
    checkpoint_observation(
        &journal,
        &root_context,
        Some(launcher),
        issue,
        phase,
        attempt,
        model,
        pr,
        "checkpoint_write_observed",
    );
}

fn checkpoint_workspace(root: &Path) -> PathBuf {
    let Some(candidate) =
        std::env::var_os("LOOM_WORKSPACE").and_then(|p| PathBuf::from(p).canonicalize().ok())
    else {
        return root.to_owned();
    };
    if root.canonicalize().ok().as_ref() == Some(&candidate) {
        return candidate;
    }
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    if let Ok(crate::proc_exec::Completion::Exited(output)) =
        crate::proc_exec::run_bounded(command, std::time::Duration::from_secs(2))
    {
        if output.status.success() {
            if let Ok(path) = std::str::from_utf8(&output.stdout) {
                if Path::new(path.trim())
                    .parent()
                    .and_then(|p| p.canonicalize().ok())
                    .as_ref()
                    == Some(&candidate)
                {
                    return candidate;
                }
            }
        }
    }
    root.to_owned()
}

#[allow(clippy::too_many_arguments)]
fn checkpoint_observation(
    journal: &Journal,
    root: &TraceContext,
    launcher: Option<TraceContext>,
    issue: u32,
    phase: &str,
    attempt: Option<u32>,
    model: Option<&str>,
    pr: Option<u32>,
    source: &str,
) {
    let role = phase
        .strip_suffix("-done")
        .or_else(|| phase.strip_suffix("-rejected"));
    let Some(role @ ("curator" | "builder" | "judge" | "doctor" | "merge")) = role else {
        return;
    };
    let result = if phase == "judge-rejected" {
        "rejected"
    } else {
        "success"
    };
    let status = if result == "success" {
        SpanStatus::Ok
    } else {
        SpanStatus::Error
    };
    let mut metadata = attributes(&[
        ("loom.phase", role),
        ("loom.role", role),
        ("loom.issue", &issue.to_string()),
        ("loom.timing_source", source),
        ("loom.result", result),
    ]);
    if role == "judge" {
        metadata.insert(
            "loom.judge_verdict".into(),
            if result == "success" {
                "approved"
            } else {
                "rejected"
            }
            .into(),
        );
    }
    if let Some(attempt) = attempt {
        metadata.insert("loom.attempt".into(), attempt.to_string());
    }
    if let Some(model) = model {
        metadata.insert("loom.configured_model".into(), model.into());
    }
    if let Some(pr) = pr {
        metadata.insert("loom.pr_number".into(), pr.to_string());
    }
    let at = Utc::now();
    // An explicitly launched role already has an authoritative start. Its
    // checkpoint completes that same attempt; never fabricate a second one.
    if let Ok(active) = journal.active() {
        if let Some(attempt_span) = active
            .iter()
            .filter(|a| {
                a.record.name == SpanName::RoleAttempt
                    && a.record
                        .attributes
                        .get("loom.role")
                        .is_some_and(|v| v == role)
            })
            .max_by_key(|a| a.record.started_at)
        {
            metadata
                .insert("loom.timing_source".into(), "owned_start_checkpoint_completion".into());
            let _ = journal.finish(attempt_span, at, status, metadata.clone());
            if let Some(phase_span) = active.iter().find(|p| {
                p.record.name == SpanName::Phase
                    && attempt_span.record.parent_span_id.as_ref()
                        == Some(&p.record.context.span_id)
            }) {
                let _ = journal.finish(phase_span, at, status, metadata);
            }
            return;
        }
    }
    let links = launcher
        .map(|context| crate::telemetry::trace::SpanLink { context })
        .into_iter()
        .collect();
    let Ok(phase) = journal.start_linked(
        root.child(),
        Some(root),
        SpanName::Phase,
        at,
        metadata.clone(),
        links,
    ) else {
        return;
    };
    if let Ok(role) = journal.start(
        phase.record.context.child(),
        Some(&phase.record.context),
        SpanName::RoleAttempt,
        at,
        metadata.clone(),
    ) {
        let _ = journal.finish(&role, at, status, metadata.clone());
    }
    let _ = journal.finish(&phase, at, status, metadata);
}

pub fn phase_transition(root: &Path, execution: &str, phase: &str, issue: u32, pr: Option<u32>) {
    if !super::tracing::enabled(root) {
        return;
    }
    let store = TraceStore::new(root);
    let Ok(saved) = TraceStore::load(&store.path(root, execution)) else {
        return;
    };
    let journal = Journal::for_context(&store.path(root, execution));
    // Native writes have already journalled the completion, including attempts
    // that could occur between polls. Polling is only a legacy fallback.
    if journal.has_checkpoint_observations().unwrap_or(true) {
        return;
    }
    checkpoint_observation(
        &journal,
        &saved.context,
        None,
        issue,
        phase,
        None,
        None,
        pr,
        "checkpoint_poll_observed",
    );
}

pub fn prepare_execution(command: &mut Command, root: &Path, execution: &str) {
    if let Some(span) = begin(
        root,
        execution,
        SpanName::Sweep,
        attributes(&[
            ("loom.sweep_id", execution),
            ("loom.timing_source", "dispatch_observed"),
        ]),
    ) {
        span.command(command);
    }
}

/// Reaper closes persisted unfinished work at the time its termination is seen.
/// Child exit details unavailable after exec/restart remain explicitly unknown.
pub fn finish_execution(
    root: &Path,
    execution: &str,
    result: &str,
    metadata: TraceAttributes,
) -> Option<TraceContext> {
    if !super::tracing::enabled(root) {
        return None;
    }
    let store = TraceStore::new(root);
    let saved = TraceStore::load(&store.path(root, execution)).ok()?;
    let journal = Journal::for_context(&store.path(root, execution));
    for active in journal.active().ok()? {
        let root_span = active.record.context == saved.context;
        let observed = root_span;
        let mut attrs = if root_span {
            metadata.clone()
        } else {
            TraceAttributes::new()
        };
        attrs
            .insert("loom.result".into(), if observed { result } else { "exit_unobserved" }.into());
        if !observed {
            attrs.insert("loom.timing_source".into(), "terminal_observed".into());
        }
        let status = if !observed {
            SpanStatus::Unset
        } else if result == "success" {
            SpanStatus::Ok
        } else {
            SpanStatus::Error
        };
        if journal.finish(&active, Utc::now(), status, attrs).is_err() {
            return None;
        }
    }
    Some(saved.context)
}

pub fn spawn_child(
    command: &mut Command,
    root: &Path,
    execution: &str,
) -> std::io::Result<std::process::Child> {
    match command.spawn() {
        Ok(child) => {
            child_spawned(root, execution, child.id());
            Ok(child)
        }
        Err(error) => {
            finish_execution(root, execution, "spawn_failed", Default::default());
            Err(error)
        }
    }
}

pub fn child_spawned(root: &Path, execution: &str, pid: u32) {
    let store = TraceStore::new(root);
    if let Ok(saved) = TraceStore::load(&store.path(root, execution)) {
        let _ = Journal::for_context(&store.path(root, execution)).set_owner(&saved.context, pid);
    }
}

/// Called only when a surviving execution is adopted under its authoritative ID.
pub fn execution_adopted(root: &Path, execution: &str) {
    let store = TraceStore::new(root);
    if let Ok(saved) = TraceStore::load(&store.path(root, execution)) {
        let journal = Journal::for_context(&store.path(root, execution));
        if journal
            .set_supervisor(&saved.context, std::process::id())
            .is_err()
        {
            log::warn!("observability: adopted trace supervisor could not be persisted");
        }
    }
}

pub fn child_exited(root: &Path, execution: &str, result: &str) {
    let store = TraceStore::new(root);
    if let Ok(saved) = TraceStore::load(&store.path(root, execution)) {
        finish_owned_runtime(
            &Journal::for_context(&store.path(root, execution)),
            &saved.context,
            result,
        );
    }
}

fn finish_owned_runtime(journal: &Journal, root: &TraceContext, result: &str) {
    let Ok(active) = journal.active() else {
        return;
    };
    let owner = active
        .iter()
        .find(|s| s.record.context == *root)
        .map(|s| s.owner_pid);
    for span in active {
        if span.record.name == SpanName::RuntimeRun
            && owner.is_some_and(|pid| pid > 0 && pid == span.owner_pid)
        {
            let status = if result == "success" {
                SpanStatus::Ok
            } else {
                SpanStatus::Error
            };
            let _ = journal.finish(
                &span,
                Utc::now(),
                status,
                attributes(&[
                    ("loom.result", result),
                    ("loom.timing_source", "child_exit_observed"),
                ]),
            );
        }
    }
}

pub fn role_child_exited(result: &str) {
    ROLE_CONTEXT.with(|slot| {
        if let Some(span) = slot.borrow().as_ref() {
            finish_owned_runtime(&span.journal, span.context(), result);
        }
    });
}

pub fn role_child_spawned(pid: u32) {
    ROLE_CONTEXT.with(|slot| {
        if let Some(span) = slot.borrow().as_ref() {
            let _ = span.journal.set_owner(span.context(), pid);
        }
    });
}

#[cfg(unix)]
fn process_gone(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal zero only probes existence; positive PID cannot address a process group.
    unsafe {
        libc::kill(pid, 0) == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }
}
#[cfg(not(unix))]
fn process_gone(_: u32) -> bool {
    false
}

fn identity_gone(pid: u32, observed_at: Option<chrono::DateTime<Utc>>) -> bool {
    if process_gone(pid) {
        return true;
    }
    if pid == 0 || i32::try_from(pid).is_err() {
        return false;
    }
    observed_at.is_some_and(|observed| {
        crate::sweep_registry::pid_identity::pid_was_recycled(
            crate::sweep_registry::pid_identity::pid_start_wallclock(pid),
            observed,
        )
    })
}

fn recover_orphans(journal: &Journal) {
    // A worker exit precedes authoritative reaping/verification. The live
    // supervisor still owns that gap, even when every worker PID is gone.
    let _ = journal.finish_abandoned(
        |span| {
            identity_gone(span.owner_pid, span.owner_observed_at)
                && span
                    .supervisor_pid
                    .is_some_and(|pid| identity_gone(pid, span.supervisor_observed_at))
        },
        attributes(&[
            ("loom.result", "process_lost"),
            ("loom.recovered", "true"),
            ("loom.timing_source", "recovery_observed"),
        ]),
    );
}

pub fn backfill(root: &Path, queue: &super::queue::DurableQueue) -> usize {
    if !super::tracing::enabled(root) {
        return 0;
    }
    let Ok(entries) = std::fs::read_dir(root.join(".loom/logs/trace-context")) else {
        return 0;
    };
    let mut count = 0;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let journal = Journal::from_path(path);
        recover_orphans(&journal);
        match journal.drain(|span| {
            let context = span.context.clone();
            let mut envelope = TelemetryEnvelope::new(
                crate::sweep_registry::host_identity(),
                TelemetryRecord::Span(span),
            );
            envelope.trace_context = Some(context);
            queue.push_durable(envelope)?;
            Ok(())
        }) {
            Ok(n) => {
                count += n;
                if journal.retire_if_drained().is_err() {
                    log::warn!("observability: completed trace journal retained");
                }
            }
            Err(_) => log::warn!("observability: trace journal remains pending for retry"),
        }
    }
    count
}

thread_local! {
    static ROLE_CONTEXT: std::cell::RefCell<Option<Span>> = const { std::cell::RefCell::new(None) };
}

pub fn role_command(command: &mut Command) {
    ROLE_CONTEXT.with(|slot| {
        if let Some(span) = slot.borrow().as_ref() {
            span.command(command);
        }
    });
}

pub fn role_invocation(
    root: &Path,
    role: &str,
    invoke: impl FnOnce() -> crate::role_runner::RoleTickOutcome,
) -> (crate::role_runner::RoleTickOutcome, Option<TraceContext>) {
    let execution = format!("role-{}", uuid::Uuid::new_v4());
    let span = begin(
        root,
        &execution,
        SpanName::RoleAttempt,
        attributes(&[
            ("loom.role", role),
            ("loom.timing_source", "owned_boundary"),
        ]),
    );
    let context = span.as_ref().map(|s| s.context().clone());
    ROLE_CONTEXT.with(|slot| *slot.borrow_mut() = span);
    let outcome = invoke();
    ROLE_CONTEXT.with(|slot| {
        slot.borrow_mut().take();
    });
    let (result, _) = crate::role_tick_telemetry::classify(&outcome);
    let result = serde_json::to_value(result)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into());
    finish_execution(root, &execution, &result, Default::default());
    (outcome, context)
}

#[cfg(test)]
mod tests;
