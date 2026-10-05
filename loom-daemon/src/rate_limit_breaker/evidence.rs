//! Which reset epoch a trip may trust (Issue #8997 gap b).
//!
//! The breaker used to derive its cooldown from one `gh api rate_limit` probe
//! run with no workspace context. On a host whose ambient `gh` credential is
//! a *user* token, that probe reads the user's healthy budget while the
//! exhausted bucket belongs to a GitHub App *installation* — so no bucket ever
//! read `remaining == 0` and every trip took the fallback guess. A second,
//! independent hole (operator report, 2026-09-27): on a freshly created
//! installation `GET /rate_limit` reads `used=0, remaining=<full>` while the
//! response headers of real calls show the budget being spent. A probe that
//! *succeeds* with wrong numbers is worse than one that fails.
//!
//! The rules here, in order:
//!
//! 1. `X-RateLimit-*` headers captured from the **failing response itself**
//!    are authoritative — they describe exactly the bucket that refused the
//!    call. They win when they say `remaining: 0` for a `core`/`graphql`
//!    resource with a reset still in the future.
//! 2. Otherwise a probe (run with the failing call's root/credential context,
//!    see [`super::report::FailureContext`]) is trusted only when it shows an
//!    exhausted bucket whose reset is still in the future.
//! 3. A probe that reads *healthy* during a **primary** rate-limit failure
//!    contradicts the failure it is meant to explain: it is untrustworthy
//!    (the false-full case), so the trip takes the configured fallback and
//!    the reading is not cached as the status surface's "last budget".
//! 4. A secondary-limit failure does not zero a primary bucket, so a healthy
//!    probe is expected there — fallback cooldown, reading kept for status.
//!
//! Every path ends in [`ResetEvidence::cooldown_until`], which keeps the
//! `[MIN_COOLDOWN_SECS, MAX_COOLDOWN_SECS]` clamps.

use chrono::{DateTime, Duration, Utc};

use super::{BudgetSnapshot, MAX_COOLDOWN_SECS, MIN_COOLDOWN_SECS};

/// The resource names GitHub reports in `X-RateLimit-Resource` for the two
/// primary buckets the breaker models.
const PRIMARY_RESOURCES: &[&str] = &["core", "graphql"];

/// Signatures of GitHub's *secondary* (abuse / concurrency) limits, a subset
/// of [`super::RATE_LIMIT_SIGNATURES`]: these refuse calls without zeroing a
/// primary bucket.
const SECONDARY_SIGNATURES: &[&str] = &[
    "secondary rate limit",
    "abuse detection mechanism",
    "was submitted too quickly",
];

/// Whether `text` is a secondary-limit rejection (see [`SECONDARY_SIGNATURES`]).
#[must_use]
pub fn is_secondary_limit(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    SECONDARY_SIGNATURES.iter().any(|sig| lowered.contains(sig))
}

/// Where a trip's release time came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResetEvidence {
    /// The failing response's own `X-RateLimit-*` headers.
    FailureResponse {
        resource: String,
        reset: DateTime<Utc>,
    },
    /// A contextual budget probe showing an exhausted bucket.
    Probe { reset: DateTime<Utc> },
    /// No trustworthy reset: the configured fallback cooldown applies.
    Fallback { reason: &'static str },
}

impl ResetEvidence {
    /// Release time for a cooldown starting at `now`, clamped to
    /// `[MIN_COOLDOWN_SECS, MAX_COOLDOWN_SECS]` from `now`.
    #[must_use]
    pub fn cooldown_until(&self, now: DateTime<Utc>, fallback_secs: u64) -> DateTime<Utc> {
        let candidate = match self {
            Self::FailureResponse { reset, .. } | Self::Probe { reset } => *reset,
            Self::Fallback { .. } => {
                now + Duration::seconds(i64::try_from(fallback_secs).unwrap_or(MAX_COOLDOWN_SECS))
            }
        };
        candidate.clamp(
            now + Duration::seconds(MIN_COOLDOWN_SECS),
            now + Duration::seconds(MAX_COOLDOWN_SECS),
        )
    }

