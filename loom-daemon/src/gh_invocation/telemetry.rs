//! Client-side telemetry for the `gh` facade (#9985 slice 3): one
//! `invoke github` span per invocation, plus a local completion record for
//! every invocation that did not succeed.
//!
//! # The span
//!
//! [`InvocationSpan::open`] fixes the invocation's own [`TraceContext`] before
//! the child is spawned, with IDs derived per `trace-identity.md` — never
//! random:
//!
//! - **With a parent** (an explicit [`ParentContext::Parent`], or the ambient
//!   `LOOM_TRACEPARENT` a sweep exports to its children): a child of that
//!   span, keyed by the span name, operation, start instant and
//!   `github.invocation`.
//! - **Without one** (an ad-hoc daemon tick): its own root, tag
//!   `loom.github.invoke.*`, keyed by the same facts. `context_source=missing`.
//!
//! `github.invocation` is `<pid>.<seq>` — the process id and a process-local
//! counter — so two invocations that start in the same clock tick under the
//! same parent never share a span ID. Every derivation input is an attribute
//! of the span, so any ID can be recomputed from the span's own data.
//!
//! The span is delivered by whichever path this process has: the daemon's
//! global ops sink (the OTLP exporters' queue), else — for a CLI process
//! spawned inside a traced sweep — that execution's trace journal
//! (`LOOM_TRACE_CONTEXT_FILE`), which the daemon drains and exports. With
//! neither, nothing is exported and the child is handed the caller's own
//! context rather than one naming a span that will never exist.
//!
//! # The local completion record
//!
//! Failures that never reach a gateway (spawn failure, timeout, a non-zero
//! exit, a launcher routing refusal) must still leave evidence on the host.
//! Each non-`ok` outcome appends one JSON line to a per-host, owner-only,
//! hourly-rotated sink: `${TMPDIR:-/tmp}/loom-gh-invocations/failures-<hour>.jsonl`
//! (`LOOM_GH_INVOCATION_RECORD_DIR` overrides; `off`/`0` disables). Recording
//! never fails, blocks on, or retries the invocation itself.

use super::{GhInvocation, ParentContext};
use crate::proc_exec::{Completion, ExecError};
use crate::telemetry::trace::journal::Journal;
use crate::telemetry::trace::{
    instant, SpanId, SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Span attribute keys an `invoke github` span may carry, in addition to the
/// lifecycle allowlist in `trace::span::bounded_attributes`. The gateway
/// collector's span `keep_keys` must include every key (contract-tested).
pub const SPAN_ATTRIBUTE_KEYS: &[&str] = &[
    "github.operation",
    "github.access_intent",
    "github.target",
    "github.outcome",
    "github.exit_code",
    "github.invocation",
    "github.launcher",
    "context_source",
];

/// The stderr marker the managed launcher (C4, #9987) prints when it refuses
/// to route a request (`routing.denied`). Only a captured run can see it; a
/// passthrough refusal is recorded as [`Outcome::ExitNonzero`].
pub const ROUTING_REFUSAL_MARKER: &str = "routing.denied";

/// Hours of failure records kept on the host.
const RETAIN_HOURS: i64 = 24;

/// How one invocation ended — the closed `github.outcome` vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Ran to completion and exited zero.
    Ok,
    /// Ran to completion and exited non-zero.
    ExitNonzero,
    /// Killed by a signal it was not sent by the facade.
    Signaled,
    /// The facade's deadline fired; the process group was killed.
    Timeout,
    /// `gh` could not be started. Nothing ran.
    SpawnFailed,
    /// `gh` started but its result could not be collected; it may have run.
    CollectFailed,
    /// The managed launcher refused to route the request.
    RoutingRefused,
}

impl Outcome {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::ExitNonzero => "exit_nonzero",
            Outcome::Signaled => "signaled",
            Outcome::Timeout => "timeout",
            Outcome::SpawnFailed => "spawn_failed",
            Outcome::CollectFailed => "collect_failed",
            Outcome::RoutingRefused => "routing_refused",
        }
    }

    /// Every value, for vocabulary tests and docs.
    pub const ALL: [Outcome; 7] = [
        Outcome::Ok,
        Outcome::ExitNonzero,
        Outcome::Signaled,
        Outcome::Timeout,
        Outcome::SpawnFailed,
        Outcome::CollectFailed,
        Outcome::RoutingRefused,
    ];
}

