//! CI (GitHub Actions) telemetry record kinds (Issue #8824 phase 1, #8825
//! phase 2, of the build/CI observability work under epic #8522).
//!
//! Four record kinds, all produced by `crate::ci_telemetry`'s poller:
//!
//! - `ci.run` ([`CiRunRecord`]) — one completed workflow run.
//! - `ci.job` ([`CiJobRecord`]) — one completed job of a run (every attempt's
//!   jobs, each keyed by its own GitHub `job_id`).
//! - `ci.duration` ([`CiDurationRecord`]) — the metric carrier: one
//!   run-or-job duration sample, mapped to the `loom.ci.run.duration_ms` /
//!   `loom.ci.job.duration_ms` OTLP histograms. It exists as its own kind
//!   because every envelope maps to exactly **one** OTLP signal (the OTLP
//!   exporter's retry loop acknowledges contiguous same-signal prefixes), so a
//!   `ci.run` log record cannot also be a histogram data point.
//! - `ci.job.log` ([`CiJobLogRecord`], #8825) — one ≤ 8 KiB chunk of one
//!   completed job's log text. **The only Loom record kind whose body is
//!   free text the daemon did not author**, which is exactly why the gateway
//!   carries a `ci.job.log`-scoped scrub stage ([`CI_LOG_SCRUB_CLASSES`])
//!   ahead of the shared allowlist: the daemon deliberately forwards what
//!   GitHub sent, chunked and size-capped, and the **gateway** is the
//!   redaction boundary.
//!
//! # Attribute discipline (#8669 allowlist precedent)
//!
//! The attribute vocabulary is declared **here, once**, as the constants
//! below. The OTLP mapping renders exactly [`CiRunRecord::log_attributes`] /
//! [`CiJobRecord::log_attributes`] (plus the shared `loom.repo` /
//! `loom.repo.visibility` keys), metric data points carry exactly
//! [`CI_METRIC_LABEL_KEYS`], and the gateway collector's `keep_keys` lists are
//! asserted against these constants by `ci_telemetry`'s contract test — so a
//! new attribute cannot ship without the allowlist moving with it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::RepoVisibility;

/// Every `loom.ci.*` log-record attribute key the `ci.run` / `ci.job` kinds
/// can emit. The gateway's log `keep_keys` must list exactly these
/// `loom.ci.*` keys (contract-tested).
pub const CI_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.ci.run_id",
    "loom.ci.run_attempt",
    "loom.ci.workflow",
    "loom.ci.ref",
    "loom.ci.head_sha",
    "loom.ci.event",
    "loom.ci.status",
    "loom.ci.conclusion",
    "loom.ci.triggered_by",
    "loom.ci.started_at",
    "loom.ci.completed_at",
    "loom.ci.duration_ms",
    "loom.ci.job_id",
    "loom.ci.job",
    "loom.ci.runner",
    "loom.ci.attempts",
    "loom.ci.timed_out",
    // `ci.job.log` (#8825). `loom.ci.chunk_index` doubles as the gateway's
    // "this is a job-log chunk" predicate — the scrub stage in
    // `defaults/observability/collector/config.yaml` is scoped by exactly
    // that key's presence, so it can never touch another kind's body.
    "loom.ci.chunk_index",
    "loom.ci.chunk_count",
    "loom.ci.log_bytes_total",
    "loom.ci.truncated",
    "loom.ci.truncation_note",
];

/// The attribute whose presence marks a log record as a `ci.job.log` chunk.
/// The gateway's scrub stage is scoped by this key and nothing else, so the
/// body exception cannot silently widen to another record kind.
pub const CI_LOG_CHUNK_MARKER_KEY: &str = "loom.ci.chunk_index";

