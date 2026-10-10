//! The failed-roll guard on the tick side (Issue #10880 item 2).
//!
//! A roll persisted nothing about itself before its restart. A host that came
//! back on the binary that armed the roll (the new one failed before its first
//! tick, or a rollback brought the old one back) found the same target still
//! above it on its first tick and fetched and armed it again at once. For a
//! floor-driven target that happened whatever the settle clocks said, because a
//! floor roll does not consult settle.
//!
//! # The record
//!
//! [`RollAttempt`] is written into `auto_update_state.json` (the
//! `roll_attempt` key, schema version unchanged) **before** a version roll is
//! armed. The rules:
//!
//! 1. **Arm.** The same target counts up (`attempts += 1`); any other target
//!    replaces the record with `attempts = 1`. A refused arm is undone. A
//!    source rebuild is not recorded: it has no release identity to hold back.
//! 2. **Judge on load.** The record is restored whatever binary saved it. A
//!    process running a version **below** the record's has watched an attempt
//!    fail: it sets `not_before = now + delay(attempts)` and logs a WARN.
//!    Otherwise the record is inert (and kept, rule 5).
//! 3. **Gate.** A tick whose target is the record's, before `not_before`,
//!    neither fetches nor arms it. This runs beside the terminal and backoff
//!    gates, so before the floor's settle skip: it applies to every
//!    `target_source`, floor included. The tick reports `defer`.
//! 4. **Backoff, bounded, never terminal.** [`delay`]: 15 min doubling to a
//!    6 h ceiling, the same table as #10832's pause-side guard
//!    ([`super::pause_resume::attempt::delay`]). A different target (a newer
//!    release, or the same version re-published under a new checksum) is
//!    tried at once.
//! 5. **Confirm.** The first tick that ends with the running version at or
//!    above the record's, in a process up for at least [`STARTUP_GRACE`],
//!    clears it. A candidate that ticks once and dies keeps the record for the
//!    binary that comes back. (When #9735's health probation exists, its pass
//!    is the better clear signal.)
//! 6. **Clock safety.** On load a `not_before` later than `now + 6 h` becomes
//!    `now + 6 h`, and an arm time in the future becomes `now`.
//!
//! #10832's guard (`pause_resume::attempt`) stays as the pause-side backstop:
//! it is recorded when H5 observes a rollback and gates H3. This record gates
//! the tick before it fetches. Deleting `auto_update_state.json` clears it; a
//! manual update is not gated by it.
//!
//! # The alert
//!
//! While a roll is held by this gate, by fetch backoff after
//! [`ALERT_AFTER_FAILURES`] or more consecutive failures, or by a terminal
//! fetch failure, every tick raises [`RollHeld`]: at ERROR when the host is
//! below its fleet floor, at WARN otherwise. It is in the log, the `status`
//! note and the `auto_update.tick` record. Nothing is paused for it; dispatch
//! continues on the running version (#10712).
//!
//! A terminal fetch failure on a **floor** target is not final: it is retried
//! after the 6 h ceiling. Any other target keeps the existing terminal rule.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::floor_roll::{FloorVerdict, TargetSource};
use super::pause_resume::attempt;
use super::pause_roll::RollTarget;
use super::{AutoUpdateState, RebuildOutcome, TickDecision};
use crate::fleet_store::floor::parse_triple;
pub use crate::telemetry::kinds::auto_update_tick::RollHeld;

/// The longest delay, and the retry interval after a terminal fetch failure
/// on a floor target.
pub const CEILING: Duration = Duration::from_secs(6 * 3600);

/// How long a process must be up before a tick on the record's version clears
/// it (the daemon's startup grace).
pub const STARTUP_GRACE: Duration =
    Duration::from_secs(crate::daemon_install_state::DEFAULT_STARTUP_GRACE_SECS);

/// Consecutive retryable fetch failures from which a backing-off roll alerts.
pub const ALERT_AFTER_FAILURES: u32 = 3;

