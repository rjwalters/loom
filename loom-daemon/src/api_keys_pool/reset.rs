//! Reset-aligned cooldowns for the API-key pool (issue #11286 item 2).
//!
//! # The gap this closes
//!
//! Before this module every automatic mark used a fixed horizon:
//! [`Classification::default_cooldown_secs`] — 6 h for `Exhausted`, 60 s for
//! `RateLimited`. Neither follows the provider's real window. A Z.ai coding
//! plan binds on a **weekly** cap (2am#996), so a seat that hit it was retried
//! every 6 h all week; and a seat whose 5-hour window reset after 5 h still
//! sat out the full 6 h.
//!
//! The horizon is now resolved in a fixed order ([`resolve_cooldown`]):
//!
//! 1. **A reset instant the provider itself printed** ([`parse_reset_at`]) —
//!    a JSON `reset_at`-style field, a `retry-after` value, or a prose
//!    `… will reset at <timestamp>` line. Applies to both `Exhausted` and
//!    `RateLimited`: the provider's own word on when it recovers is the best
//!    fact available whatever the needle was.
//! 2. **The account's configured plan window** (`exhaustionWindowSecs` in the
//!    provider's `.limits.json`, set with `api-keys limit --exhaustion-window`)
//!    — `Exhausted` only. The plan limit is *configured, not probed*: no
//!    provider here offers a cheap, plan-safe usage probe.
//! 3. **The classification's default** — the pre-#11286 behaviour, so an
//!    account with neither keeps working exactly as before.
//!
//! A credential failure never gets a horizon at all (see [`super::classify`]).
//!
//! # Honest limits: the shapes are not live captures
//!
//! **No real Z.ai exhausted/rate-limited output has been captured** (the same
//! gap [`super::classify`] records). The shapes recognised here are generic
//! and provider-neutral — RFC 3339 instants, epoch seconds/milliseconds,
//! `retry-after` seconds, and a timezone-less `YYYY-MM-DD HH:MM:SS` — chosen
//! because they are what HTTP APIs conventionally emit, not because any was
//! observed coming out of a Z.ai seat. Every test fixture for this module is
//! therefore **synthetic** and says so. Fold real captures in as they are
//! observed.
//!
//! # Fail-safe direction
//!
//! A parsed horizon is used only when it is in the future and no further than
//! [`MAX_RESET_HORIZON_SECS`] out (a weekly cap plus a day of slack);
//! anything else falls through to the configured or default window rather
//! than inventing a hold. A timezone-less timestamp is read as **UTC**: the
//! providers in scope are east of UTC (Z.ai/Zhipu), so a UTC reading is never
//! *earlier* than the true instant — the error, if any, holds a seat a little
//! long rather than re-selecting it before it has recovered.
//!
//! Last match wins, as in [`crate::tokens_pool::codex_reset`]: the provider's
//! fatal refusal is the last thing written before the harness gives up.

use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::Value;

use super::classify::Classification;

/// Furthest a parsed reset instant may lie in the future: a weekly cap plus a
/// day of slack. Beyond that the text is more likely a misread than a window.
pub const MAX_RESET_HORIZON_SECS: u64 = 8 * 24 * 3600;

/// JSON keys whose value is an **absolute** reset instant.
const ABSOLUTE_KEYS: &[&str] = &[
    "reset_at",
    "resets_at",
    "resetat",
    "reset_time",
    "resettime",
    "next_reset_time",
    "nextresettime",
    "next_reset_at",
];

/// JSON keys whose value is **seconds from now**.
const RELATIVE_KEYS: &[&str] = &["retry_after", "retryafter", "retry-after"];

/// Prose phrases that introduce an absolute instant. Matched lowercase.
const ABSOLUTE_PHRASES: &[&str] = &["will reset at", "resets at", "reset at", "reset time:"];

/// Prose phrases that introduce a relative number of seconds. Matched
/// lowercase.
const RELATIVE_PHRASES: &[&str] = &["retry-after:", "retry-after=", "retry after"];

/// Where a resolved cooldown came from — rendered into the mark's operator
/// detail so `api-keys list` can say *why* a seat is held as long as it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CooldownSource {
    /// The provider printed a reset instant ([`parse_reset_at`]).
    ProviderReset,
    /// The account's configured `exhaustionWindowSecs`.
    ConfiguredWindow,
    /// [`Classification::default_cooldown_secs`].
    Default,
}

impl CooldownSource {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ProviderReset => "provider-reported reset",
            Self::ConfiguredWindow => "configured plan window",
            Self::Default => "default cooldown",
        }
    }
}

/// The cooldown (seconds from `now`) a mark of `classification` should carry,
/// and where it came from — or `None` when the classification must not mark
/// at all (a credential failure). See the module docs for the order.
///
/// `provider_text` must already be the provider's/harness's own words (for a
/// launch log, [`super::classify::provider_lines`]) — never an agent
/// transcript, which could otherwise *extend* a hold by quoting a date.
#[must_use]
pub fn resolve_cooldown(
    classification: Classification,
    provider_text: &str,
    now: u64,
    configured_window: Option<u64>,
) -> Option<(u64, CooldownSource)> {
    let fallback = classification.default_cooldown_secs()?;
    if let Some(at) = parse_reset_at(provider_text, now) {
        return Some(((at - now).max(1), CooldownSource::ProviderReset));
    }
    match (classification, configured_window.filter(|secs| *secs > 0)) {
        (Classification::Exhausted, Some(window)) => {
            Some((window, CooldownSource::ConfiguredWindow))
        }
        _ => Some((fallback, CooldownSource::Default)),
    }
}

