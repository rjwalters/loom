//! Bounded recovery with a circuit breaker (#5391).
//!
//! Before #5391 the watchdog only *reported* a confirmed-down daemon. Reporting
//! is not recovery: the 2026-07-26 incident was discovered hours later because
//! nothing acted on the signal. So a confirmed outage is now restarted under
//! bounded retries with exponential backoff, and escalated to a forge issue once
//! the attempt budget is spent.
//!
//! Every bound here exists because unbounded automatic restart is worse than
//! none. A daemon that dies on startup for a persistent reason — a bad config,
//! a port already held, a corrupt state file — would otherwise be relaunched
//! forever, burning the host and hiding the cause behind a wall of identical
//! log lines. The breaker makes the failure visible instead of loud.

use std::path::Path;

use super::consts::{
    DEFAULT_RECOVER_BACKOFF_CAP_SECS, DEFAULT_RECOVER_BACKOFF_SECS, DEFAULT_RECOVER_MAX_ATTEMPTS,
};
use super::env;

/// One outage episode, persisted across ticks.
///
/// The watchdog owns no long-lived process — launchd re-runs it every interval
/// — so everything it remembers between ticks lives in this file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    /// Unix seconds when the outage was first observed.
    pub down_since: u64,
    /// Consecutive ticks that have observed it.
    pub ticks: u64,
    /// Recovery attempts spent. When this reaches the budget the breaker opens.
    pub attempts: u64,
    /// Unix seconds of the last attempt, for the backoff comparison.
    pub last_attempt: u64,
}

/// Read the episode, or a zeroed state when none is recorded.
///
/// A malformed field reads as 0 rather than erroring: a scheduled tick must
/// never abort on a corrupt state file and leave the host with no detector.
#[must_use]
pub fn read(path: &Path) -> State {
    let Ok(text) = std::fs::read_to_string(path) else {
        return State::default();
    };
    let get = |key: &str| -> u64 {
        let prefix = format!("{key}=");
        text.lines()
            .find(|l| l.starts_with(&prefix))
            .and_then(|l| l[prefix.len()..].trim().parse().ok())
            .unwrap_or(0)
    };
    State {
        down_since: get("down_since"),
        ticks: get("ticks"),
        attempts: get("attempts"),
        last_attempt: get("last_attempt"),
    }
}

/// Persist the episode. Failures are swallowed, as the shell's `|| true` did —
/// losing the memo is better than aborting the tick that would have written it.
pub fn write(path: &Path, state: &State) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(
        path,
        format!(
            "down_since={}\nticks={}\nattempts={}\nlast_attempt={}\n",
            state.down_since, state.ticks, state.attempts, state.last_attempt
        ),
    );
}

/// End the episode.
///
/// Clears the escalation sentinel too, and that pairing is load-bearing: the
/// sentinel is what suppresses a duplicate forge issue for an outage already
/// escalated. Leaving it behind after a recovery means the NEXT outage is
/// silently treated as already-reported and never escalated at all.
pub fn clear(state_path: &Path, escalation_sentinel: &Path) {
    let _ = std::fs::remove_file(state_path);
    let _ = std::fs::remove_file(escalation_sentinel);
}

/// Seconds to wait before attempt `n` (1-based).
///
/// Doubles per attempt from the base, saturating at the cap. Attempt 1 is not
/// delayed — the first restart of a daemon that has just died should be
/// immediate, because the overwhelming majority of outages are transient and
/// waiting a minute to find that out helps nobody.
#[must_use]
pub fn backoff_for(n: u64, base_secs: u64, cap_secs: u64) -> u64 {
    let mut backoff = base_secs;
    let mut i = 1;
    while i < n {
        backoff = backoff.saturating_mul(2);
        if backoff >= cap_secs {
            return cap_secs;
        }
        i += 1;
    }
    backoff
}

/// The knobs, each falling back to its documented default on a malformed value.
pub struct Limits {
    pub enabled: bool,
    pub max_attempts: u64,
    pub backoff_secs: u64,
    pub backoff_cap_secs: u64,
}

impl Limits {
    #[must_use]
    pub fn from_env() -> Self {
        // A zero attempt budget would mean "never recover", which is what the
        // enable flag is for; the shell's `^[1-9][0-9]*$` guard rejects it, so
        // a typo'd 0 falls back to the default rather than silently disabling
        // recovery.
        let max_attempts = env::var("LOOM_WATCHDOG_RECOVER_MAX_ATTEMPTS")
            .filter(|s| {
                !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && !s.starts_with('0')
            })
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_RECOVER_MAX_ATTEMPTS);
        Self {
            enabled: !env::var("LOOM_WATCHDOG_AUTO_RECOVER").is_some_and(|v| env::is_false(&v)),
            max_attempts,
            backoff_secs: env::num(
                "LOOM_WATCHDOG_RECOVER_BACKOFF_SECS",
                DEFAULT_RECOVER_BACKOFF_SECS,
            ),
            backoff_cap_secs: env::num(
                "LOOM_WATCHDOG_RECOVER_BACKOFF_CAP_SECS",
                DEFAULT_RECOVER_BACKOFF_CAP_SECS,
            ),
        }
    }
}

