//! GitHub Actions REST shapes → telemetry envelopes.
//!
//! Each completed **job** becomes one unit of three envelopes (`ci.job`, its
//! `ci.duration`, its `loom.ci.job` span) and each completed **run** one unit
//! of three (`ci.run`, its `ci.duration`, its `loom.ci.run` span). Trace and
//! span ids are **derived** from the GitHub identities (`repo`, `run_id`,
//! `job_id`), never random, so a replayed or second-host emission of the same
//! run is byte-identical in identity and a backend can deduplicate on it.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::telemetry::ci::{duration_ms, CiDurationMetric};
use crate::telemetry::trace::{
    SpanId, SpanRecord, SpanStatus, TraceAttributes, TraceContext, TraceId,
};
use crate::telemetry::{
    trace::SpanName, CiDurationRecord, CiJobRecord, CiRunRecord, RepoVisibility, TelemetryEnvelope,
    TelemetryRecord,
};

/// One `GET /orgs/{org}/repos` row, reduced to what the poller uses.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RepoJson {
    pub name: String,
    pub full_name: String,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub archived: bool,
}

impl RepoJson {
    #[must_use]
    pub fn visibility(&self) -> RepoVisibility {
        if self.private {
            RepoVisibility::Private
        } else {
            RepoVisibility::Public
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActorJson {
    pub login: String,
}

fn first_attempt() -> u32 {
    1
}

/// One `GET /repos/{o}/{r}/actions/runs` row.
#[derive(Debug, Clone, Deserialize)]
pub struct RunJson {
    pub id: u64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub head_branch: Option<String>,
    #[serde(default)]
    pub head_sha: String,
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default = "first_attempt")]
    pub run_attempt: u32,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub run_started_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub triggering_actor: Option<ActorJson>,
    #[serde(default)]
    pub actor: Option<ActorJson>,
}

impl RunJson {
    #[must_use]
    pub fn is_completed(&self) -> bool {
        self.status.as_deref() == Some("completed")
    }

    #[must_use]
    pub fn workflow(&self) -> String {
        self.name.clone().unwrap_or_else(|| "unnamed".to_string())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RunsPage {
    #[serde(default)]
    pub workflow_runs: Vec<RunJson>,
}

/// One `GET /repos/{o}/{r}/actions/runs/{id}/jobs` row.
#[derive(Debug, Clone, Deserialize)]
pub struct JobJson {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default = "first_attempt")]
    pub run_attempt: u32,
}

impl JobJson {
    #[must_use]
    pub fn is_completed(&self) -> bool {
        self.status == "completed"
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct JobsPage {
    #[serde(default)]
    pub jobs: Vec<JobJson>,
}

fn digest_hex(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0_u8]);
    }
    hex::encode(hasher.finalize())
}

/// Deterministic identifier from `parts`. SHA-256 output is all-zero with
/// negligible probability; the `1` fallback keeps the id valid regardless.
fn derived_id(parts: &[&str], hex_len: usize) -> String {
    let hex = digest_hex(parts)[..hex_len].to_string();
    if hex.bytes().all(|b| b == b'0') {
        format!("{}1", &hex[..hex_len - 1])
    } else {
        hex
    }
}

/// A run attempt's trace context: trace id and root span id both derived
/// from `(repo, run_id, attempt)` — one trace per run attempt. Always
/// sampled.
#[must_use]
pub fn run_context(repo: &str, run_id: u64, attempt: u32) -> TraceContext {
    let run = run_id.to_string();
    let attempt = attempt.to_string();
    TraceContext {
        trace_id: TraceId::try_from(derived_id(&["loom.ci.trace", repo, &run, &attempt], 32))
            .unwrap_or_else(|_| TraceContext::root(true).trace_id),
        span_id: SpanId::try_from(derived_id(&["loom.ci.run", repo, &run, &attempt], 16))
            .unwrap_or_else(|_| TraceContext::root(true).span_id),
        flags: 1,
    }
}

