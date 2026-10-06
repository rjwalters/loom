use super::{SpanId, TraceContext};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type TraceAttributes = BTreeMap<String, String>;

/// Fixed names prevent payload text or issue numbers becoming operation names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpanName {
    #[serde(rename = "loom.sweep")]
    Sweep,
    #[serde(rename = "loom.phase")]
    Phase,
    #[serde(rename = "loom.role_attempt")]
    RoleAttempt,
    #[serde(rename = "loom.runtime.preflight")]
    RuntimePreflight,
    #[serde(rename = "loom.runtime.run")]
    RuntimeRun,
    #[serde(rename = "loom.tool")]
    Tool,
    /// One GitHub Actions workflow run (Issue #8824) — the root of a CI trace.
    #[serde(rename = "loom.ci.run")]
    CiRun,
    /// One job of a GitHub Actions run, parented to its [`Self::CiRun`] span.
    #[serde(rename = "loom.ci.job")]
    CiJob,
    /// One step of a GitHub Actions job (Issue #9089), parented to its
    /// [`Self::CiJob`] span. Derived from the `steps[]` array of the jobs
    /// listing the poller already fetches — no extra API call.
    #[serde(rename = "loom.ci.step")]
    CiStep,
    /// One shell test suite of a sharded job (Issue #9089), parented to its
    /// [`Self::CiJob`] span. Built from the timings artifact
    /// `run-ci-suites.sh` uploads — the only surface that carries per-suite
    /// durations out of a finished runner.
    #[serde(rename = "loom.ci.suite")]
    CiSuite,
    /// One test of a `nextest-partition` leg (Issue #9456), parented to its
    /// [`Self::CiJob`] span. Built from the JUnit XML
    /// `.config/nextest.toml`'s `ci` profile writes and `ci.yml` uploads —
    /// the only surface that carries per-test durations out of a finished
    /// runner. Emitted for the leg's slow tail only, never one span per test;
    /// see `ci_telemetry::nextest`.
    #[serde(rename = "loom.ci.test")]
    CiTest,
    /// One work-finder tick (Issue #8860) — its own root trace per tick.
    #[serde(rename = "loom.dispatch.tick")]
    DispatchTick,
    /// One execution's exact token usage (Issue #8908): a late child of its
    /// `loom.runtime.run` span, journalled once usage is known.
    #[serde(rename = "loom.runtime.usage")]
    RuntimeUsage,
    /// One pool dispatch hold, from arming to clearing (Issue #8931) — its own
    /// root trace.
    #[serde(rename = "loom.pool.hold")]
    PoolHold,
    /// One work-finder `dispatch()` attempt (Issue #8907), parented to its
    /// tick's [`Self::DispatchTick`] span.
    #[serde(rename = "loom.dispatch.admission")]
    DispatchAdmission,
    /// One ready-queue row's disposition, emitted on a transition, a periodic
    /// refresh, or the row leaving the queue (Issue #9222), parented to its
    /// tick's [`Self::DispatchTick`] span when the tick is still known.
    #[serde(rename = "loom.dispatch.disposition")]
    DispatchDisposition,
    /// One `gh` invocation through the `gh_invocation` facade (Issue #9985):
    /// the client-side "caller operation" span of the forge egress trace
    /// tree. A child of the caller's execution when it has one, else its own
    /// root (`context_source=missing`).
    #[serde(rename = "invoke github")]
    GithubInvoke,
    /// One GitHub rate-limit breaker trip (Issue #10022): an instant span,
    /// its own root trace, carrying the tripping job and the trip-time
    /// own/external attribution.
    #[serde(rename = "loom.ratelimit.trip")]
    RateLimitTrip,
    /// One reader App withdrawal (W4-A): an instant span, its own root
    /// trace, naming the App, owner, resource, end and reset source.
    #[serde(rename = "forge.reader.withdrawn")]
    ForgeReaderWithdrawn,
    /// One read-pool spill-latch transition (W4-B): an instant span, its own
    /// root trace, naming the repo, resource, home and target reader, mode
    /// and release instant.
    #[serde(rename = "forge.reader.spill")]
    ForgeReaderSpill,
    /// One read a reader route deferred (W4-C): an instant span, its own
    /// root trace, naming the operation, class, reader, owner, resource and
    /// until.
    #[serde(rename = "forge.read.shed")]
    ForgeReadShed,
}

