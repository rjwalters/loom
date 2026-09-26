//! Reason-classified pool-account marks and pool-hold spans (Issue #8931).
//!
//! `loom.pool.exhaustions` (#8857) is derived by diffing 5-minute pool
//! snapshots, so it says *that* an account left the usable pool but not
//! *why*. The reason is known, inside the daemon, at the moment the mark is
//! written. Each seam that writes one calls into this module, which emits one
//! `loom.pool.account_marks{provider,reason}` delta point:
//!
//! | Seam | Classification read | Provider |
//! |---|---|---|
//! | `sweep_registry::provider_health_feedback` (Codex) | `TerminalClassification` | `codex` |
//! | `sweep_registry::provider_health_feedback` (native) | `api_keys_pool::classify::Classification` | the API-key pool namespace |
//! | `role_runner::provider_health_feedback` (both) | same pair, per role tick | same |
//! | `sweep_registry::quarantine::insta_crash_is_account_exhaustion` | the Claude exhaustion signature | `claude` |
//!
//! A point is emitted only when a mark was actually written: a native
//! credential failure (no bad mark) or a failed write emits nothing, and a
//! Codex outcome that records no hold (`SUCCESS`, `TIMEOUT`, …) maps to no
//! reason.
//!
//! **Nothing about the account leaves the host.** `reason` is the closed
//! [`MarkReason`] enum and `provider` is either a fixed literal or a pool
//! namespace passed through [`provider_label`], which replaces anything that
//! is not a short `[a-z0-9_-]` token with `other`. There is no `account`
//! label, no key and no log text.
//!
//! The work finder's pool pre-flight hold (`work_finder::pool_preflight`)
//! emits one `loom.pool.hold` span per hold when it **clears**, covering
//! arm → clear: [`record_pool_hold`]. A hold still armed when the daemon
//! stops produces no span.

use std::path::Path;

use chrono::{DateTime, Utc};

use crate::api_keys_pool::classify::Classification;
use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};
use crate::tokens_pool::TerminalClassification;

/// Why an account was marked out of selection. The `reason` label's whole
/// vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MarkReason {
    /// A rate or usage-window limit: an API-key pool's 429, or Claude's "hit
    /// your … limit" / `RATE_LIMIT_ABORT` banners (the wording cannot tell a
    /// 5-hour window from a weekly one, so neither is claimed).
    RateLimited,
    /// The plan's allowance for its billing period is used up (Codex
    /// `TOKEN_EXHAUSTED`, an API-key pool's exhaustion).
    Exhausted,
    /// A concurrent-session ceiling (Codex `SESSION_LIMIT`).
    SessionLimit,
    /// Per-model credits or a per-model ceiling (Codex
    /// `MODEL_CREDITS_EXHAUSTED`, Claude's `model-credits-exhausted` and
    /// `model-limit` signatures).
    ModelCredits,
    /// The credential is dead and needs re-authentication (Codex
    /// `TOKEN_EXPIRED`).
    Credential,
    /// A transient failure given a short backoff (Codex `RECOVERABLE`).
    Transient,
}

impl MarkReason {
    /// Every value, in label order.
    pub const ALL: [Self; 6] = [
        Self::RateLimited,
        Self::Exhausted,
        Self::SessionLimit,
        Self::ModelCredits,
        Self::Credential,
        Self::Transient,
    ];

    /// The `reason` label value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::Exhausted => "exhausted",
            Self::SessionLimit => "session_limit",
            Self::ModelCredits => "model_credits",
            Self::Credential => "credential",
            Self::Transient => "transient",
        }
    }

    /// The reason a Codex terminal classification marks an account for, or
    /// `None` when `tokens_pool::health` records no hold for it.
    #[must_use]
    pub fn from_codex(classification: TerminalClassification) -> Option<Self> {
        match classification {
            TerminalClassification::TokenExhausted => Some(Self::Exhausted),
            TerminalClassification::ModelCreditsExhausted => Some(Self::ModelCredits),
            TerminalClassification::SessionLimit => Some(Self::SessionLimit),
            TerminalClassification::TokenExpired => Some(Self::Credential),
            TerminalClassification::Recoverable => Some(Self::Transient),
            TerminalClassification::Success
            | TerminalClassification::Timeout
            | TerminalClassification::Fatal
            | TerminalClassification::CwdDeleted
            | TerminalClassification::ModelRefusal => None,
        }
    }

    /// The reason an API-key pool classification marks an account for.
    #[must_use]
    pub fn from_api_key(classification: Classification) -> Self {
        match classification {
            Classification::Exhausted => Self::Exhausted,
            Classification::RateLimited => Self::RateLimited,
            Classification::CredentialFailure => Self::Credential,
        }
    }

    /// The reason for a Claude insta-crash exhaustion signature
    /// (`sweep_registry::classify_account_exhaustion`'s labels).
    #[must_use]
    pub fn from_claude_signature(signature: &str) -> Option<Self> {
        match signature {
            "rate-limited" | "rate-limit-abort" => Some(Self::RateLimited),
            "model-credits-exhausted" | "model-limit" => Some(Self::ModelCredits),
            _ => None,
        }
    }
}