/// The `(outcome, exit code)` of a captured run.
#[must_use]
pub fn classify_captured(result: &Result<Completion, ExecError>) -> (Outcome, Option<i32>) {
    match result {
        Ok(Completion::Exited(out)) => {
            let refused = !out.status.success()
                && String::from_utf8_lossy(&out.stderr).contains(ROUTING_REFUSAL_MARKER);
            classify_status(out.status, refused)
        }
        Ok(Completion::TimedOut { .. }) => (Outcome::Timeout, None),
        Err(e) => (exec_error_outcome(e), None),
    }
}

/// The `(outcome, exit code)` of a passthrough run (stderr is not ours to read).
#[must_use]
pub fn classify_passthrough(
    result: &Result<std::process::ExitStatus, ExecError>,
) -> (Outcome, Option<i32>) {
    match result {
        Ok(status) => classify_status(*status, false),
        Err(e) => (exec_error_outcome(e), None),
    }
}

fn classify_status(status: std::process::ExitStatus, refused: bool) -> (Outcome, Option<i32>) {
    match status.code() {
        Some(0) => (Outcome::Ok, Some(0)),
        Some(code) if refused => (Outcome::RoutingRefused, Some(code)),
        Some(code) => (Outcome::ExitNonzero, Some(code)),
        None => (Outcome::Signaled, None),
    }
}

fn exec_error_outcome(error: &ExecError) -> Outcome {
    match error {
        ExecError::Spawn(_) => Outcome::SpawnFailed,
        ExecError::Collect(_) => Outcome::CollectFailed,
    }
}

/// Where a finished span goes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Delivery {
    /// The daemon's global ops sink (or a test capture).
    OpsSink,
    /// The ambient execution's trace journal.
    Journal(PathBuf),
    /// Not exported.
    None,
}

fn delivery(parent: &ParentContext) -> Delivery {
    if crate::observability::ops::spans_exported() {
        return Delivery::OpsSink;
    }
    // A journal only accepts spans of its own execution's trace.
    let (ambient, journal) = ambient();
    match (parent, ambient, journal) {
        (ParentContext::Parent(ctx), Some(ambient), Some(file)) if *ctx == ambient => {
            Delivery::Journal(file)
        }
        _ => Delivery::None,
    }
}

/// The ambient execution context: the validated `LOOM_TRACEPARENT` a traced
/// sweep exports to its children. Loom's own namespaced variable only — a
/// third-party `TRACEPARENT` is never adopted as a Loom parent.
#[must_use]
pub fn ambient_parent() -> Option<TraceContext> {
    ambient().0
}

/// `(LOOM_TRACEPARENT, LOOM_TRACE_CONTEXT_FILE)` of this process.
#[cfg(not(test))]
fn ambient() -> (Option<TraceContext>, Option<PathBuf>) {
    use crate::telemetry::trace::store::{CONTEXT_FILE_ENV, TRACEPARENT_ENV};
    parse_ambient(
        std::env::var(TRACEPARENT_ENV).ok().as_deref(),
        std::env::var_os(CONTEXT_FILE_ENV).map(PathBuf::from),
    )
}

/// Test builds never read the real environment — a `cargo test` run inside a
/// traced sweep must not journal fixture spans into that sweep's trace. A test
/// thread opts in with [`set_test_ambient`].
#[cfg(test)]
fn ambient() -> (Option<TraceContext>, Option<PathBuf>) {
    TEST_AMBIENT.with(|a| a.borrow().clone())
}

#[cfg(test)]
thread_local! {
    static TEST_AMBIENT: std::cell::RefCell<(Option<TraceContext>, Option<PathBuf>)> =
        const { std::cell::RefCell::new((None, None)) };
}

/// Set THIS test thread's ambient `(LOOM_TRACEPARENT, context file)`.
#[cfg(test)]
pub(crate) fn set_test_ambient(traceparent: Option<&str>, context_file: Option<PathBuf>) {
    let parsed = parse_ambient(traceparent, context_file);
    TEST_AMBIENT.with(|a| *a.borrow_mut() = parsed);
}

/// Validate a raw ambient pair: an unparseable traceparent is no parent, and
/// an empty context-file path is no journal.
#[must_use]
pub fn parse_ambient(
    traceparent: Option<&str>,
    context_file: Option<PathBuf>,
) -> (Option<TraceContext>, Option<PathBuf>) {
    (
        traceparent.and_then(|v| TraceContext::parse(v.trim()).ok()),
        context_file.filter(|p| !p.as_os_str().is_empty()),
    )
}

