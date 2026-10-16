//! Whether the self-update loop is making progress at all (Issue #9736).
//!
//! The pre-#9736 `auto_update` health section went Degraded for a terminal
//! error, a wrong-repo streak, or a stale *source checkout* that was also
//! blocked. A host with no checkout whose release fetch backed off every tick
//! therefore reported Green indefinitely, and nothing looked at how long ago
//! the loop last *succeeded*. This module keeps the two clocks that close that
//! gap and the pure assessment over them:
//!
//! - **Last successful check.** A tick counts only when a release resolved
//!   (up to date, newer, or even a wrong-repo answer: the forge replied) or the
//!   source path reached a stale/current decision. A network error, an
//!   unresolved platform or a panicked tick is an *attempt*, never freshness.
//! - **Pending age.** When the newest applicable release is newer than the
//!   running daemon, the time since that was *first* seen. A newer release
//!   replacing the target does not reset it (the clock follows "behind", not
//!   "behind this tag"); it clears only on a successful check that finds the
//!   daemon current. A failed check leaves it standing.
//!
//! Identity is the daemon's own (`CARGO_PKG_VERSION`, filled in by the
//! daemon), so a new binary on disk or a newer CLI cannot make an old process
//! look converged.
//!
//! # Thresholds
//!
//! Defaults are multiples of the check interval so a normal cadence never trips
//! them (`env > config > default`, see [`Thresholds::resolve`]):
//!
//! | Setting | Default | Meaning |
//! |---|---|---|
//! | `overdueSecs` / `LOOM_AUTO_UPDATE_OVERDUE_SECS` | `4 × interval` | no successful check for this long, or none since start |
//! | `convergeDeadlineSecs` / `LOOM_AUTO_UPDATE_CONVERGE_DEADLINE_SECS` | `6 × settle + 4 × interval` | a newer release unapplied this long |
//!
//! The converge default is the settle ceiling (the longest a steady release
//! stream may legitimately defer a roll) plus the overdue margin, so settling
//! and draining are never findings; only a roll that should have started and
//! did not is.
//!
//! # Clocks
//!
//! Times are wall-clock UTC and persisted ([`FreshnessRecord`]). A time in the
//! future (the clock stepped back) is read as `now`, an age is never negative,
//! and a step forward can at worst raise one finding, which clears at the next
//! successful tick.
//!
//! # Alerts
//!
//! One incident, one alert: [`Freshness::edge`] reports [`Edge::Opened`] on the
//! tick the finding first appears and [`Edge::Recovered`] when it clears, and
//! nothing in between. The loop turns those into one ERROR log line and one
//! loom-ui alert each; no notification service is added.

use crate::auto_update::{ArtifactResolution, UpdateCheck};
use crate::types::DaemonStatusReport;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Overdue-check default, in check intervals.
const OVERDUE_INTERVALS: u32 = 4;
/// The settle ceiling multiplier (`SETTLE_CEILING_MULTIPLIER` in the parent).
const SETTLE_CEILING_INTERVALS: u32 = 6;

/// Env override for [`Thresholds::overdue_after`].
pub const OVERDUE_ENV: &str = "LOOM_AUTO_UPDATE_OVERDUE_SECS";
/// Env override for [`Thresholds::converge_deadline`].
pub const CONVERGE_DEADLINE_ENV: &str = "LOOM_AUTO_UPDATE_CONVERGE_DEADLINE_SECS";

/// Resolved thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    /// The check cadence (for the next-attempt estimate).
    pub interval: Duration,
    /// No successful check for this long (or none since start) is a finding.
    pub overdue_after: Duration,
    /// A newer release unapplied for this long is a finding.
    pub converge_deadline: Duration,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self::from_defaults(
            Duration::from_secs(super::DEFAULT_AUTO_UPDATE_INTERVAL_SECS),
            Duration::from_secs(super::DEFAULT_AUTO_UPDATE_SETTLE_SECS),
        )
    }
}

impl Thresholds {
    fn from_defaults(interval: Duration, settle: Duration) -> Self {
        Self {
            interval,
            overdue_after: interval.saturating_mul(OVERDUE_INTERVALS),
            converge_deadline: settle
                .saturating_mul(SETTLE_CEILING_INTERVALS)
                .saturating_add(interval.saturating_mul(OVERDUE_INTERVALS)),
        }
    }

    /// Resolve with **env > config > default**; a zero or unparseable value
    /// falls through to the next tier.
    #[must_use]
    pub fn resolve(config: &super::AutoUpdateConfig, interval: Duration, settle: Duration) -> Self {
        let env = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|&s| s > 0)
        };
        let base = Self::from_defaults(interval, settle);
        Self {
            interval,
            overdue_after: env(OVERDUE_ENV)
                .or(config.overdue_secs)
                .map_or(base.overdue_after, Duration::from_secs),
            converge_deadline: env(CONVERGE_DEADLINE_ENV)
                .or(config.converge_deadline_secs)
                .map_or(base.converge_deadline, Duration::from_secs),
        }
    }
}

