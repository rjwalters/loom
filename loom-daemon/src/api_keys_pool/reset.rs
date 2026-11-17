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
//! # What is live and what is still inference
//!
//! Real Z.ai output was captured on 2026-11-16 (#12395; the constants are in
//! [`super::classify`]). It showed two things the synthetic fixtures had not:
//!
//! * The reset instant arrives **inside JSON string values** (`error.data.
//!   message`, and again in the escaped `responseBody`), never as a reset
//!   field. Reading the raw line as prose left the time glued to the JSON
//!   that follows it (`01:52:07","statusCode":429,…`), nothing parsed, and
//!   every mark took the default cooldown. String values are now read as
//!   prose themselves, and a stamp token is cut at its first non-stamp byte.
//! * The stamp has no timezone and is in the provider's wall clock, **UTC+8**.
//!   The response says so itself: its `x-log-id` opens with the request time
//!   in that clock while its `date` header gives the same instant in GMT.
//!   When an event carries both, the difference is the offset applied to a
//!   timezone-less stamp ([`wall_clock_offset_secs`]). Nothing is assumed
//!   about a provider by name.
//!
//! The other shapes (RFC 3339 instants, epoch seconds/milliseconds,
//! `retry-after` seconds, reset fields) remain generic and unobserved; their
//! fixtures are synthetic and say so.
//!
//! # Fail-safe direction
//!
//! A parsed horizon is used only when it is in the future and no further than
//! [`MAX_RESET_HORIZON_SECS`] out (a weekly cap plus a day of slack);
//! anything else falls through to the configured or default window rather
//! than inventing a hold. A timezone-less timestamp with no offset evidence
//! beside it is read as **UTC**: the providers in scope are east of UTC
//! (Z.ai/Zhipu), so a UTC reading is never *earlier* than the true instant —
//! the error, if any, holds a seat a little long rather than re-selecting it
//! before it has recovered. The offset evidence is used only when it is a
//! whole number of quarter hours within fourteen hours of UTC.
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
    // Offset evidence the text carries outside any one event: the egress proxy
    // hands over a bare error body followed by `date:` / `x-log-id:` header
    // lines. An event that proves its own offset uses that instead.
    let text_offset = text_wall_clock_offset_secs(text).unwrap_or(0);
    text.lines()
        .rev()
        .find_map(|line| line_reset_at(line, now, text_offset).filter(|at| plausible(*at, now)))
}

fn plausible(at: u64, now: u64) -> bool {
    at > now && at - now <= MAX_RESET_HORIZON_SECS
}

fn line_reset_at(line: &str, now: u64, text_offset: i64) -> Option<u64> {
    let trimmed = line.trim();
    let mut offset = text_offset;
    if trimmed.starts_with('{') {
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            offset = wall_clock_offset_secs(&value).unwrap_or(text_offset);
            if let Some(at) = json_reset_at(&value, now, offset) {
                return Some(at);
            }
        }
    }
    // The same offset as the structured read: a stamp the event's own clock
    // puts in the past must not come back as a future instant read as UTC.
    prose_reset_at(line, now, offset)
}

/// Header name of the response time in GMT, and of the provider's log id.
pub const CLOCK_HEADERS: [&str; 2] = ["date", "x-log-id"];

/// [`wall_clock_offset_secs`] for evidence written as `<header>: <value>`
/// lines (the egress proxy's reset text). The last line of each name wins.
fn text_wall_clock_offset_secs(text: &str) -> Option<i64> {
    let header = |wanted: &str| {
        text.lines().rev().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case(wanted)
                .then(|| value.trim())
        })
    };
    offset_between(header(CLOCK_HEADERS[0])?, header(CLOCK_HEADERS[1])?)
}

/// The quarter-hour offset between an RFC 2822 `date` (GMT) and a log id that
/// opens with the same request's `YYYYMMDDHHMMSS` wall-clock stamp, or `None`
/// when either is unreadable, they disagree by more than two minutes from a
/// quarter hour, or the offset is outside [`MAX_WALL_CLOCK_OFFSET_SECS`].
fn offset_between(date: &str, log_id: &str) -> Option<i64> {
    let utc = DateTime::parse_from_rfc2822(date.trim()).ok()?.timestamp();
    let local = NaiveDateTime::parse_from_str(log_id.trim().get(..14)?, "%Y%m%d%H%M%S")
        .ok()?
        .and_utc()
        .timestamp();
    let raw = local - utc;
    let offset = ((raw + raw.signum() * 450) / 900) * 900;
    ((raw - offset).abs() <= 120 && offset.abs() <= MAX_WALL_CLOCK_OFFSET_SECS).then_some(offset)
}