fn next_invocation_key() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!("{}.{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed))
}

/// One invocation's span, opened before spawn and finished after.
#[derive(Debug, Clone)]
pub struct InvocationSpan {
    pub context: TraceContext,
    pub parent_span_id: Option<SpanId>,
    pub started_at: DateTime<Utc>,
    pub invocation: String,
    delivery: Delivery,
}

impl InvocationSpan {
    /// Open the span for `inv`, starting now.
    #[must_use]
    pub fn open(inv: &GhInvocation) -> Self {
        Self::open_at(inv, Utc::now(), next_invocation_key())
    }

    /// [`Self::open`] with the start instant and invocation key supplied.
    #[must_use]
    pub fn open_at(inv: &GhInvocation, started_at: DateTime<Utc>, invocation: String) -> Self {
        let name = SpanName::GithubInvoke.as_str();
        let started = instant(started_at);
        let key = [
            name,
            inv.operation.as_str(),
            started.as_str(),
            invocation.as_str(),
        ];
        let (context, parent_span_id) = match &inv.parent {
            ParentContext::Parent(parent) => {
                (parent.derived_child(&key), Some(parent.span_id.clone()))
            }
            ParentContext::Missing => (TraceContext::derived("github.invoke", &key[1..]), None),
        };
        Self {
            context,
            parent_span_id,
            started_at,
            invocation,
            delivery: delivery(&inv.parent),
        }
    }

    /// The context to hand the child as its `traceparent`: this span's own
    /// when it will be exported, else the caller's parent (or none).
    #[must_use]
    pub fn child_context<'a>(&'a self, parent: &'a ParentContext) -> Option<&'a TraceContext> {
        if self.delivery == Delivery::None {
            match parent {
                ParentContext::Parent(ctx) => Some(ctx),
                ParentContext::Missing => None,
            }
        } else {
            Some(&self.context)
        }
    }

    /// The completed span record.
    #[must_use]
    pub fn record(
        &self,
        inv: &GhInvocation,
        launcher: super::GhBinSource,
        outcome: Outcome,
        exit_code: Option<i32>,
        ended_at: DateTime<Utc>,
    ) -> SpanRecord {
        let mut attributes: TraceAttributes = [
            ("github.operation", inv.operation.as_str().to_string()),
            ("github.access_intent", inv.intent.as_str().to_string()),
            ("github.target", inv.target.bounded()),
            ("github.outcome", outcome.as_str().to_string()),
            ("github.invocation", self.invocation.clone()),
            ("github.launcher", launcher_str(launcher).to_string()),
            ("context_source", inv.context_source().to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        if let Some(code) = exit_code {
            attributes.insert("github.exit_code".into(), code.to_string());
        }
        crate::telemetry::trace::provenance::stamp(&mut attributes);
        SpanRecord {
            context: self.context.clone(),
            parent_span_id: self.parent_span_id.clone(),
            name: SpanName::GithubInvoke,
            started_at: self.started_at,
            ended_at: ended_at.max(self.started_at),
            status: if outcome == Outcome::Ok {
                SpanStatus::Ok
            } else {
                SpanStatus::Error
            },
            attributes,
            events: Vec::new(),
            links: Vec::new(),
        }
    }

    /// Emit the span and, for a non-`ok` outcome, the local completion record.
    pub fn finish(
        self,
        inv: &GhInvocation,
        launcher: super::GhBinSource,
        outcome: Outcome,
        exit_code: Option<i32>,
    ) {
        let ended_at = Utc::now();
        let span = self.record(inv, launcher, outcome, exit_code, ended_at);
        if outcome != Outcome::Ok {
            record_failure(&FailureRecord::from_span(&span, inv, outcome, exit_code));
        }
        match &self.delivery {
            Delivery::OpsSink => crate::observability::ops::emit_span(span),
            Delivery::Journal(file) => {
                if let Err(e) = Journal::for_context(file).append_completed(span) {
                    log::debug!("gh_invocation: span not journalled: {e}");
                }
            }
            Delivery::None => {}
        }
    }
}

fn launcher_str(source: super::GhBinSource) -> &'static str {
    match source {
        super::GhBinSource::Policy => "policy",
        super::GhBinSource::EnvOverride => "env_override",
        super::GhBinSource::Path => "path",
        super::GhBinSource::Injected => "injected",
    }
}

/// One local completion record (a failed invocation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureRecord {
    /// Completion time, Unix seconds.
    pub t: i64,
    pub operation: String,
    pub access_intent: String,
    pub target: String,
    pub outcome: Outcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub duration_ms: i64,
    pub context_source: String,
    pub trace_id: String,
    pub span_id: String,
}