/// Longest `provider` label kept verbatim.
const MAX_PROVIDER_LEN: usize = 32;

/// A pool namespace as a `provider` label: a short lowercase
/// `[a-z0-9_-]` token passes through, anything else becomes `other`, so no
/// free text (or an account name smuggled into a namespace) can become a
/// label value.
#[must_use]
pub fn provider_label(namespace: &str) -> String {
    let ok = !namespace.is_empty()
        && namespace.len() <= MAX_PROVIDER_LEN
        && namespace
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if ok {
        namespace.to_string()
    } else {
        "other".to_string()
    }
}

/// The single point one mark emits.
#[must_use]
pub fn mark_point(provider: &str, reason: MarkReason) -> MetricPoint {
    MetricPoint::int(MetricName::PoolAccountMarks, 1)
        .label("provider", provider_label(provider))
        .label("reason", reason.as_str())
}

/// Emit one mark. A no-op when no ops sink is registered.
pub fn record_mark(provider: &str, reason: MarkReason) {
    super::emit_metrics(vec![mark_point(provider, reason)]);
}

/// A Codex account's terminal feedback was persisted as `classification`.
pub fn record_codex(classification: TerminalClassification) {
    if let Some(reason) = MarkReason::from_codex(classification) {
        record_mark("codex", reason);
    }
}

/// An API-key pool ingest returned `feedback`: one point when it bad-marked
/// the account, none when it did not (a credential failure, a failed write).
pub fn record_api_key(feedback: &crate::api_keys_pool::ingest::LaunchFeedback) {
    if feedback.mark.is_some() {
        record_mark(&feedback.provider, MarkReason::from_api_key(feedback.classification));
    }
}

/// Mark a Claude account bad for an insta-crash exhaustion `signature` — the
/// quarantine seam's `bad_tokens::mark_bad`, plus its point on success. Lives
/// here because `sweep_registry/quarantine.rs` is frozen by the file-size
/// ratchet.
pub fn mark_claude_bad(
    workspace: &Path,
    token_name: &str,
    reason: &str,
    signature: &str,
) -> Result<(), String> {
    crate::tokens_pool::bad_tokens::mark_bad(workspace, token_name, reason)?;
    if let Some(reason) = MarkReason::from_claude_signature(signature) {
        record_mark("claude", reason);
    }
    Ok(())
}

/// The `loom.pool.hold` span for a hold armed at `since` and cleared at
/// `cleared`: its own sampled root trace.
#[must_use]
pub fn hold_span(
    since: DateTime<Utc>,
    cleared: DateTime<Utc>,
    post_mortem: bool,
    accounts: usize,
) -> SpanRecord {
    let attributes: TraceAttributes = [
        ("loom.pool.hold.post_mortem", post_mortem.to_string()),
        ("loom.pool.hold.accounts", accounts.to_string()),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect();
    SpanRecord {
        context: TraceContext::root(true),
        parent_span_id: None,
        name: SpanName::PoolHold,
        started_at: since,
        ended_at: cleared.max(since),
        status: SpanStatus::Ok,
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    }
}

/// Export one cleared pool hold. A no-op when no ops sink is registered.
pub fn record_pool_hold(
    since: DateTime<Utc>,
    cleared: DateTime<Utc>,
    post_mortem: bool,
    accounts: usize,
) {
    super::emit_span(hold_span(since, cleared, post_mortem, accounts));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "pool_marks_tests.rs"]
mod tests;