/// A job span's context inside its run attempt's trace.
#[must_use]
pub fn job_context(repo: &str, run_id: u64, attempt: u32, job_id: u64) -> TraceContext {
    let run = run_context(repo, run_id, attempt);
    TraceContext {
        span_id: SpanId::try_from(derived_id(&["loom.ci.job", repo, &job_id.to_string()], 16))
            .unwrap_or_else(|_| run.child().span_id),
        ..run
    }
}

fn span_status(conclusion: Option<&str>) -> SpanStatus {
    match conclusion {
        Some("success") => SpanStatus::Ok,
        Some("failure" | "timed_out" | "startup_failure") => SpanStatus::Error,
        _ => SpanStatus::Unset,
    }
}

fn attrs(pairs: Vec<(&str, Option<String>)>) -> TraceAttributes {
    pairs
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| (k.to_string(), v)))
        .collect()
}

fn envelope(
    host_id: &str,
    record: TelemetryRecord,
    ctx: Option<TraceContext>,
) -> TelemetryEnvelope {
    let mut env = TelemetryEnvelope::new(host_id, record);
    env.trace_context = ctx;
    env
}

/// The envelopes of one completed run's run-level unit: `ci.run`,
/// `ci.duration` (run), and the `loom.ci.run` root span.
#[must_use]
pub fn run_envelopes(repo: &RepoJson, run: &RunJson, host_id: &str) -> Vec<TelemetryEnvelope> {
    let started_at = run.run_started_at.unwrap_or(run.created_at);
    let completed_at = run.updated_at.max(started_at);
    let duration = duration_ms(started_at, completed_at);
    let workflow = run.workflow();
    let ctx = run_context(&repo.full_name, run.id, run.run_attempt);
    let record = CiRunRecord {
        repo: repo.full_name.clone(),
        visibility: repo.visibility(),
        run_id: run.id,
        run_attempt: run.run_attempt,
        workflow: workflow.clone(),
        git_ref: run.head_branch.clone(),
        head_sha: run.head_sha.clone(),
        event: run.event.clone(),
        status: run.status.clone().unwrap_or_default(),
        conclusion: run.conclusion.clone(),
        triggered_by: run
            .triggering_actor
            .as_ref()
            .or(run.actor.as_ref())
            .map(|a| a.login.clone()),
        started_at,
        completed_at,
        duration_ms: duration,
    };
    let duration_record = CiDurationRecord {
        metric: CiDurationMetric::Run,
        repo: repo.full_name.clone(),
        visibility: repo.visibility(),
        run_id: run.id,
        run_attempt: run.run_attempt,
        job_id: None,
        workflow: workflow.clone(),
        job: None,
        runner: None,
        conclusion: run.conclusion.clone(),
        started_at,
        completed_at,
        duration_ms: duration,
    };
    let span = SpanRecord {
        context: ctx.clone(),
        parent_span_id: None,
        name: SpanName::CiRun,
        started_at,
        ended_at: completed_at,
        status: span_status(run.conclusion.as_deref()),
        attributes: attrs(vec![
            ("loom.repo", Some(repo.full_name.clone())),
            ("loom.repo.visibility", Some(visibility_str(repo.visibility()).to_string())),
            ("loom.ci.run_id", Some(run.id.to_string())),
            ("loom.ci.workflow", Some(workflow)),
            ("loom.ci.event", Some(run.event.clone())),
            ("loom.ci.conclusion", run.conclusion.clone()),
        ]),
        events: Vec::new(),
        links: Vec::new(),
    };
    vec![
        envelope(host_id, TelemetryRecord::CiRun(record), Some(ctx.clone())),
        envelope(host_id, TelemetryRecord::CiDuration(duration_record), None),
        envelope(host_id, TelemetryRecord::Span(span), Some(ctx)),
    ]
}

fn visibility_str(visibility: RepoVisibility) -> &'static str {
    match visibility {
        RepoVisibility::Public => "public",
        RepoVisibility::Private => "private",
    }
}