impl FailureRecord {
    fn from_span(
        span: &SpanRecord,
        inv: &GhInvocation,
        outcome: Outcome,
        exit_code: Option<i32>,
    ) -> Self {
        Self {
            t: span.ended_at.timestamp(),
            operation: inv.operation.as_str().to_string(),
            access_intent: inv.intent.as_str().to_string(),
            target: inv.target.bounded(),
            outcome,
            exit_code,
            duration_ms: (span.ended_at - span.started_at).num_milliseconds(),
            context_source: inv.context_source().to_string(),
            trace_id: span.context.trace_id.as_str().to_string(),
            span_id: span.context.span_id.as_str().to_string(),
        }
    }
}

/// Append `record` to the host sink. Never fails the caller.
pub fn record_failure(record: &FailureRecord) {
    if let Some(dir) = record_dir() {
        if let Err(e) = append(&dir, record) {
            log::debug!("gh_invocation: failure record to {} not written: {e}", dir.display());
        }
    }
}

#[cfg(not(test))]
fn record_dir() -> Option<PathBuf> {
    match std::env::var("LOOM_GH_INVOCATION_RECORD_DIR") {
        Ok(d) if d == "off" || d == "0" => None,
        Ok(d) if !d.is_empty() => Some(PathBuf::from(d)),
        _ => Some(crate::forge_etag_store::host_tmp_base().join("loom-gh-invocations")),
    }
}

/// Test builds: no sink unless the current test thread opts in.
#[cfg(test)]
fn record_dir() -> Option<PathBuf> {
    TEST_RECORD_DIR.with(|d| d.borrow().clone())
}

#[cfg(test)]
thread_local! {
    static TEST_RECORD_DIR: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Point THIS test thread's failure-record sink at `dir` (`None` = off).
#[cfg(test)]
pub(crate) fn set_test_record_dir(dir: Option<PathBuf>) {
    TEST_RECORD_DIR.with(|d| *d.borrow_mut() = dir);
}

fn record_file(dir: &Path, hour: i64) -> PathBuf {
    dir.join(format!("failures-{hour}.jsonl"))
}

fn append(dir: &Path, record: &FailureRecord) -> std::io::Result<()> {
    // Same owner-only rules as the ETag store: a 0700 dir we own, 0600 files.
    if !crate::forge_etag_store::private_dir(dir, true) {
        return Err(std::io::Error::other("untrusted record dir"));
    }
    let mut buf = serde_json::to_vec(record)?;
    buf.push(b'\n');
    let hour = record.t.div_euclid(3600);
    let path = record_file(dir, hour);
    let mut create = std::fs::OpenOptions::new();
    create.append(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut create, 0o600);
    let (mut file, fresh) = match create.open(&path) {
        Ok(f) => (f, true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            (std::fs::OpenOptions::new().append(true).open(&path)?, false)
        }
        Err(e) => return Err(e),
    };
    // One write of one short line: atomic under O_APPEND.
    file.write_all(&buf)?;
    if fresh {
        prune(dir, hour);
    }
    Ok(())
}

fn prune(dir: &Path, current_hour: i64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let hour = entry
            .file_name()
            .to_str()
            .and_then(|n| n.strip_prefix("failures-"))
            .and_then(|n| n.strip_suffix(".jsonl"))
            .and_then(|n| n.parse::<i64>().ok());
        if hour.is_some_and(|h| h < current_hour - RETAIN_HOURS) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Read every failure record in `dir` (tests and future views).
#[must_use]
pub fn read_records(dir: &Path) -> Vec<FailureRecord> {
    let mut records = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return records;
    };
    let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if let Ok(text) = std::fs::read_to_string(&path) {
            records.extend(text.lines().filter_map(|l| serde_json::from_str(l).ok()));
        }
    }
    records
}

#[cfg(test)]
#[path = "telemetry_tests.rs"]
mod tests;
