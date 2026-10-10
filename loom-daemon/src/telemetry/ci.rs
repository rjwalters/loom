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
    // `ci.run` / `ci.job` (#10511): when this daemon *observed* the completed
    // run/job — its knowable-at instant, distinct from GitHub's own
    // `completed_at`. A point-in-time reader filters on it, never on the
    // event time.
    "loom.ci.observed_at",
    "loom.ci.duration_ms",
    // `ci.run` (#9007 follow-up: `run_started_at − created_at`) AND `ci.job`
    // (#9089: `started_at − created_at`) — the CI queue segment for each, so
    // "CI queued" and "CI running" are separable at both levels.
    "loom.ci.queued_ms",
    // `ci.job` only (#9089): milliseconds the job spent blocked on its
    // `needs:` predecessors before GitHub created it — `created_at` minus the
    // run attempt's first job creation. Distinct from `loom.ci.queued_ms`,
    // which starts only once the job exists; see
    // `ci_telemetry::records::JobCreationBaseline`.
    "loom.ci.dependency_wait_ms",
    // `ci.run` only (#9337): why this run attempt happened — `new_commit`,
    // `stale_main_bump`, `flaky_retry` or `unknown`
    // (`ci_telemetry::records::TriggerReason`).
    "loom.ci.trigger_reason",
    "loom.ci.job_id",
    "loom.ci.job",
    "loom.ci.runner",
    "loom.ci.attempts",
    "loom.ci.timed_out",
    // `ci.job` only (#9089): a matrix leg's shard identity, parsed from its
    // display name — see `ci_telemetry::records::parse_shard`.
    "loom.ci.shard.index",
    "loom.ci.shard.total",
    "loom.ci.shard.kind",
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
    // Join keys (Issue #9007): `loom.ci.ref` reuses the log-side name for the
    // head branch rather than inventing `loom.ci.head_branch`. `loom.pr_number`
    // needs no entry here — it is already in `bounded_attributes()`'s generic
    // always-admitted key list and the collector's span `keep_keys`.
    "loom.ci.head_sha",
    "loom.ci.ref",
    // Run span (#9007 follow-up) AND job span (#9089): the CI queue segment,
    // see `CiRunRecord::queued_ms` / `CiJobRecord::queued_ms`.
    "loom.ci.queued_ms",
    // Job span only (#9089): the dependency segment that precedes the queue
    // segment — see `CiJobRecord::dependency_wait_ms`. Not carried on step or
    // suite spans: it is a property of the job, and repeating it there would
    // multiply one job's wait across its children in any sum.
    "loom.ci.dependency_wait_ms",
    // Run span only (#9337): the attempt (so `flaky_retry` is auditable from
    // the span — the job span's equivalent is `loom.ci.attempts`) and the
    // trigger attribution, see `CiRunRecord::trigger_reason`.
    "loom.ci.run_attempt",
    "loom.ci.trigger_reason",
    // Job span only (#9089): a matrix leg's shard identity — see
    // `ci_telemetry::records::parse_shard`.
    "loom.ci.shard.index",
    "loom.ci.shard.total",
    "loom.ci.shard.kind",
    // `loom.ci.step` span only (#9089): which step of its job this is. The
    // step span also repeats its job's `loom.ci.job`/`loom.ci.job_id` and the
    // shard trio above, so "which step of which leg is slow" is one group-by
    // and needs no trace join.
    "loom.ci.step",
    "loom.ci.step_number",
    // `loom.ci.suite` span only (#9089): which shell test suite of its sharded
    // job this is, from the timings artifact `run-ci-suites.sh` uploads. The
    // outcome has its own key rather than reusing `loom.ci.conclusion`: that
    // one carries GitHub's vocabulary (`success`/`failure`/…) everywhere else,
    // and mixing a suite's `pass`/`fail`/`skip` into it would corrupt every
    // group-by over it. Like a step span, a suite span also repeats its job's
    // identity and shard trio.
    "loom.ci.suite",
    "loom.ci.suite.outcome",
    "loom.ci.suite.retried",
    // `loom.ci.test` span only (#9456): which test of its `nextest-partition`
    // leg this is, from the JUnit XML the `ci` nextest profile writes. Two
    // keys rather than one concatenated id, because a test path is only
    // unique within its binary and "which binary is slow" is its own
    // group-by. The outcome has its own key for the same reason the suite
    // outcome does: `loom.ci.conclusion` carries GitHub's vocabulary
    // everywhere else, and mixing a test's pass/fail/flaky into it would
    // corrupt every group-by over it. Like a suite span, a test span also
    // repeats its job's identity and shard trio.
    "loom.ci.test",
    "loom.ci.test.binary",
    "loom.ci.test.outcome",
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
    /// Milliseconds the run sat queued before a runner picked it up:
    /// `run_started_at − created_at`, floored at zero (#9007 follow-up). The
    /// span and `duration_ms` both start at `started_at`, so without this the
    /// queue wait was invisible. `None` when GitHub reported no
    /// `run_started_at` (a missing start never reads as a zero queue) and on
    /// a pre-#9007 journal line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_ms: Option<i64>,
    /// Why this run attempt happened (#9337): `new_commit`,
    /// `stale_main_bump`, `flaky_retry` or `unknown` — see
    /// `ci_telemetry::records::TriggerReason`. `None` only on a pre-#9337
    /// journal line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_reason: Option<String>,
    /// When this daemon observed the record (#10511): the instant it became
    /// knowable to Loom, as opposed to GitHub's event time (`completed_at`).
    /// A point-in-time reader filters on it (`observed_at <= cutoff`), so a
    /// run backfilled later can never leak into a
    /// cutoff it was not knowable at. `None` only on a pre-#10511 record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<DateTime<Utc>>,
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
        if let Some(queued_ms) = self.queued_ms {
            out.push(("loom.ci.queued_ms", CiAttr::Int(queued_ms)));
        }
        if let Some(reason) = &self.trigger_reason {
            out.push(("loom.ci.trigger_reason", CiAttr::Str(reason.clone())));
        }
        if let Some(observed_at) = self.observed_at {
            out.push(("loom.ci.observed_at", CiAttr::Str(observed_at.to_rfc3339())));
        }
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
    /// Milliseconds this job sat queued for a runner: `started_at −
    /// created_at`, floored at zero (#9089 — the per-job analogue of
    /// [`CiRunRecord::queued_ms`]). `None` when GitHub reported no
    /// `created_at` for the job (a pre-#9089 recording never reads as a zero
    /// queue).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_ms: Option<i64>,
    /// Milliseconds this job spent blocked on its `needs:` predecessors before
    /// GitHub created it: `created_at` minus the run attempt's earliest job
    /// creation, floored at zero (#9089, issue problem 5).
    ///
    /// This is the segment that precedes [`Self::queued_ms`], never part of
    /// it: `dependency_wait_ms` ends when the job is created,
    /// `queued_ms` begins there and ends when a runner picks it up. A job that
    /// waited 59s on `build-daemon` and then 3s for a runner reports
    /// `59_000` and `3_000`, and a `needs:` fan-in is distinguishable from a
    /// runner-capacity burst without reading the workflow file.
    ///
    /// `None` when GitHub reported no `created_at` for the job or for any job
    /// of its run (a pre-#9089 recording) — never a zero, which would read as
    /// "waited on nothing".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependency_wait_ms: Option<i64>,
    /// A matrix leg's 1-based position, parsed from the job's display name
    /// (#9089; see `ci_telemetry::records::parse_shard`). `None` for an
    /// unsharded job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shard_index: Option<u32>,
    /// The matrix's total leg count, alongside `shard_index`. `None` for an
    /// unsharded job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shard_total: Option<u32>,
    /// Which sharded job family this is: `nextest-partition`,
    /// `shell-suite-shard`, or `none` (#9089). Always present — unlike
    /// `shard_index`/`shard_total`, "not sharded" is itself the answer, not
    /// an absence.
    #[serde(default = "shard_kind_none")]
    pub shard_kind: String,
    /// When this daemon observed the record (#10511): the instant it became
    /// knowable to Loom, as opposed to GitHub's event time (`completed_at`).
    /// A point-in-time reader filters on it (`observed_at <= cutoff`), so a
    /// run backfilled later can never leak into a
    /// cutoff it was not knowable at. `None` only on a pre-#10511 record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<DateTime<Utc>>,
}

fn shard_kind_none() -> String {
    "none".to_string()
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
        if let Some(queued_ms) = self.queued_ms {
            out.push(("loom.ci.queued_ms", CiAttr::Int(queued_ms)));
        }
        if let Some(dependency_wait_ms) = self.dependency_wait_ms {
            out.push(("loom.ci.dependency_wait_ms", CiAttr::Int(dependency_wait_ms)));
        }
        if let Some(index) = self.shard_index {
            out.push(("loom.ci.shard.index", CiAttr::Int(i64::from(index))));
        }
        if let Some(total) = self.shard_total {
            out.push(("loom.ci.shard.total", CiAttr::Int(i64::from(total))));
        }
        out.push(("loom.ci.shard.kind", CiAttr::Str(self.shard_kind.clone())));
        if let Some(observed_at) = self.observed_at {
            out.push(("loom.ci.observed_at", CiAttr::Str(observed_at.to_rfc3339())));
        }
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
    /// When this daemon observed the sample (#10511) — see
    /// [`CiRunRecord::observed_at`]. Never a metric label (unbounded
    /// cardinality): it rides on the native-HTTPS record only, and the
    /// histogram point keeps `completed_at` as its time. `None` only on a
    /// pre-#10511 record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<DateTime<Utc>>,
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