/// A version roll this host armed, as `auto_update_state.json` carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollAttempt {
    /// The tick's target identity: `artifact:<version>:<sha or tag>`.
    pub target: String,
    /// The version rolled to.
    pub version: String,
    /// The release tag rolled to.
    pub tag: String,
    /// The trigger's `target_source`: `floor`, `repo_ahead` or `autoupdate`.
    pub source: String,
    /// `<version>+<commit>` of the process that armed it (diagnostic).
    pub from_binary: String,
    /// Arms of this target so far.
    pub attempts: u32,
    /// The first arm of this target.
    pub first_armed_at: DateTime<Utc>,
    /// The latest arm.
    pub last_armed_at: DateTime<Utc>,
    /// No fetch or arm of this target before this time. Set once an attempt
    /// is judged failed; cleared by the next arm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<DateTime<Utc>>,
    /// Why the last attempt is judged failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<String>,
}

/// The delay after the `attempts`-th failed attempt on one target.
#[must_use]
pub fn delay(attempts: u32) -> Duration {
    attempt::delay(attempts).to_std().unwrap_or(CEILING)
}

/// `a >= b` as plain `X.Y.Z` versions; `false` when either does not parse.
fn at_or_above(a: &str, b: &str) -> bool {
    let bare = |v: &str| parse_triple(v.trim().trim_start_matches('v'));
    matches!((bare(a), bare(b)), (Some(a), Some(b)) if a >= b)
}

fn chrono(d: Duration) -> chrono::Duration {
    chrono::Duration::from_std(d).unwrap_or_else(|_| chrono::Duration::hours(6))
}