/// The envelopes of one completed job's unit: `ci.job`, `ci.duration`
/// (job), and the `loom.ci.job` span parented to the run span.
#[must_use]
pub fn job_envelopes(
    repo: &RepoJson,
    run: &RunJson,
    job: &JobJson,
    host_id: &str,
) -> Vec<TelemetryEnvelope> {
    let started_at = job.started_at.unwrap_or(run.created_at);
    let completed_at = job.completed_at.unwrap_or(started_at).max(started_at);
    let duration = duration_ms(started_at, completed_at);
    let workflow = run.workflow();
    let runner = job.labels.first().cloned();
    let ctx = job_context(&repo.full_name, run.id, job.run_attempt, job.id);
    let run_span = run_context(&repo.full_name, run.id, job.run_attempt).span_id;
    let record = CiJobRecord {
        repo: repo.full_name.clone(),
        visibility: repo.visibility(),
        run_id: run.id,
        job_id: job.id,
        workflow: workflow.clone(),
        job: job.name.clone(),
        runner: runner.clone(),
        attempts: job.run_attempt,
        status: job.status.clone(),
        conclusion: job.conclusion.clone(),
        timed_out: job.conclusion.as_deref() == Some("timed_out"),
        started_at,
        completed_at,
        duration_ms: duration,
    };
    let duration_record = CiDurationRecord {
        metric: CiDurationMetric::Job,
        repo: repo.full_name.clone(),
        visibility: repo.visibility(),
        run_id: run.id,
        run_attempt: job.run_attempt,
        job_id: Some(job.id),
        workflow: workflow.clone(),
        job: Some(job.name.clone()),
        runner: runner.clone(),
        conclusion: job.conclusion.clone(),
        started_at,
        completed_at,
        duration_ms: duration,
    };
    let span = SpanRecord {
        context: ctx.clone(),
        parent_span_id: Some(run_span),
        name: SpanName::CiJob,
        started_at,
        ended_at: completed_at,
        status: span_status(job.conclusion.as_deref()),
        attributes: attrs(vec![
            ("loom.repo", Some(repo.full_name.clone())),
            ("loom.repo.visibility", Some(visibility_str(repo.visibility()).to_string())),
            ("loom.ci.run_id", Some(run.id.to_string())),
            ("loom.ci.job_id", Some(job.id.to_string())),
            ("loom.ci.workflow", Some(workflow)),
            ("loom.ci.job", Some(job.name.clone())),
            ("loom.ci.runner", runner),
            ("loom.ci.attempts", Some(job.run_attempt.to_string())),
            ("loom.ci.conclusion", job.conclusion.clone()),
        ]),
        events: Vec::new(),
        links: Vec::new(),
    };
    vec![
        envelope(host_id, TelemetryRecord::CiJob(record), Some(ctx.clone())),
        envelope(host_id, TelemetryRecord::CiDuration(duration_record), None),
        envelope(host_id, TelemetryRecord::Span(span), Some(ctx)),
    ]
}

/// The journal-level identity of one CI envelope — what "already emitted"
/// means when a committed-but-unconfirmed unit is replayed. `None` for any
/// non-CI envelope.
#[must_use]
pub fn envelope_identity(env: &TelemetryEnvelope) -> Option<String> {
    match &env.record {
        TelemetryRecord::CiRun(r) => {
            Some(format!("ci.run|{}|{}|{}", r.repo, r.run_id, r.run_attempt))
        }
        TelemetryRecord::CiJob(r) => Some(format!("ci.job|{}|{}", r.repo, r.job_id)),
        TelemetryRecord::CiDuration(r) => Some(format!(
            "ci.duration|{}|{}|{}|{}",
            r.repo,
            r.run_id,
            r.run_attempt,
            r.job_id
                .map_or_else(|| "run".to_string(), |id| id.to_string())
        )),
        TelemetryRecord::Span(s) => Some(format!("span|{}", s.context.span_id.as_str())),
        _ => None,
    }
}