/// Whether this tick may spend an attempt, and why not when it may not.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Spend attempt number `attempt`.
    Attempt { attempt: u64 },
    /// The breaker is open: the budget is spent.
    BreakerOpen { attempts: u64 },
    /// Backing off; `remaining` seconds to go before the next attempt.
    BackingOff { next_attempt: u64, remaining: u64 },
    /// Automatic recovery is switched off on this host.
    Disabled,
}

/// Decide whether to attempt recovery on this tick.
#[must_use]
pub fn decide(state: &State, limits: &Limits, now: u64) -> Decision {
    if !limits.enabled {
        return Decision::Disabled;
    }
    if state.attempts >= limits.max_attempts {
        // Open, and it stays open until a tick observes a healthy daemon (which
        // clears the state) or an operator deletes the file. It deliberately
        // does NOT reopen on a timer: a daemon that failed its whole budget is
        // failing for a reason that time alone does not fix.
        return Decision::BreakerOpen {
            attempts: state.attempts,
        };
    }
    let next = state.attempts + 1;
    let wait = backoff_for(next, limits.backoff_secs, limits.backoff_cap_secs);
    // Attempt 1 has nothing to back off from.
    if state.attempts > 0 {
        let elapsed = now.saturating_sub(state.last_attempt);
        if elapsed < wait {
            return Decision::BackingOff {
                next_attempt: next,
                remaining: wait - elapsed,
            };
        }
    }
    Decision::Attempt { attempt: next }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            enabled: true,
            max_attempts: 5,
            backoff_secs: 60,
            backoff_cap_secs: 1800,
        }
    }

    #[test]
    fn backoff_doubles_from_the_base_and_saturates_at_the_cap() {
        assert_eq!(backoff_for(1, 60, 1800), 60, "the first attempt is not delayed past the base");
        assert_eq!(backoff_for(2, 60, 1800), 120);
        assert_eq!(backoff_for(3, 60, 1800), 240);
        assert_eq!(backoff_for(4, 60, 1800), 480);
        assert_eq!(backoff_for(5, 60, 1800), 960);
        assert_eq!(backoff_for(6, 60, 1800), 1800, "capped");
        assert_eq!(backoff_for(99, 60, 1800), 1800, "stays capped, never overflows");
    }

    #[test]
    fn backoff_cannot_overflow_on_an_absurd_attempt_number() {
        // saturating_mul, not `*`: a corrupt state file naming attempt 2^40
        // must not panic a scheduled tick in release or wrap in debug.
        assert_eq!(backoff_for(u64::MAX, 60, 1800), 1800);
    }

    #[test]
    fn the_first_attempt_is_immediate() {
        let s = State::default();
        assert_eq!(decide(&s, &limits(), 1000), Decision::Attempt { attempt: 1 });
    }

    #[test]
    fn a_second_attempt_waits_for_the_backoff() {
        let s = State {
            attempts: 1,
            last_attempt: 1000,
            ..State::default()
        };
        // backoff_for(2) == 120, so at +60 there are 60 seconds left.
        assert_eq!(
            decide(&s, &limits(), 1060),
            Decision::BackingOff {
                next_attempt: 2,
                remaining: 60
            }
        );
        assert_eq!(decide(&s, &limits(), 1120), Decision::Attempt { attempt: 2 });
    }

    #[test]
    fn the_breaker_opens_when_the_budget_is_spent_and_does_not_reopen_on_time() {
        let s = State {
            attempts: 5,
            last_attempt: 1000,
            ..State::default()
        };
        assert_eq!(decide(&s, &limits(), 1000), Decision::BreakerOpen { attempts: 5 });
        // A day later it is still open. Only a healthy tick or an operator
        // clears it — time alone does not fix a daemon that spent its budget.
        assert_eq!(decide(&s, &limits(), 1000 + 86_400), Decision::BreakerOpen { attempts: 5 });
    }

    #[test]
    fn disabling_recovery_short_circuits_everything() {
        let l = Limits {
            enabled: false,
            ..limits()
        };
        assert_eq!(decide(&State::default(), &l, 1000), Decision::Disabled);
    }

    #[test]
    fn a_corrupt_state_file_reads_as_a_fresh_episode_rather_than_aborting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("state");
        std::fs::write(&p, "down_since=notanumber\nticks=\ngarbage\n").expect("write");
        assert_eq!(read(&p), State::default());
    }

    #[test]
    fn a_missing_state_file_is_a_fresh_episode() {
        assert_eq!(read(Path::new("/nonexistent/state")), State::default());
    }

    #[test]
    fn round_trips_through_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("nested").join("state");
        let s = State {
            down_since: 111,
            ticks: 2,
            attempts: 3,
            last_attempt: 444,
        };
        write(&p, &s);
        assert_eq!(read(&p), s);
    }

    #[test]
    fn clearing_removes_the_escalation_sentinel_too() {
        // Load-bearing: the sentinel suppresses a duplicate issue for an
        // already-escalated outage. Left behind after recovery, the NEXT outage
        // reads as already-reported and is never escalated.
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("state");
        let sentinel = dir.path().join("sentinel");
        std::fs::write(&state, "ticks=1\n").expect("write");
        std::fs::write(&sentinel, "issue=1\n").expect("write");
        clear(&state, &sentinel);
        assert!(!state.exists());
        assert!(!sentinel.exists(), "a stale sentinel silences the next outage");
    }
}