/// Largest wall-clock offset from UTC accepted as evidence, either side.
const MAX_WALL_CLOCK_OFFSET_SECS: i64 = 14 * 3600;

/// Seconds **east of UTC** of the provider's wall clock, when one JSON event
/// proves it: an object holding both an HTTP `date` header and an `x-log-id`
/// that opens with a `YYYYMMDDHHMMSS` stamp of the same request (the live Z.ai
/// shape, [`super::classify::CAPTURED_ZAI_WEEKLY_LIMIT_EVENT`]). `None` when
/// the pair is absent, unparseable, or not a quarter-hour offset inside
/// [`MAX_WALL_CLOCK_OFFSET_SECS`] — the caller then reads the stamp as UTC.
fn wall_clock_offset_secs(value: &Value) -> Option<i64> {
    match value {
        Value::Object(map) => {
            let header = |wanted: &str| {
                map.iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case(wanted))
                    .and_then(|(_, inner)| inner.as_str())
            };
            let here = header(CLOCK_HEADERS[0])
                .zip(header(CLOCK_HEADERS[1]))
                .and_then(|(date, log_id)| offset_between(date, log_id));
            here.or_else(|| map.values().find_map(wall_clock_offset_secs))
        }
        Value::Array(items) => items.iter().find_map(wall_clock_offset_secs),
        _ => None,
    }
}

/// Depth-first, last-key-wins search of a JSON value for a reset field, or for
/// a string value that states a reset in prose (the live Z.ai shape: the
/// instant is in `error.data.message`). `naive_offset` is the wall-clock
/// offset a timezone-less stamp is read in ([`wall_clock_offset_secs`]).
fn json_reset_at(value: &Value, now: u64, naive_offset: i64) -> Option<u64> {
    match value {
        Value::Object(map) => map.iter().rev().find_map(|(key, inner)| {
            let key = key.to_ascii_lowercase();
            if ABSOLUTE_KEYS.contains(&key.as_str()) {
                if let Some(at) =
                    absolute_value(inner, naive_offset).filter(|at| plausible(*at, now))
                {
                    return Some(at);
                }
            }
            if RELATIVE_KEYS.contains(&key.as_str()) {
                if let Some(at) = relative_value(inner, now) {
                    return Some(at);
                }
            }
            json_reset_at(inner, now, naive_offset)
        }),
        Value::Array(items) => items
            .iter()
            .rev()
            .find_map(|item| json_reset_at(item, now, naive_offset)),
        Value::String(text) => {
            prose_reset_at(text, now, naive_offset).filter(|at| plausible(*at, now))
        }
        _ => None,
    }
}

fn absolute_value(value: &Value, naive_offset: i64) -> Option<u64> {
    match value {
        Value::Number(n) => n.as_u64().and_then(epoch_from_number),
        Value::String(s) => parse_instant(s, naive_offset),
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
/// `YYYY-MM-DD[ T]HH:MM[:SS]`, or an epoch number. A timezone-less stamp is
/// read in the wall clock `naive_offset` seconds east of UTC — `0`, i.e. UTC,
/// unless the event proved another (see the module docs).
fn parse_instant(raw: &str, naive_offset: i64) -> Option<u64> {
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
            return u64::try_from(naive.and_utc().timestamp() - naive_offset).ok();
        }
    }
    None
}

/// The leading run of `token` that can belong to a timestamp. A stamp quoted
/// inside JSON arrives glued to what follows it (`01:52:07","statusCode":…`,
/// or `01:52:07\"}}` in an escaped body); everything from the first byte no
/// timestamp uses is dropped.
fn stamp_prefix(token: &str) -> &str {
    // An opening quote or parenthesis is not part of the stamp either.
    let token = token.trim_start_matches(['"', '\'', '(']);
    let end = token
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '-' | ':' | 'T' | 'Z' | '+' | '.')))
        .unwrap_or(token.len());
    &token[..end]
}

fn prose_reset_at(line: &str, now: u64, naive_offset: i64) -> Option<u64> {
    // `to_ascii_lowercase` is length-preserving, so offsets found in the
    // lowered copy index the original line.
    let lowered = line.to_ascii_lowercase();
    for phrase in ABSOLUTE_PHRASES {
        if let Some(at) = lowered.rfind(phrase) {
            let rest = line.get(at + phrase.len()..)?;
            let tokens: Vec<&str> = rest.split_whitespace().take(2).map(stamp_prefix).collect();
            // A date and a time as two tokens first, then one token alone.
            let joined = tokens.join(" ");
            if let Some(found) = parse_instant(&joined, naive_offset)
                .or_else(|| parse_instant(tokens.first()?, naive_offset))
            {
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