    /// Whether this is a real reset reading (not the fallback guess).
    #[must_use]
    pub fn is_trusted(&self) -> bool {
        !matches!(self, Self::Fallback { .. })
    }

    /// One-line description for the trip/refine log.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::FailureResponse { resource, reset } => {
                format!("failing response headers ({resource} exhausted, resets {reset})")
            }
            Self::Probe { reset } => format!("contextual budget probe (resets {reset})"),
            Self::Fallback { reason } => format!("fallback cooldown — {reason}"),
        }
    }
}

/// `X-RateLimit-*` fields read off one response head.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HeaderReading {
    pub remaining: Option<u64>,
    pub reset: Option<i64>,
    pub used: Option<u64>,
    pub resource: Option<String>,
}

/// Parse the `X-RateLimit-*` headers out of a response head (pure; header
/// names case-insensitive, CRLF tolerated, anything after the first blank
/// line ignored so a body can never masquerade as headers).
#[must_use]
pub fn parse_ratelimit_headers(head: &str) -> HeaderReading {
    let mut reading = HeaderReading::default();
    for line in head.lines().map(|l| l.trim_end_matches('\r')) {
        if line.trim().is_empty() {
            break;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "x-ratelimit-remaining" => reading.remaining = v.parse().ok(),
            "x-ratelimit-reset" => reading.reset = v.parse().ok(),
            "x-ratelimit-used" => reading.used = v.parse().ok(),
            "x-ratelimit-resource" => reading.resource = Some(v.to_ascii_lowercase()),
            _ => {}
        }
    }
    reading
}

/// Rule 1: trusted evidence from the failing response's headers, or `None`
/// when they are absent, malformed, name a non-primary resource, are not
/// exhausted (a secondary limit), or carry a reset that already passed.
#[must_use]
pub fn from_failure_headers(head: &str, now: DateTime<Utc>) -> Option<ResetEvidence> {
    let r = parse_ratelimit_headers(head);
    let resource = r
        .resource
        .filter(|res| PRIMARY_RESOURCES.contains(&res.as_str()))?;
    if r.remaining? != 0 {
        return None;
    }
    let reset = DateTime::from_timestamp(r.reset?, 0)?;
    (reset > now).then_some(ResetEvidence::FailureResponse { resource, reset })
}

/// Rules 2–4 for a probe reading. Returns the evidence plus whether the
/// reading itself may be shown on the status surface (`false` for a stale or
/// self-contradicting reading, so status never claims a healthy budget the
/// failure just disproved).
#[must_use]
pub fn from_probe(
    error_text: &str,
    budget: Option<&BudgetSnapshot>,
    now: DateTime<Utc>,
) -> (ResetEvidence, bool) {
    let Some(b) = budget else {
        return (
            ResetEvidence::Fallback {
                reason: "no budget reading",
            },
            false,
        );
    };
    let exhausted: Vec<DateTime<Utc>> = [
        (b.core_remaining, b.core_reset),
        (b.graphql_remaining, b.graphql_reset),
    ]
    .into_iter()
    .filter(|(remaining, _)| *remaining == 0)
    .map(|(_, reset)| reset)
    .collect();
    if let Some(latest) = exhausted.iter().copied().filter(|r| *r > now).max() {
        return (ResetEvidence::Probe { reset: latest }, true);
    }
    if !exhausted.is_empty() {
        return (
            ResetEvidence::Fallback {
                reason: "probe's exhausted bucket reset already passed (stale reading)",
            },
            false,
        );
    }
    if is_secondary_limit(error_text) {
        return (
            ResetEvidence::Fallback {
                reason: "secondary rate limit (primary budgets not exhausted)",
            },
            true,
        );
    }
    (
        ResetEvidence::Fallback {
            reason: "probe reads a non-exhausted budget during a primary rate-limit failure \
                     (wrong credential or false-full /rate_limit) — reading untrusted",
        },
        false,
    )
}