/// What the status report carries (and `health` assesses).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshnessReport {
    /// The daemon's own running version.
    pub running_version: String,
    /// When this loop started tracking (the startup allowance's origin).
    pub tracking_since: DateTime<Utc>,
    /// The last tick that obtained a real answer.
    pub last_success: Option<DateTime<Utc>>,
    /// The newest applicable release newer than the running daemon.
    pub pending_target: Option<String>,
    /// When a newer release was first seen and not yet applied.
    pub pending_since: Option<DateTime<Utc>>,
    /// When the next attempt is expected (last tick + interval).
    pub next_check: Option<DateTime<Utc>>,
    /// Why the loop is not converging (the latest tick note).
    pub blocking_reason: Option<String>,
    /// [`Thresholds::overdue_after`], seconds.
    pub overdue_after_secs: u64,
    /// [`Thresholds::converge_deadline`], seconds.
    pub converge_deadline_secs: u64,
}

/// `at - since`, never negative (a `since` in the future is read as `at`).
fn age(at: DateTime<Utc>, since: DateTime<Utc>) -> Duration {
    (at - since).to_std().unwrap_or(Duration::ZERO)
}

fn human(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}m", s / 60)
    }
}

impl FreshnessReport {
    /// The degraded summary at `at`, or `None` when progress is fine. The one
    /// assessment `health` and the loop's alert share. The caller has already
    /// excluded a disabled loop.
    #[must_use]
    pub fn finding(&self, at: DateTime<Utc>) -> Option<String> {
        let why = self
            .blocking_reason
            .as_deref()
            .filter(|r| !r.is_empty())
            .map_or(String::new(), |r| format!("; latest tick: {r}"));
        let overdue = Duration::from_secs(self.overdue_after_secs);
        match self.last_success {
            None if age(at, self.tracking_since) > overdue => {
                return Some(format!(
                    "auto_update has not completed a successful release check in {} since it \
                     started tracking (allowance {}){why}",
                    human(age(at, self.tracking_since)),
                    human(overdue)
                ));
            }
            Some(last) if age(at, last) > overdue => {
                return Some(format!(
                    "auto_update's last successful release check was {} ago (overdue after {}); \
                     errors do not count as a check{why}",
                    human(age(at, last)),
                    human(overdue)
                ));
            }
            _ => {}
        }
        let (Some(target), Some(since)) = (&self.pending_target, self.pending_since) else {
            return None;
        };
        let deadline = Duration::from_secs(self.converge_deadline_secs);
        (age(at, since) > deadline).then(|| {
            format!(
                "auto_update: release {target} has been newer than the running daemon {} for {} \
                 (deadline {}){why}",
                self.running_version,
                human(age(at, since)),
                human(deadline)
            )
        })
    }
}

/// Persisted shape of [`Freshness`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshnessRecord {
    /// See [`FreshnessReport::tracking_since`].
    pub tracking_since: Option<DateTime<Utc>>,
    /// See [`FreshnessReport::last_success`].
    pub last_success: Option<DateTime<Utc>>,
    /// See [`FreshnessReport::pending_target`].
    pub pending_target: Option<String>,
    /// See [`FreshnessReport::pending_since`].
    pub pending_since: Option<DateTime<Utc>>,
    /// When the standing incident was alerted, so a restart does not re-alert.
    pub alerted_at: Option<DateTime<Utc>>,
}

/// A change in the standing incident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edge {
    /// The finding appeared: alert once.
    Opened(String),
    /// It cleared after being alerted: signal recovery once.
    Recovered,
}

/// The loop's freshness bookkeeping.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Freshness {
    rec: FreshnessRecord,
    thresholds: Thresholds,
}

/// What one tick learned, from [`classify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// A real answer was obtained (see the module doc).
    pub success: bool,
    /// The release newer than the running daemon, when one is known.
    pub pending: Option<String>,
}

/// Derive an [`Observation`]. `chases` is false on a fleet host, which moves
/// with the floor rather than the newest release, so it has no "newer release
/// unapplied" finding of its own (the floor surfaces cover it).
#[must_use]
pub fn classify(artifact: &ArtifactResolution, check: &UpdateCheck, chases: bool) -> Observation {
    let success =
        matches!(artifact, ArtifactResolution::Resolved(_)) || check.update_available.is_some();
    let pending = if !chases {
        None
    } else {
        match artifact {
            ArtifactResolution::Resolved(info) => {
                artifact.is_actionable().then(|| info.version.clone())
            }
            ArtifactResolution::Unresolved(_) => {
                (check.update_available == Some(true)).then(|| {
                    check
                        .source_commit
                        .as_ref()
                        .map_or_else(|| "source checkout".to_string(), |c| format!("source {c}"))
                })
            }
        }
    };
    Observation { success, pending }
}