/// The account's configured plan window, or `None` when none is declared or
/// the limits file cannot be read. Unreadable degrades to the default horizon
/// rather than refusing the mark: a mark with the default cooldown is still
/// strictly better than no mark.
#[must_use]
pub fn configured_window(root: &std::path::Path, provider: &str, account: &str) -> Option<u64> {
    super::limits::read_limits(root, provider)
        .ok()?
        .get(account)
        .and_then(|limits| limits.exhaustion_window_secs)
}

/// The **last** plausible reset instant (Unix seconds) `text` names, or
/// `None`. See the module docs for the recognised shapes and the plausibility
/// window.
#[must_use]
pub fn parse_reset_at(text: &str, now: u64) -> Option<u64> {
    text.lines()
        .rev()
        .find_map(|line| line_reset_at(line, now).filter(|at| plausible(*at, now)))
}

fn plausible(at: u64, now: u64) -> bool {
    at > now && at - now <= MAX_RESET_HORIZON_SECS
}

fn line_reset_at(line: &str, now: u64) -> Option<u64> {
    let trimmed = line.trim();
    if trimmed.starts_with('{') {
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            if let Some(at) = json_reset_at(&value, now) {
                return Some(at);
            }
        }
    }
    prose_reset_at(line, now)
}

/// Depth-first, last-key-wins search of a JSON value for a reset field.
fn json_reset_at(value: &Value, now: u64) -> Option<u64> {
    match value {
        Value::Object(map) => map.iter().rev().find_map(|(key, inner)| {
            let key = key.to_ascii_lowercase();
            if ABSOLUTE_KEYS.contains(&key.as_str()) {
                if let Some(at) = absolute_value(inner).filter(|at| plausible(*at, now)) {
                    return Some(at);
                }
            }
            if RELATIVE_KEYS.contains(&key.as_str()) {
                if let Some(at) = relative_value(inner, now) {
                    return Some(at);
                }
            }
            json_reset_at(inner, now)
        }),
        Value::Array(items) => items.iter().rev().find_map(|item| json_reset_at(item, now)),
        _ => None,
    }
}

fn absolute_value(value: &Value) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64().and_then(epoch_from_number),
        Value::String(s) => parse_instant(s),
        _ => None,
    }
}

fn relative_value(value: &Value, now: u64) -> Option<u64> {
    let secs = match value {
        Value::Number(n) => n.as_u64()?,
        Value::String(s) => s.trim().parse::<u64>().ok()?,
        _ => return None,
    };
    (secs > 0).then(|| now.saturating_add(secs))
}

/// An integer that is an absolute instant: epoch milliseconds or seconds. A
/// small integer is ambiguous (seconds-from-now? a counter?) and is refused.
fn epoch_from_number(n: u64) -> Option<u64> {
    if n >= 1_000_000_000_000 {
        Some(n / 1000)
    } else if n >= 1_000_000_000 {
        Some(n)
    } else {
        None
    }
}

/// An absolute instant in text: RFC 3339, a timezone-less
/// `YYYY-MM-DD[ T]HH:MM[:SS]` (read as UTC — see the module docs), or an
/// epoch number.
fn parse_instant(raw: &str) -> Option<u64> {
    let candidate = raw
        .trim()
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '(' | ')' | ',' | ';'))
        .trim_end_matches('.');
    if let Ok(n) = candidate.parse::<u64>() {
        return epoch_from_number(n);
    }
    if let Ok(at) = DateTime::parse_from_rfc3339(candidate) {
        return u64::try_from(at.with_timezone(&Utc).timestamp()).ok();
    }
    for format in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(candidate, format) {
            return u64::try_from(naive.and_utc().timestamp()).ok();
        }
    }
    None
}

fn prose_reset_at(line: &str, now: u64) -> Option<u64> {
    // `to_ascii_lowercase` is length-preserving, so offsets found in the
    // lowered copy index the original line.
    let lowered = line.to_ascii_lowercase();
    for phrase in ABSOLUTE_PHRASES {
        if let Some(at) = lowered.rfind(phrase) {
            let rest = line.get(at + phrase.len()..)?;
            let tokens: Vec<&str> = rest.split_whitespace().take(2).collect();
            // A date and a time as two tokens first, then one token alone.
            let joined = tokens.join(" ");
            if let Some(found) = parse_instant(&joined).or_else(|| parse_instant(tokens.first()?)) {
                return Some(found);
            }
        }
    }
    for phrase in RELATIVE_PHRASES {
        if let Some(at) = lowered.rfind(phrase) {
            let rest = lowered.get(at + phrase.len()..)?;
            let digits: String = rest
                .trim_start()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if let Ok(secs) = digits.parse::<u64>() {
                if secs > 0 {
                    return Some(now.saturating_add(secs));
                }
            }
        }
    }
    None
}

#[cfg(test)]
#[path = "reset_tests.rs"]
mod tests;