/// Every secret class the gateway scrubs out of a `ci.job.log` body, in the
/// order the collector applies them. Each becomes `[REDACTED:<class>]`.
///
/// **This list is the reviewable source of truth.** A new secret family is
/// added here, to `defaults/observability/collector/config.yaml`, and to the
/// contract test **in the same PR** — the static contract test
/// (`collector_fanout::gateway_scrubs_exactly_the_declared_ci_log_classes`)
/// fails if the two ever disagree, in either direction.
///
/// Order matters: `authorization` consumes the remainder of an
/// `Authorization:` header line before `bearer-token` can leave its value
/// behind, and every earlier class consumes its own key name so the broad
/// `credential` assignment rule cannot re-redact an existing marker.
pub const CI_LOG_SCRUB_CLASSES: &[&str] = &[
    "authorization",
    "bearer-token",
    "github-token",
    "anthropic-key",
    "api-key",
    "aws-access-key-id",
    "aws-secret-access-key",
    "credential",
];

/// The `[REDACTED:<class>]` marker for one [`CI_LOG_SCRUB_CLASSES`] entry.
#[must_use]
pub fn scrub_marker(class: &str) -> String {
    format!("[REDACTED:{class}]")
}

/// Every `loom.ci.*` span attribute key the CI run/job spans can carry. The
/// gateway's span `keep_keys` must list exactly these `loom.ci.*` keys
/// (contract-tested), and `trace::bounded_attributes` admits them.
pub const CI_SPAN_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.ci.run_id",
    "loom.ci.workflow",
    "loom.ci.event",
    "loom.ci.conclusion",
    "loom.ci.job_id",
    "loom.ci.job",
    "loom.ci.runner",
    "loom.ci.attempts",
];

/// The low-cardinality metric label allowlist for the two CI duration
/// histograms. Never a sha, ref, run id, or issue number.
pub const CI_METRIC_LABEL_KEYS: &[&str] = &["repo", "workflow", "job", "runner", "conclusion"];

/// Histogram name for run durations.
pub const CI_RUN_DURATION_METRIC: &str = "loom.ci.run.duration_ms";
/// Histogram name for job durations.
pub const CI_JOB_DURATION_METRIC: &str = "loom.ci.job.duration_ms";

/// A typed attribute value, independent of the OTLP feature so the attribute
/// vocabulary is testable in a default build.
#[derive(Debug, Clone, PartialEq)]
pub enum CiAttr {
    Str(String),
    Int(i64),
    Bool(bool),
}

/// Milliseconds between `started_at` and `completed_at`, floored at zero (a
/// forge clock skew must never produce a negative duration).
#[must_use]
pub fn duration_ms(started_at: DateTime<Utc>, completed_at: DateTime<Utc>) -> i64 {
    (completed_at - started_at).num_milliseconds().max(0)
}

/// One completed GitHub Actions workflow run (`ci.run`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CiRunRecord {
    /// `owner/name`.
    pub repo: String,
    #[serde(default)]
    pub visibility: RepoVisibility,
    pub run_id: u64,
    pub run_attempt: u32,
    /// Workflow name (the run's `name`).
    pub workflow: String,
    /// The run's head branch, when GitHub reports one.
    #[serde(rename = "ref", default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    pub head_sha: String,
    /// Triggering event (`push`, `pull_request`, `schedule`, …).
    pub event: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    /// Login of the triggering actor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triggered_by: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub duration_ms: i64,
}

