//! Z.ai GLM Coding plan quota reader (Issue #11396).
//!
//! `GET https://api.z.ai/api/monitor/usage/quota/limit` is a read-only
//! endpoint (no model call) that reports each seat's 5h and weekly credit
//! windows. This module turns it into `tokens.snapshot` account rows
//! ([`TokenAccountState`]) so the Z.ai pool shows a per-seat burn fraction,
//! which flows on through [`super::record`] into the pool gauges.
//!
//! Contract, all of it tested:
//!
//! - In a `CREDIT_LIMIT` window `usage` is the cap and `currentValue` the
//!   spend; `unit` 3 is the 5h window, 6 the weekly one; `nextResetTime` is
//!   epoch milliseconds. A fraction is clamped to `0..=1`; a zero cap is
//!   unknown, never a divide-by-zero.
//! - Anything short of a clean answer (transport error, non-2xx, timeout,
//!   `success:false`, a missing window) is **unknown** (`None`), never `0`.
//! - Results (including failures) are cached per seat: a success for
//!   [`TTL`], a failure for an exponentially growing backoff, so the endpoint
//!   sees at most one request per seat per TTL.
//! - The key is sent only in the `Authorization` header, in-process. No error
//!   type here carries it, a URL, or a response body.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::telemetry::TokenAccountState;

/// `provider` label — the API-key pool's directory name for Z.ai.
pub const ZAI_PROVIDER: &str = "zai";
/// The quota endpoint.
pub const QUOTA_URL: &str = "https://api.z.ai/api/monitor/usage/quota/limit";
/// Per-request timeout; the collector tick never waits longer per batch.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
/// Cache lifetime of a good reading: the collector's snapshot cadence, so a
/// tick never polls more often than it samples.
pub const TTL: Duration = crate::observability::SNAPSHOT_INTERVAL;
/// Ceiling on the failure backoff.
const MAX_BACKOFF: Duration = Duration::from_secs(60 * 60);

const UNIT_HOURS: i64 = 3;
const UNIT_WEEKS: i64 = 6;

/// One credit window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    /// Spend over cap, clamped to `0..=1`; `None` when the cap is unusable.
    pub fraction: Option<f64>,
    pub resets_at: Option<DateTime<Utc>>,
}

/// A seat's two windows; a window the response omits is `None`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ZaiQuota {
    pub five_hour: Option<Window>,
    pub weekly: Option<Window>,
}

/// Why a poll yielded no reading. Display text is fixed: never the key, URL
/// or response body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZaiError {
    Transport,
    Timeout,
    Status(u16),
    Rejected,
    Malformed,
}

impl std::fmt::Display for ZaiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ZaiError::Transport => f.write_str("z.ai quota request failed"),
            ZaiError::Timeout => f.write_str("z.ai quota request timed out"),
            ZaiError::Status(code) => write!(f, "z.ai quota endpoint returned HTTP {code}"),
            ZaiError::Rejected => f.write_str("z.ai quota endpoint reported success=false"),
            ZaiError::Malformed => f.write_str("z.ai quota response was not understood"),
        }
    }
}

fn window_from(limit: &serde_json::Value) -> Window {
    let cap = limit.get("usage").and_then(serde_json::Value::as_f64);
    let spent = limit
        .get("currentValue")
        .and_then(serde_json::Value::as_f64);
    let fraction = match (spent, cap) {
        (Some(spent), Some(cap)) if cap > 0.0 && cap.is_finite() && spent.is_finite() => {
            Some((spent / cap).clamp(0.0, 1.0))
        }
        _ => None,
    };
    let resets_at = limit
        .get("nextResetTime")
        .and_then(serde_json::Value::as_i64)
        .and_then(DateTime::<Utc>::from_timestamp_millis);
    Window {
        fraction,
        resets_at,
    }
}