impl SpanName {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sweep => "loom.sweep",
            Self::Phase => "loom.phase",
            Self::RoleAttempt => "loom.role_attempt",
            Self::RuntimePreflight => "loom.runtime.preflight",
            Self::RuntimeRun => "loom.runtime.run",
            Self::Tool => "loom.tool",
            Self::CiRun => "loom.ci.run",
            Self::CiJob => "loom.ci.job",
            Self::CiStep => "loom.ci.step",
            Self::CiSuite => "loom.ci.suite",
            Self::CiTest => "loom.ci.test",
            Self::DispatchTick => "loom.dispatch.tick",
            Self::RuntimeUsage => "loom.runtime.usage",
            Self::PoolHold => "loom.pool.hold",
            Self::DispatchAdmission => "loom.dispatch.admission",
            Self::DispatchDisposition => "loom.dispatch.disposition",
            Self::GithubInvoke => "invoke github",
            Self::RateLimitTrip => "loom.ratelimit.trip",
            Self::ForgeReaderWithdrawn => "forge.reader.withdrawn",
            Self::ForgeReaderSpill => "forge.reader.spill",
            Self::ForgeReadShed => "forge.read.shed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanStatus {
    Unset,
    Ok,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpanEvent {
    pub name: String,
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub attributes: TraceAttributes,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpanLink {
    pub context: TraceContext,
}

/// Only completed spans enter the durable export queue; unfinished roots stay
/// in execution state and do not delay completed children's export.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpanRecord {
    pub context: TraceContext,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<SpanId>,
    pub name: SpanName,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub status: SpanStatus,
    #[serde(default)]
    pub attributes: TraceAttributes,
    #[serde(default)]
    pub events: Vec<SpanEvent>,
    #[serde(default)]
    pub links: Vec<SpanLink>,
}

/// Applied again at export so a restored queue cannot bypass emission policy.
pub fn bounded_attributes(attributes: &TraceAttributes) -> TraceAttributes {
    attributes
        .iter()
        .filter(|(key, value)| {
            (matches!(
                key.as_str(),
                "loom.repo"
                    | "loom.repo.visibility"
                    | "loom.sweep_id"
                    | "loom.story_id"
                    | "loom.story"
                    | "loom.story.key_version"
                    | "loom.issue"
                    | "loom.pr_number"
                    | "loom.role"
                    | "loom.phase"
                    | "loom.attempt"
                    | "loom.attempt.worked"
                    | "loom.runtime"
                    | "loom.provider"
                    | "loom.model"
                    | "loom.configured_model"
                    | "loom.result"
                    | "loom.failure_class"
                    | "loom.host.mem_total_bytes"
                    | "loom.host.mem_available_bytes"
                    | "loom.host.mem_compressed_bytes"
                    | "loom.host.swap_total_bytes"
                    | "loom.host.swap_used_bytes"
                    | "loom.host.swap_in_bytes_total"
                    | "loom.host.swap_out_bytes_total"
                    | "loom.host.oom_kill_total"
                    | "loom.host.pressure"
                    | "loom.host.load_per_core"
                    | "loom.admission.reason"
                    | "loom.admission.load_per_core"
                    | "loom.admission.load_threshold"
                    | "loom.admission.pool"
                    | "loom.admission.pool_total"
                    | "loom.admission.unmet_capabilities"
                    | "loom.effort"
                    | "loom.doctor_cycles"
                    | "loom.judge_verdict"
                    | "loom.recovered"
                    | "loom.timing_source"
                    | "loom.tool.name"
            ) || crate::telemetry::ci::CI_SPAN_ATTRIBUTE_KEYS.contains(&key.as_str())
                || crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&key.as_str())
                || crate::gh_invocation::telemetry::SPAN_ATTRIBUTE_KEYS.contains(&key.as_str())
                || super::provenance::KEYS.contains(&key.as_str()))
                && value.len() <= 256
                && !value.chars().any(char::is_control)
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

impl SpanRecord {
    pub fn validate(&self) -> Result<(), &'static str> {
        if [self.started_at, self.ended_at]
            .iter()
            .any(|at| at.timestamp_nanos_opt().is_none_or(|nanos| nanos < 0))
        {
            return Err("span timestamp is outside supported Unix nanosecond bounds");
        }
        if self.ended_at < self.started_at {
            return Err("span ends before it starts");
        }
        if self.parent_span_id.as_ref() == Some(&self.context.span_id) {
            return Err("span cannot parent itself");
        }
        Ok(())
    }

    #[must_use]
    pub fn bounded(mut self) -> Self {
        self.attributes = bounded_attributes(&self.attributes);
        self.events.retain(|e| {
            matches!(
                e.name.as_str(),
                "started" | "completed" | "retry" | "cancelled" | "recovered" | "rejected"
            ) && e.at >= self.started_at
                && e.at <= self.ended_at
        });
        self.events.truncate(32);
        for event in &mut self.events {
            event.attributes = bounded_attributes(&event.attributes);
        }
        self.links.truncate(16);
        self
    }
}