impl Freshness {
    /// Set the thresholds (every tick, so a config reload takes effect) and
    /// start tracking on first use.
    pub fn set_tuning(&mut self, thresholds: Thresholds, now: DateTime<Utc>) {
        self.thresholds = thresholds;
        self.rec.tracking_since.get_or_insert(now);
    }

    /// Fold in one tick's observation.
    pub fn observe(&mut self, now: DateTime<Utc>, obs: &Observation) {
        if !obs.success {
            return; // an attempt, not an answer: nothing is learned
        }
        self.rec.last_success = Some(now);
        match &obs.pending {
            Some(target) => {
                // Keep the origin across target changes; the target is a label.
                self.rec.pending_since.get_or_insert(now);
                self.rec.pending_target = Some(target.clone());
            }
            None => {
                self.rec.pending_since = None;
                self.rec.pending_target = None;
            }
        }
    }

    /// The report published after a tick.
    #[must_use]
    pub fn report(&self, last_tick: DateTime<Utc>, note: &str) -> FreshnessReport {
        FreshnessReport {
            running_version: env!("CARGO_PKG_VERSION").to_string(),
            tracking_since: self.rec.tracking_since.unwrap_or(last_tick),
            last_success: self.rec.last_success,
            pending_target: self.rec.pending_target.clone(),
            pending_since: self.rec.pending_since,
            next_check: chrono::Duration::from_std(self.thresholds.interval)
                .ok()
                .map(|i| last_tick + i),
            blocking_reason: Some(note.to_string()),
            overdue_after_secs: self.thresholds.overdue_after.as_secs(),
            converge_deadline_secs: self.thresholds.converge_deadline.as_secs(),
        }
    }

    /// Compare the finding with the incident already alerted.
    pub fn edge(&mut self, at: DateTime<Utc>, report: &FreshnessReport) -> Option<Edge> {
        match (report.finding(at), self.rec.alerted_at) {
            (Some(summary), None) => {
                self.rec.alerted_at = Some(at);
                Some(Edge::Opened(summary))
            }
            (None, Some(_)) => {
                self.rec.alerted_at = None;
                Some(Edge::Recovered)
            }
            _ => None,
        }
    }

    /// The record to persist.
    #[must_use]
    pub fn record(&self) -> FreshnessRecord {
        self.rec.clone()
    }

    /// Restore a saved record. Times after `now` read as `now`. The pending
    /// clock is kept only when `same_binary`: a different binary means a roll
    /// completed, which ends convergence tracking.
    pub fn restore(&mut self, saved: FreshnessRecord, now: DateTime<Utc>, same_binary: bool) {
        let clamp = |t: Option<DateTime<Utc>>| t.map(|t| t.min(now));
        self.rec = FreshnessRecord {
            tracking_since: clamp(saved.tracking_since),
            last_success: clamp(saved.last_success),
            pending_target: saved.pending_target.filter(|_| same_binary),
            pending_since: clamp(saved.pending_since).filter(|_| same_binary),
            alerted_at: clamp(saved.alerted_at),
        };
        if self.rec.pending_since.is_none() {
            self.rec.pending_target = None;
        }
    }
}

impl super::AutoUpdateState {
    /// Log and post the loop-level alert for a freshness incident that opened
    /// or recovered on this tick (once each; see the module doc).
    pub(super) fn alert_on_freshness_edge(&mut self, snapshot: &super::AutoUpdateStatusSnapshot) {
        use crate::loom_ui_alerts::{Alert, Severity, State};
        const RULE: &str = "auto-update.stalled";
        let Some(report) = &snapshot.freshness else {
            return;
        };
        match self.freshness.edge(Utc::now(), report) {
            Some(Edge::Opened(summary)) => {
                log::error!("auto_update: {summary}");
                crate::loom_ui_alerts::post(
                    Alert::new(
                        RULE,
                        "auto_update",
                        "Self-update is not making progress on this host",
                        Severity::Warning,
                        State::Open,
                    )
                    .with_detail(summary),
                );
            }
            Some(Edge::Recovered) => {
                log::info!("auto_update: progress resumed (successful check, no overdue release)");
                crate::loom_ui_alerts::post(Alert::new(
                    RULE,
                    "auto_update",
                    "Self-update is making progress again",
                    Severity::Warning,
                    State::Resolved,
                ));
            }
            None => {}
        }
    }
}

/// The `health` side: the finding for a status report at `at`, or `None`.
#[must_use]
pub fn finding_for(status: &DaemonStatusReport, at: DateTime<Utc>) -> Option<String> {
    status.auto_update_freshness.as_ref()?.finding(at)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