fn rfc3339(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The record and its in-memory companions, held by [`AutoUpdateState`].
#[derive(Debug)]
pub struct AttemptGuard {
    record: Option<RollAttempt>,
    /// The monotonic twin of `record.not_before`, set when this process
    /// judged (or restored) a failed attempt.
    hold_until: Option<Instant>,
    /// When this loop's state was built: the process's uptime for rule 5.
    started: Instant,
    /// When a floor target's fetch failed terminally (retried at [`CEILING`]).
    terminal_at: Option<Instant>,
    /// The last retryable or terminal fetch failure, for the alert.
    last_fetch_failure: Option<String>,
}

impl Default for AttemptGuard {
    fn default() -> Self {
        Self {
            record: None,
            hold_until: None,
            started: Instant::now(),
            terminal_at: None,
            last_fetch_failure: None,
        }
    }
}

impl AttemptGuard {
    /// The record, as it is persisted.
    #[must_use]
    pub fn record(&self) -> Option<&RollAttempt> {
        self.record.as_ref()
    }

    /// Rule 1: record an arm of `target`. Returns the previous record, for
    /// [`Self::undo`] if the arm is refused.
    pub fn arm(&mut self, now_utc: DateTime<Utc>, armed: RollAttempt) -> Option<RollAttempt> {
        let previous = self.record.clone();
        let next = match self.record.take() {
            Some(rec) if rec.target == armed.target => RollAttempt {
                attempts: rec.attempts.saturating_add(1),
                first_armed_at: rec.first_armed_at,
                last_armed_at: now_utc,
                ..armed
            },
            _ => RollAttempt {
                attempts: 1,
                first_armed_at: now_utc,
                last_armed_at: now_utc,
                ..armed
            },
        };
        self.record = Some(next);
        self.hold_until = None;
        previous
    }

    /// Put back the record an [`Self::arm`] replaced: the arm was refused.
    pub fn undo(&mut self, previous: Option<RollAttempt>) {
        self.record = previous;
    }

    /// Rules 2 and 6: restore `saved` for a process running `running`, and
    /// describe it for the load line.
    pub fn restore(
        &mut self,
        saved: Option<RollAttempt>,
        now: Instant,
        now_utc: DateTime<Utc>,
        running: &str,
    ) -> String {
        self.hold_until = None;
        let Some(mut rec) = saved else {
            self.record = None;
            return String::new();
        };
        let latest = now_utc + chrono(CEILING);
        rec.first_armed_at = rec.first_armed_at.min(now_utc);
        rec.last_armed_at = rec.last_armed_at.min(now_utc);
        rec.not_before = rec.not_before.map(|at| at.min(latest));
        let note = if attempt::below(running, &rec.version) {
            if rec.not_before.is_none() {
                rec.not_before = Some(now_utc + chrono(delay(rec.attempts)));
                rec.last_failure = Some(format!(
                    "the daemon came back on {running} after arming the roll to {} (attempt {})",
                    rec.tag, rec.attempts
                ));
                log::warn!(
                    "auto_update: the roll to {} did not take: this process runs {running} after \
                     attempt {} armed it, so no fetch or roll to it before {} (failed-roll backoff)",
                    rec.target,
                    rec.attempts,
                    rec.not_before.map(rfc3339).unwrap_or_default()
                );
            }
            self.hold_until = rec
                .not_before
                .and_then(|at| (at - now_utc).to_std().ok())
                .and_then(|left| now.checked_add(left));
            format!(
                "; failed roll to {} (attempt {}), held until {}",
                rec.target,
                rec.attempts,
                rec.not_before.map(rfc3339).unwrap_or_default()
            )
        } else {
            format!(
                "; roll attempt to {} (attempt {}) kept until this binary has run {}s",
                rec.target,
                rec.attempts,
                STARTUP_GRACE.as_secs()
            )
        };
        self.record = Some(rec);
        note
    }

    /// Rule 3: the record and the time left when it holds `target` at `now`.
    fn holding(&self, target: &str, now: Instant) -> Option<(&RollAttempt, Duration)> {
        let rec = self.record.as_ref().filter(|r| r.target == target)?;
        let until = self.hold_until.filter(|until| now < *until)?;
        Some((rec, until - now))
    }

    /// Rule 5: clear the record once `running` has reached it in a process up
    /// for [`STARTUP_GRACE`]. Returns whether it was cleared.
    pub fn confirm(&mut self, now: Instant, running: &str) -> bool {
        let Some(rec) = &self.record else {
            return false;
        };
        if !at_or_above(running, &rec.version)
            || now.saturating_duration_since(self.started) < STARTUP_GRACE
        {
            return false;
        }
        log::info!(
            "auto_update: the roll to {} took (running {running}); its attempt record is cleared",
            rec.target
        );
        self.record = None;
        self.hold_until = None;
        true
    }

    /// Note a fetch's outcome for the alert and the floor's terminal retry.
    pub fn note_outcome(&mut self, now: Instant, outcome: &RebuildOutcome) {
        match outcome {
            RebuildOutcome::Success => {
                self.last_fetch_failure = None;
                self.terminal_at = None;
            }
            RebuildOutcome::Retryable(why) => self.last_fetch_failure = Some(why.clone()),
            RebuildOutcome::Terminal(why) => {
                self.last_fetch_failure = Some(why.clone());
                self.terminal_at = Some(now);
            }
        }
    }

    #[cfg(test)]
    pub(super) fn set_started(&mut self, started: Instant) {
        self.started = started;
    }

    #[cfg(test)]
    pub(super) fn hold_until(&self) -> Option<Instant> {
        self.hold_until
    }

    #[cfg(test)]
    pub(super) fn set_hold_until(&mut self, at: Option<Instant>) {
        self.hold_until = at;
    }
}

impl AutoUpdateState {
    /// Rule 3 and the floor's terminal retry, then the terminal and backoff
    /// gates. `source` is who chose this tick's target.
    pub(super) fn roll_gates(
        &mut self,
        now: Instant,
        source: TargetSource,
    ) -> Option<TickDecision> {
        let tracked = self.tracked_target.as_deref().unwrap_or_default();
        if let Some((rec, left)) = self.attempt.holding(tracked, now) {
            return Some(TickDecision::Skip(format!(
                "the last roll to {} did not take ({} attempt(s): {}) — not fetching or arming it \
                 again before {}s from now (failed-roll backoff: 15 min doubling to 6 h, never \
                 terminal; a newer release is tried at once)",
                rec.target,
                rec.attempts,
                rec.last_failure.as_deref().unwrap_or("no detail"),
                left.as_secs()
            )));
        }
        match (&self.terminal_reason, source) {
            (None, _) => self.attempt.terminal_at = None,
            (Some(reason), TargetSource::Floor) => {
                let at = *self.attempt.terminal_at.get_or_insert(now);
                let left = CEILING.saturating_sub(now.saturating_duration_since(at));
                if !left.is_zero() {
                    return Some(TickDecision::Skip(format!(
                        "terminal fetch failure on the floor target — retrying in ~{}s (the {}s \
                         ceiling; a floor roll is never abandoned): {reason}",
                        left.as_secs(),
                        CEILING.as_secs()
                    )));
                }
                log::warn!(
                    "auto_update: retrying the floor target {}s after its terminal fetch failure \
                     (a floor roll is never abandoned): {reason}",
                    CEILING.as_secs()
                );
                self.terminal_reason = None;
                self.attempt.terminal_at = None;
            }
            (Some(_), _) => {}
        }
        self.terminal_or_backoff_gate(now)
    }

    /// Rule 1 for a version roll about to be armed to `target` at `tag`.
    /// Returns the previous record, for [`AttemptGuard::undo`].
    pub(super) fn arm_attempt(&mut self, target: &RollTarget, tag: &str) -> Option<RollAttempt> {
        let now_utc = Utc::now();
        let armed = RollAttempt {
            target: self.tracked_target.clone().unwrap_or_default(),
            version: target.to_version.clone().unwrap_or_default(),
            tag: tag.to_string(),
            source: target.source.as_str().to_string(),
            from_binary: super::persisted_state::running_binary(),
            attempts: 1,
            first_armed_at: now_utc,
            last_armed_at: now_utc,
            not_before: None,
            last_failure: None,
        };
        self.attempt.arm(now_utc, armed)
    }

    /// The alert for a release roll this tick left held, if any.
    #[must_use]
    pub(super) fn held_roll(&self, now: Instant, now_utc: DateTime<Utc>) -> Option<RollHeld> {
        let target = self
            .tracked_target
            .as_deref()
            .filter(|t| t.starts_with("artifact:"))?;
        let floor = match self.floor.verdict() {
            FloorVerdict::Below { floor, .. } => Some(floor.clone()),
            _ => None,
        };
        let at = |until: Instant| now_utc + chrono(until.saturating_duration_since(now));
        let (cause, attempts, last_failure, next_retry) =
            if let Some((rec, left)) = self.attempt.holding(target, now) {
                (
                    "failed_roll",
                    rec.attempts,
                    rec.last_failure.clone(),
                    Some(now_utc + chrono(left)),
                )
            } else if let Some(reason) = &self.terminal_reason {
                let next = self
                    .attempt
                    .terminal_at
                    .filter(|_| floor.is_some())
                    .map(|t| at(t + CEILING));
                let failures = self.consecutive_failures.saturating_add(1);
                ("fetch_terminal", failures, Some(reason.clone()), next)
            } else if self.consecutive_failures >= ALERT_AFTER_FAILURES
                && self.backoff_until.is_some_and(|until| now < until)
            {
                let next = self.backoff_until.map(at);
                let why = self.attempt.last_fetch_failure.clone();
                ("fetch_backoff", self.consecutive_failures, why, next)
            } else {
                return None;
            };
        Some(RollHeld {
            floor,
            running: self.floor.running().to_string(),
            target: target.to_string(),
            cause: cause.to_string(),
            attempts,
            last_failure,
            next_retry,
        })
    }
}

/// The alert's text: the log line and the `status` note's suffix.
#[must_use]
pub fn held_note(held: &RollHeld) -> String {
    let RollHeld {
        floor,
        running,
        target,
        cause,
        attempts,
        last_failure,
        next_retry,
    } = held;
    let why = match cause.as_str() {
        "failed_roll" => "the last roll to it did not take",
        "fetch_terminal" => "its fetch failed terminally",
        _ => "its fetch keeps failing",
    };
    let retry = next_retry.map_or_else(
        || "not retried until a new release".to_string(),
        |at| format!("next retry {}", rfc3339(at)),
    );
    let failure = last_failure.as_deref().unwrap_or("no detail");
    match floor {
        Some(floor) => format!(
            "FLOOR ROLL FAILING: running {running} is below the fleet floor {floor}, and the roll \
             to {target} is held because {why} ({attempts} attempt(s); last failure: {failure}); \
             {retry}. DISPATCH CONTINUES on {running}: the floor never refuses work."
        ),
        None => format!(
            "ROLL HELD: the roll to {target} (running {running}) is held because {why} \
             ({attempts} attempt(s); last failure: {failure}); {retry}. Dispatch continues."
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "roll_attempt_tests.rs"]
mod tests;
