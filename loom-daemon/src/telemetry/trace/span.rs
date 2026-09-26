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
            Self::DispatchTick => "loom.dispatch.tick",
            Self::RuntimeUsage => "loom.runtime.usage",
            Self::PoolHold => "loom.pool.hold",
            Self::DispatchAdmission => "loom.dispatch.admission",
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
                    | "loom.issue"
                    | "loom.pr_number"
                    | "loom.role"
                    | "loom.phase"
                    | "loom.attempt"
                    | "loom.runtime"
                    | "loom.provider"
                    | "loom.model"
                    | "loom.configured_model"
                    | "loom.result"
                    | "loom.failure_class"
                    | "loom.effort"
                    | "loom.doctor_cycles"
                    | "loom.judge_verdict"
                    | "loom.recovered"
                    | "loom.timing_source"
                    | "loom.tool.name"
            ) || crate::telemetry::ci::CI_SPAN_ATTRIBUTE_KEYS.contains(&key.as_str())
                || crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&key.as_str()))
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