impl CiRunRecord {
    /// The `loom.ci.*` log attributes, in a stable order. Optional fields are
    /// absent (never empty strings) when GitHub did not report them.
    #[must_use]
    pub fn log_attributes(&self) -> Vec<(&'static str, CiAttr)> {
        let mut out = vec![
            ("loom.ci.run_id", CiAttr::Int(clamp_i64(self.run_id))),
            ("loom.ci.run_attempt", CiAttr::Int(i64::from(self.run_attempt))),
            ("loom.ci.workflow", CiAttr::Str(self.workflow.clone())),
        ];
        if let Some(git_ref) = &self.git_ref {
            out.push(("loom.ci.ref", CiAttr::Str(git_ref.clone())));
        }
        out.push(("loom.ci.head_sha", CiAttr::Str(self.head_sha.clone())));
        out.push(("loom.ci.event", CiAttr::Str(self.event.clone())));
        out.push(("loom.ci.status", CiAttr::Str(self.status.clone())));
        if let Some(conclusion) = &self.conclusion {
            out.push(("loom.ci.conclusion", CiAttr::Str(conclusion.clone())));
        }
        if let Some(actor) = &self.triggered_by {
            out.push(("loom.ci.triggered_by", CiAttr::Str(actor.clone())));
        }
        out.push(("loom.ci.started_at", CiAttr::Str(self.started_at.to_rfc3339())));
        out.push(("loom.ci.completed_at", CiAttr::Str(self.completed_at.to_rfc3339())));
        out.push(("loom.ci.duration_ms", CiAttr::Int(self.duration_ms)));
        out
    }
}

/// One completed job of a workflow run (`ci.job`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CiJobRecord {
    pub repo: String,
    #[serde(default)]
    pub visibility: RepoVisibility,
    pub run_id: u64,
    pub job_id: u64,
    /// The parent run's workflow name.
    pub workflow: String,
    /// Job name.
    pub job: String,
    /// First runner label (e.g. `ubuntu-latest`), when the job reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,
    /// The run attempt this job belongs to (GitHub `run_attempt`).
    pub attempts: u32,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    pub timed_out: bool,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub duration_ms: i64,
}

impl CiJobRecord {
    /// The `loom.ci.*` log attributes, in a stable order.
    #[must_use]
    pub fn log_attributes(&self) -> Vec<(&'static str, CiAttr)> {
        let mut out = vec![
            ("loom.ci.run_id", CiAttr::Int(clamp_i64(self.run_id))),
            ("loom.ci.job_id", CiAttr::Int(clamp_i64(self.job_id))),
            ("loom.ci.workflow", CiAttr::Str(self.workflow.clone())),
            ("loom.ci.job", CiAttr::Str(self.job.clone())),
        ];
        if let Some(runner) = &self.runner {
            out.push(("loom.ci.runner", CiAttr::Str(runner.clone())));
        }
        out.push(("loom.ci.attempts", CiAttr::Int(i64::from(self.attempts))));
        out.push(("loom.ci.status", CiAttr::Str(self.status.clone())));
        if let Some(conclusion) = &self.conclusion {
            out.push(("loom.ci.conclusion", CiAttr::Str(conclusion.clone())));
        }
        out.push(("loom.ci.timed_out", CiAttr::Bool(self.timed_out)));
        out.push(("loom.ci.started_at", CiAttr::Str(self.started_at.to_rfc3339())));
        out.push(("loom.ci.completed_at", CiAttr::Str(self.completed_at.to_rfc3339())));
        out.push(("loom.ci.duration_ms", CiAttr::Int(self.duration_ms)));
        out
    }
}