/// Parse a quota response body.
///
/// # Errors
/// [`ZaiError::Malformed`] for non-JSON or an unexpected shape,
/// [`ZaiError::Rejected`] for `success:false`.
pub fn parse_quota(body: &str) -> Result<ZaiQuota, ZaiError> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|_| ZaiError::Malformed)?;
    if value.get("success").and_then(serde_json::Value::as_bool) == Some(false) {
        return Err(ZaiError::Rejected);
    }
    let limits = value
        .get("data")
        .and_then(|d| d.get("limits"))
        .and_then(serde_json::Value::as_array)
        .ok_or(ZaiError::Malformed)?;
    let mut quota = ZaiQuota::default();
    for limit in limits {
        if limit.get("type").and_then(serde_json::Value::as_str) != Some("CREDIT_LIMIT") {
            continue;
        }
        let slot = match limit.get("unit").and_then(serde_json::Value::as_i64) {
            Some(UNIT_HOURS) => &mut quota.five_hour,
            Some(UNIT_WEEKS) => &mut quota.weekly,
            _ => continue,
        };
        slot.get_or_insert_with(|| window_from(limit));
    }
    Ok(quota)
}

fn spent(window: Option<Window>) -> bool {
    window
        .and_then(|w| w.fraction)
        .is_some_and(|fraction| fraction >= 1.0)
}

/// Build the snapshot row for one seat. `reading` is `None` when unknown;
/// `marked_exhausted`/`mark_resets_at` come from the pool's bad-mark state.
#[must_use]
pub fn account_state(
    seat: &str,
    reading: Option<&ZaiQuota>,
    marked_exhausted: bool,
    mark_resets_at: Option<DateTime<Utc>>,
) -> TokenAccountState {
    let five = reading.and_then(|q| q.five_hour);
    let weekly = reading.and_then(|q| q.weekly);
    let weekly_spent = spent(weekly);
    let exhausted = marked_exhausted || weekly_spent || spent(five);
    let window_reset = if weekly_spent {
        weekly.and_then(|w| w.resets_at)
    } else {
        five.and_then(|w| w.resets_at)
            .or_else(|| weekly.and_then(|w| w.resets_at))
    };
    TokenAccountState {
        account: seat.to_string(),
        provider: ZAI_PROVIDER.to_string(),
        rank: None,
        usage_fraction: five.and_then(|w| w.fraction),
        usage_fraction_weekly: weekly.and_then(|w| w.fraction),
        limit_window_reset_at: window_reset.or(if marked_exhausted {
            mark_resets_at
        } else {
            None
        }),
        exhausted,
    }
}

#[derive(Debug)]
struct Entry {
    fetched: Instant,
    valid_for: Duration,
    failures: u32,
    reading: Option<ZaiQuota>,
}

/// Per-seat TTL cache with failure backoff.
#[derive(Debug, Default)]
pub struct ZaiCache {
    entries: HashMap<String, Entry>,
}

impl ZaiCache {
    /// `Some(reading)` when `seat` need not be polled now (fresh good reading,
    /// or inside a failure backoff, where the reading is `None`/unknown).
    #[must_use]
    pub fn cached(&self, seat: &str, now: Instant) -> Option<Option<ZaiQuota>> {
        let entry = self.entries.get(seat)?;
        (now.saturating_duration_since(entry.fetched) < entry.valid_for).then_some(entry.reading)
    }

    /// Record a poll outcome.
    pub fn store(&mut self, seat: &str, now: Instant, outcome: Result<ZaiQuota, ZaiError>) {
        let failures = self.entries.get(seat).map_or(0, |e| e.failures);
        let entry = match outcome {
            Ok(quota) => Entry {
                fetched: now,
                valid_for: TTL,
                failures: 0,
                reading: Some(quota),
            },
            Err(_) => {
                let failures = failures.saturating_add(1);
                let backoff = TTL
                    .saturating_mul(1u32 << failures.min(10))
                    .min(MAX_BACKOFF);
                Entry {
                    fetched: now,
                    valid_for: backoff.max(TTL),
                    failures,
                    reading: None,
                }
            }
        };
        self.entries.insert(seat.to_string(), entry);
    }

