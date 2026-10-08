//! The ETA authority's emit heartbeat (#10898).
//!
//! On 2026-10-07/08 the authority covered two repos for ~31 hours and emitted
//! no `eta.estimate` / `eta.snapshot` (no OTLP exporter), while every
//! liveness signal read healthy. This module is the small, local record that
//! answers "when did this authority last *emit*?": the authority writes it
//! after every ETA pass, and readers (`loom-daemon eta doctor`, the
//! `fleetAlert` condition, the `loom.eta.authority.*` gauges) age it on read
//! against their own clock, so a stopped writer cannot keep itself fresh.
//!
//! It lives beside the other local ETA health files
//! (`.loom/state/eta/health/last-emit.json`, see [`super::health`]). Unlike the
//! captain-gauges heartbeat it is **not** published to the fleet store: the
//! SigNoz rule (`eta-not-emitted.json`) is the cross-host detector, this file
//! is the on-host one. Failover to another host stays out of scope.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Emission silence that counts as "authority silent": the 2 h of the issue.
pub const SILENT_AFTER: Duration = Duration::hours(2);

/// What the authority recorded after its latest ETA pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    /// The authority host that wrote it.
    pub host: String,
    /// When the pass ran.
    pub pass_at: DateTime<Utc>,
    /// When records were last actually offered to an exporter; `None` = never.
    #[serde(default)]
    pub last_emit_at: Option<DateTime<Utc>>,
    /// Repos the pass covered.
    pub repos_covered: u64,
    /// Review-stage PRs open across those repos at that pass.
    pub open_prs: u64,
    /// Every in-scope repo's review listing succeeded, so `open_prs` is an
    /// observation. `false` = some listing failed: the count is unknown, not
    /// zero. Absent (an older file) reads as `false`, the conservative side.
    #[serde(default)]
    pub complete: bool,
}

/// `<root>/.loom/state/eta/health/last-emit.json`.
#[must_use]
pub fn path(root: &Path) -> PathBuf {
    super::health::dir(root).join("last-emit.json")
}

/// Fold one pass into the heartbeat: `emitted` records offered at `now`
/// refresh `last_emit_at`, otherwise the previous one is kept. `complete` is
/// whether the pass's listings all succeeded (see [`Heartbeat::complete`]).
#[must_use]
pub fn next(
    prev: Option<&Heartbeat>,
    host: &str,
    now: DateTime<Utc>,
    emitted: bool,
    repos_covered: u64,
    open_prs: u64,
    complete: bool,
) -> Heartbeat {
    let kept = prev.and_then(|p| p.last_emit_at);
    Heartbeat {
        host: host.to_string(),
        pass_at: now,
        last_emit_at: if emitted { Some(now) } else { kept },
        repos_covered,
        open_prs,
        complete,
    }
}

/// Persist `hb`; a failure is logged (health state never costs a pass).
pub fn write(root: &Path, hb: &Heartbeat) {
    let Ok(text) = serde_json::to_string_pretty(hb) else {
        return;
    };
    if let Err(e) = super::health::write_atomic(&path(root), &text) {
        log::warn!("eta health: writing last-emit.json failed: {e}");
    }
}

/// The heartbeat, if one was written and parses.
#[must_use]
pub fn read(root: &Path) -> Option<Heartbeat> {
    serde_json::from_str(&std::fs::read_to_string(path(root)).ok()?).ok()
}

/// When this process first asked; the grace origin for a heartbeat that is
/// older than the process (a restarted authority gets a fresh window).
#[must_use]
pub fn process_started() -> DateTime<Utc> {
    static STARTED: OnceLock<DateTime<Utc>> = OnceLock::new();
    *STARTED.get_or_init(Utc::now)
}

/// How long this authority has been silent, and whether that is too long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Silence {
    /// Seconds since the last emit (or since `started` when none is newer);
    /// `None` when nothing was ever recorded and no origin is known.
    pub age_secs: Option<i64>,
    /// Whether the last emit is older than the threshold.
    pub silent: bool,
    /// An emit was ever recorded.
    pub ever_emitted: bool,
}

/// Age the heartbeat at `now` against `threshold`. The silence clock starts at
/// the later of the last emit and `started`, so a freshly (re)started
/// authority is not silent before it has had a window to emit. Pure.
#[must_use]
pub fn assess(
    hb: Option<&Heartbeat>,
    now: DateTime<Utc>,
    started: DateTime<Utc>,
    threshold: Duration,
) -> Silence {
    let last = hb.and_then(|h| h.last_emit_at);
    let origin = last.map_or(started, |t| t.max(started));
    let age = (now - origin).max(Duration::zero());
    Silence {
        age_secs: Some(age.num_seconds()),
        silent: age > threshold,
        ever_emitted: last.is_some(),
    }
}

/// Seconds since the last recorded emit, ignoring process start (the gauge
/// reading); `None` when never emitted.
#[must_use]
pub fn last_emit_age_secs(hb: Option<&Heartbeat>, now: DateTime<Utc>) -> Option<i64> {
    let at = hb?.last_emit_at?;
    Some((now - at).num_seconds().max(0))
}

#[cfg(test)]
#[path = "emit_heartbeat_tests.rs"]
mod tests;