/// One ≤ 8 KiB chunk of one completed job's log text (`ci.job.log`, #8825).
///
/// # The body is free text, on purpose
///
/// Every other record kind's body is a string this daemon authored. This
/// one's is whatever GitHub's job-log endpoint returned, forwarded unfiltered
/// apart from the per-job size cap — the operator decision for #8825 is that
/// the **gateway** is the redaction boundary, not the source. Nothing in this
/// struct may ever move log text into an *attribute*: the gateway's scrub
/// stage rewrites the body only, so a log-derived attribute would ride
/// straight past it. That is why there is no `step` attribute here — see
/// `defaults/docs/ci-observability.md` §"Why there is no `step` attribute".
///
/// # Reconstruction contract
///
/// A job's log is `ORDER BY chunk_index` over the `chunk_count` records
/// sharing one `(repo, job_id)`. `truncated` is true on **every** chunk of a
/// capped log (not just the last), so a single record read in isolation can
/// never read as a complete log; the final marker chunk additionally carries
/// `truncation_note` naming the cap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CiJobLogRecord {
    pub repo: String,
    #[serde(default)]
    pub visibility: RepoVisibility,
    pub run_id: u64,
    pub job_id: u64,
    /// The parent run's workflow name.
    pub workflow: String,
    /// Job name.
    pub job: String,
    /// 0-based position of this chunk in the job's log.
    pub chunk_index: u32,
    /// How many chunks the job's log was split into, marker chunk included.
    pub chunk_count: u32,
    /// Bytes of log text GitHub returned for this job, before the cap.
    pub log_bytes_total: u64,
    /// True on every chunk of a log that hit the per-job cap.
    pub truncated: bool,
    /// Present only on the final marker chunk of a truncated log; names the
    /// cap that truncated it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation_note: Option<String>,
    /// The job's completion instant — the event time every chunk is stamped
    /// at, so logs land beside their `ci.job` record rather than at poll time.
    pub completed_at: DateTime<Utc>,
    /// This chunk's log text. **Never** promoted to an attribute.
    pub text: String,
}

impl CiJobLogRecord {
    /// The `loom.ci.*` log attributes, in a stable order. No attribute here
    /// is derived from log text.
    #[must_use]
    pub fn log_attributes(&self) -> Vec<(&'static str, CiAttr)> {
        let mut out = vec![
            ("loom.ci.run_id", CiAttr::Int(clamp_i64(self.run_id))),
            ("loom.ci.job_id", CiAttr::Int(clamp_i64(self.job_id))),
            ("loom.ci.workflow", CiAttr::Str(self.workflow.clone())),
            ("loom.ci.job", CiAttr::Str(self.job.clone())),
            ("loom.ci.chunk_index", CiAttr::Int(i64::from(self.chunk_index))),
            ("loom.ci.chunk_count", CiAttr::Int(i64::from(self.chunk_count))),
            ("loom.ci.log_bytes_total", CiAttr::Int(clamp_i64(self.log_bytes_total))),
            ("loom.ci.truncated", CiAttr::Bool(self.truncated)),
        ];
        if let Some(note) = &self.truncation_note {
            out.push(("loom.ci.truncation_note", CiAttr::Str(note.clone())));
        }
        out
    }
}

/// Which duration histogram a [`CiDurationRecord`] feeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiDurationMetric {
    Run,
    Job,
}

impl CiDurationMetric {
    /// The OTLP histogram name.
    #[must_use]
    pub fn metric_name(self) -> &'static str {
        match self {
            CiDurationMetric::Run => CI_RUN_DURATION_METRIC,
            CiDurationMetric::Job => CI_JOB_DURATION_METRIC,
        }
    }
}

/// One duration sample (`ci.duration`) — the carrier for the CI duration
/// histograms. `run_id` / `run_attempt` / `job_id` are the record's dedup
/// identity only; they
/// are **never** rendered as metric labels (see [`Self::metric_labels`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CiDurationRecord {
    pub metric: CiDurationMetric,
    pub repo: String,
    #[serde(default)]
    pub visibility: RepoVisibility,
    pub run_id: u64,
    pub run_attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<u64>,
    pub workflow: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub duration_ms: i64,
}

impl CiDurationRecord {
    /// The metric labels, drawn only from [`CI_METRIC_LABEL_KEYS`]. An
    /// unreported optional value is an absent label, never an empty one.
    #[must_use]
    pub fn metric_labels(&self) -> Vec<(&'static str, String)> {
        let mut out = vec![
            ("repo", self.repo.clone()),
            ("workflow", self.workflow.clone()),
        ];
        if let Some(job) = &self.job {
            out.push(("job", job.clone()));
        }
        if let Some(runner) = &self.runner {
            out.push(("runner", runner.clone()));
        }
        if let Some(conclusion) = &self.conclusion {
            out.push(("conclusion", conclusion.clone()));
        }
        out
    }
}

fn clamp_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