    /// Forget seats that are no longer provisioned.
    pub fn retain(&mut self, seats: &[String]) {
        self.entries.retain(|seat, _| seats.contains(seat));
    }
}

async fn fetch(client: &reqwest::Client, key: &str) -> Result<ZaiQuota, ZaiError> {
    fetch_from(client, QUOTA_URL, key).await
}

/// Test seam: the real fetch path against an injected `url`. Production only
/// ever calls it with the fixed [`QUOTA_URL`] (there is deliberately no env or
/// config override, so the key cannot be redirected to another host).
async fn fetch_from(client: &reqwest::Client, url: &str, key: &str) -> Result<ZaiQuota, ZaiError> {
    let response = client
        .get(url)
        .header(reqwest::header::AUTHORIZATION, key)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                ZaiError::Timeout
            } else {
                ZaiError::Transport
            }
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(ZaiError::Status(status.as_u16()));
    }
    let body = response.text().await.map_err(|e| {
        if e.is_timeout() {
            ZaiError::Timeout
        } else {
            ZaiError::Transport
        }
    })?;
    parse_quota(&body)
}

static CACHE: Mutex<Option<ZaiCache>> = Mutex::new(None);

/// A provisioned seat: its non-secret name, its key, and its bad-mark facts.
/// `key` is `None` when the credential could not be read: the seat still gets
/// an UNKNOWN row, but no request is attempted for it.
pub struct Seat {
    pub name: String,
    pub key: Option<String>,
    pub marked_exhausted: bool,
    pub mark_resets_at: Option<DateTime<Utc>>,
}

/// Rows for `seats`, polling only the ones whose cache entry lapsed. Returns
/// at once (no client, no request) when there are no seats.
pub async fn sample(seats: Vec<Seat>) -> Vec<TokenAccountState> {
    if seats.is_empty() {
        return Vec::new();
    }
    let names: Vec<String> = seats.iter().map(|s| s.name.clone()).collect();
    let now = Instant::now();
    let mut readings: HashMap<String, Option<ZaiQuota>> = HashMap::new();
    let mut due = Vec::new();
    {
        let mut guard = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
        let cache = guard.get_or_insert_with(ZaiCache::default);
        cache.retain(&names);
        for seat in &seats {
            if seat.key.is_none() {
                readings.insert(seat.name.clone(), None);
                continue;
            }
            match cache.cached(&seat.name, now) {
                Some(reading) => {
                    readings.insert(seat.name.clone(), reading);
                }
                None => due.push(seat),
            }
        }
    }
    if !due.is_empty() {
        if let Ok(client) = reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() {
            let handles: Vec<_> = due
                .iter()
                .map(|seat| {
                    let client = client.clone();
                    let key = seat.key.clone().unwrap_or_default();
                    tokio::spawn(async move { fetch(&client, &key).await })
                })
                .collect();
            let mut outcomes = Vec::with_capacity(handles.len());
            for handle in handles {
                outcomes.push(handle.await.unwrap_or(Err(ZaiError::Transport)));
            }
            let mut guard = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
            let cache = guard.get_or_insert_with(ZaiCache::default);
            for (seat, outcome) in due.iter().zip(outcomes) {
                if let Err(error) = &outcome {
                    log::debug!("zai quota poll for seat {}: {error}", seat.name);
                }
                readings.insert(seat.name.clone(), outcome.as_ref().ok().copied());
                cache.store(&seat.name, Instant::now(), outcome);
            }
        }
    }
    seats
        .iter()
        .map(|seat| {
            let reading = readings.get(&seat.name).copied().flatten();
            account_state(&seat.name, reading.as_ref(), seat.marked_exhausted, seat.mark_resets_at)
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "zai_tests.rs"]
mod tests;
