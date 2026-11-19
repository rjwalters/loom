//! `loom.runtime.usage_missing`: a run whose usage could not be read says so
//! (Issue #14566).
//!
//! A `loom.runtime.usage` span exists only when a reader found tokens, so a
//! run it could not read left nothing, and "never read" looked the same as "no
//! such run". On 0.19.1316 about 40% of OpenCode runs closed that way and no
//! query could say why. Where a run's usage is unreadable but its trace
//! context is known, one `loom.runtime.usage_missing` span now lands beside
//! where the usage span would have, carrying `loom.usage.missing_reason`: one
//! token of [`UsageMissing`], a fixed set small enough to `GROUP BY`.
//!
//! Counters are never estimated: the span has none. A run whose usage *was*
//! read keeps its usage span and gets no missing span.
//!
//! Not coverable: a run with no persisted trace context has nowhere to hang a
//! span, and a span with a fresh root would break the no-random-trace-id policy
//! (`trace-identity.md`). That case is a `log::warn!` at the emit site.

use chrono::{DateTime, Utc};

use super::spans::UsageScope;
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};

/// `loom.usage.missing_reason`: why a run's usage could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageMissing {
    /// The step's own log (or its dispatch header) could not be read, so the
    /// native session ids are unknown. The registry can still know the runtime.
    NoStepLog,
    /// The step's log names no native session id (the child died before its
    /// event stream emitted one) and the directory + window read found
    /// nothing. For a role tick the "own log" is its latest per-role-log
    /// slice (#14664); a Codex run whose ids WERE captured reports the
    /// store/session reasons below instead, from the by-id read.
    NoSessionId,
    /// Candidate usage data was unavailable (absent, unreadable or locked
    /// past the bounded wait).
    StoreUnreadable,
    /// Stores were read and none held a session the log names.
    SessionNotFound,
    /// A named session exists but recorded no tokens.
    NoUsageTokens,
    /// A runtime whose reader cannot say more than "nothing attributable".
    NoAttributableUsage,
    /// The sweep's start instant was unknown, so no window bounded the read.
    NoWindow,
}

impl UsageMissing {
    /// The wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoStepLog => "no_step_log",
            Self::NoSessionId => "no_session_id",
            Self::StoreUnreadable => "store_unreadable",
            Self::SessionNotFound => "session_not_found",
            Self::NoUsageTokens => "no_usage_tokens",
            Self::NoAttributableUsage => "no_attributable_usage",
            Self::NoWindow => "no_sweep_window",
        }
    }

    /// The reason for an `Unattributable` sweep, from the `tokens_status_reason`
    /// [`crate::sweep_usage::resolve`] already computed. `None` for any other
    /// reason: a sweep that never spawned (nothing to measure) or whose reading
    /// was `Suspect` (it has rows) is not a missing-usage case.
    #[must_use]
    pub fn from_sweep_reason(reason: &str) -> Option<Self> {
        match reason {
            crate::sweep_usage::REASON_NO_WINDOW => Some(Self::NoWindow),
            crate::sweep_usage::REASON_NO_STORE => Some(Self::StoreUnreadable),
            crate::sweep_usage::REASON_NO_TRANSCRIPT => Some(Self::NoAttributableUsage),
            _ => None,
        }
    }
}

/// The `loom.runtime.usage_missing` span for one unit, a child of `parent`
/// over `window`. `common` (`loom.runtime`, `loom.role`, `loom.sweep_id`, …) goes
/// on it as on a usage span. The id derives from `(parent, scope)`, so a
/// re-emit yields the same id and `append_new` skips it. Pure.
#[must_use]
pub fn span(
    parent: &TraceContext,
    window: (DateTime<Utc>, DateTime<Utc>),
    reason: UsageMissing,
    scope: UsageScope,
    common: &TraceAttributes,
) -> SpanRecord {
    let (started_at, ended_at) = window;
    let mut attributes = common.clone();
    attributes.retain(|_, v| !v.is_empty());
    attributes.insert("loom.usage.missing_reason".into(), reason.as_str().into());
    attributes.insert("loom.usage.scope".into(), scope.as_str().into());
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    SpanRecord {
        context: parent.derived_child(&[SpanName::RuntimeUsageMissing.as_str(), scope.as_str()]),
        parent_span_id: Some(parent.span_id.clone()),
        name: SpanName::RuntimeUsageMissing,
        started_at,
        ended_at: ended_at.max(started_at),
        status: SpanStatus::Ok,
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    }
    .bounded()
}

#[cfg(test)]
#[path = "missing_tests.rs"]
mod tests;
