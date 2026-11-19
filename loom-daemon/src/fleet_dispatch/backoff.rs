//! When a worker asks the captain for work (#12817): capacity-driven
//! requests, with capped, jittered exponential backoff on empty answers.
//!
//! There is no fixed polling period. A worker asks when it has capacity,
//! keeps **one** request in flight, and after an **empty** answer waits an
//! exponentially growing, jittered delay before asking again. An empty
//! answer is the captain saying "nothing fits you now", not an error.
//!
//! # Schedule
//!
//! The nominal delay after the n-th consecutive empty answer is
//! `min(initial * 2^n, max)`: 30 s, 60 s, 120 s, 120 s, ... by default. The
//! actual delay is `nominal * (1 - jitter * u)` with `u` uniform in `[0, 1)`
//! drawn independently per host and per delay, so it never exceeds `max`
//! (the hard cap) and keeps its spread at the cap instead of putting every
//! host on the same instant. With the defaults (`jitter = 0.5`) the delays
//! are in `[15, 30)`, `[30, 60)`, then `[60, 120)` s. The first request after
//! start waits a random `[0, startup)` so a fleet restart does not synchronise.
//!
//! # Events
//!
//! - **Work received** ([`Requester::on_answer`] with work): the empty streak
//!   resets, and the next request is due at once (capacity may remain).
//! - **Empty answer**: the streak grows, the next request is scheduled.
//! - **Captain error** ([`Requester::on_error`]): neither. The streak is kept
//!   and the retry waits the *current* delay; the worker's fallback
//!   hysteresis ([`super::fallback`]) owns what a failure means.
//! - **Capacity** ([`Requester::on_capacity`]): no capacity stops requests.
//!   Repeated checks that show the same or less capacity change nothing and
//!   never reset the streak. A *rise* in free slots is a wake: the pending
//!   request comes forward to `now`, but never closer than [`Config::wake_floor`]
//!   (the smallest first delay, `initial * (1 - jitter)`) to the previous
//!   request, so a storm of slots freeing and filling cannot defeat the
//!   herd protection. Wakes coalesce into one pending flag; one in flight
//!   means a wake waits for its answer.
//!
//! Pure: time is a [`Mono`] and randomness a [`Rng`], both injected.
//!
//! # Configuration
//!
//! Live-reloadable keys under `fleetDispatch.request`, read by
//! [`Config::from_effective`]; an absent key takes its default and an invalid
//! one rejects the whole block (the caller keeps the last good config):
//!
//! | Key | Default | Validation |
//! |---|---|---|
//! | `initialBackoffSecs` | 30 | finite, `> 0` |
//! | `maxBackoffSecs` | 120 | finite, `>= initialBackoffSecs` |
//! | `jitter` | 0.5 | finite, `0 ..= 1` |
//! | `startupSecs` | 30 | finite, `0 ..= maxBackoffSecs` |
//!
//! A reload ([`Requester::set_config`]) keeps the empty streak, keeps the
//! scheduled instant but pulls it in to at most the new `max` from now, and
//! applies the new values to the next delay drawn.

use std::time::Duration;

use serde_json::Value;

use super::table::Mono;

/// `fleetDispatch.request.initialBackoffSecs`.
pub const INITIAL_KEY: &str = "fleetDispatch.request.initialBackoffSecs";
/// `fleetDispatch.request.maxBackoffSecs`.
pub const MAX_KEY: &str = "fleetDispatch.request.maxBackoffSecs";
/// `fleetDispatch.request.jitter`.
pub const JITTER_KEY: &str = "fleetDispatch.request.jitter";
/// `fleetDispatch.request.startupSecs`.
pub const STARTUP_KEY: &str = "fleetDispatch.request.startupSecs";

/// Source of uniform randomness in `[0, 1)`.
pub trait Rng {
    fn unit(&mut self) -> f64;
}

/// The process's randomness: a fresh v4 UUID per draw.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRng;

impl Rng for SystemRng {
    fn unit(&mut self) -> f64 {
        let bits = uuid::Uuid::new_v4().as_u128() >> 75;
        // 53 random bits over 2^53.
        #[allow(clippy::cast_precision_loss)]
        let n = bits as u64 as f64;
        n / 9_007_199_254_740_992.0
    }
}

/// The backoff settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    pub initial: Duration,
    pub max: Duration,
    pub jitter: f64,
    pub startup: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(30),
            max: Duration::from_secs(120),
            jitter: 0.5,
            startup: Duration::from_secs(30),
        }
    }
}

fn secs(v: f64) -> Duration {
    Duration::from_secs_f64(v)
}

impl Config {
    /// Check the invariants (module docs).
    ///
    /// # Errors
    /// Names the violated rule.
    pub fn validate(&self) -> Result<(), String> {
        let (init, max, start) =
            (self.initial.as_secs_f64(), self.max.as_secs_f64(), self.startup.as_secs_f64());
        if init <= 0.0 {
            return Err("initialBackoffSecs must be greater than zero".into());
        }
        if max < init {
            return Err("maxBackoffSecs must not be below initialBackoffSecs".into());
        }
        if !self.jitter.is_finite() || !(0.0..=1.0).contains(&self.jitter) {
            return Err("jitter must be a finite number from 0 to 1".into());
        }
        if start > max {
            return Err("startupSecs must not exceed maxBackoffSecs".into());
        }
        Ok(())
    }

    /// Read the block from an effective config; absent keys default.
    ///
    /// # Errors
    /// A key that is not a finite non-negative number, or a block that fails
    /// [`Self::validate`].
    pub fn from_effective(effective: &Value) -> Result<Self, String> {
        use crate::config_resolver::get_path;
        let read = |key: &str, default: f64| -> Result<f64, String> {
            match get_path(effective, key) {
                None | Some(Value::Null) => Ok(default),
                Some(v) => v
                    .as_f64()
                    .filter(|n| n.is_finite() && *n >= 0.0)
                    .ok_or_else(|| format!("{key} must be a finite, non-negative number")),
            }
        };
        let d = Self::default();
        let cfg = Self {
            initial: secs(read(INITIAL_KEY, d.initial.as_secs_f64())?),
            max: secs(read(MAX_KEY, d.max.as_secs_f64())?),
            jitter: read(JITTER_KEY, d.jitter)?,
            startup: secs(read(STARTUP_KEY, d.startup.as_secs_f64())?),
        };
        cfg.validate()?;
        Ok(cfg)
    }

    /// The nominal delay after `streak` consecutive empty answers.
    #[must_use]
    pub fn nominal(&self, streak: u32) -> Duration {
        let factor = 2u32.checked_pow(streak.min(30)).unwrap_or(u32::MAX);
        self.initial.saturating_mul(factor).min(self.max)
    }

    /// The smallest actual delay at `streak`.
    #[must_use]
    pub fn min_delay(&self, streak: u32) -> Duration {
        self.nominal(streak).mul_f64(1.0 - self.jitter)
    }

    /// The largest actual delay at `streak` (exclusive): the nominal delay.
    #[must_use]
    pub fn max_delay(&self, streak: u32) -> Duration {
        self.nominal(streak)
    }

    /// The closest a capacity wake may follow the previous request.
    #[must_use]
    pub fn wake_floor(&self) -> Duration {
        self.min_delay(0)
    }

    fn draw(&self, streak: u32, rng: &mut dyn Rng) -> Duration {
        self.nominal(streak).mul_f64(1.0 - self.jitter * rng.unit())
    }
}

/// What the loop should do now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Poll {
    /// Send a request for this many free slots. The requester now counts one
    /// in flight until [`Requester::on_answer`] or [`Requester::on_error`].
    Request { free_slots: u32 },
    /// Nothing to do for this long (or until a notification).
    Wait(Duration),
    /// No capacity, or a request is in flight: wait for a notification.
    Idle,
}

/// The request schedule of one worker.
#[derive(Debug, Clone)]
pub struct Requester {
    cfg: Config,
    /// Consecutive empty answers.
    streak: u32,
    due: Mono,
    in_flight: bool,
    free_slots: u32,
    wake: bool,
    last_sent: Option<Mono>,
}

impl Requester {
    /// A requester whose first request waits a random `[0, startup)`.
    #[must_use]
    pub fn new(cfg: Config, now: Mono, rng: &mut dyn Rng) -> Self {
        let wait = cfg.startup.mul_f64(rng.unit());
        Self {
            cfg,
            streak: 0,
            due: now.plus(wait),
            in_flight: false,
            free_slots: 0,
            wake: false,
            last_sent: None,
        }
    }

    /// Consecutive empty answers.
    #[must_use]
    pub fn streak(&self) -> u32 {
        self.streak
    }

    /// Whether a request is in flight.
    #[must_use]
    pub fn in_flight(&self) -> bool {
        self.in_flight
    }

    /// Apply a reloaded configuration (module docs).
    pub fn set_config(&mut self, cfg: Config, now: Mono) {
        self.cfg = cfg;
        self.due = self.due.min(now.plus(cfg.max));
    }

    /// The host's free slots changed (or were re-checked). A rise is a wake;
    /// the same or less changes nothing but the count asked for.
    pub fn on_capacity(&mut self, free_slots: u32) {
        if free_slots > self.free_slots {
            self.wake = true;
        }
        self.free_slots = free_slots;
        if free_slots == 0 {
            self.wake = false;
        }
    }

    /// The next action at `now`, claiming the in-flight slot when it is a
    /// request.
    pub fn poll(&mut self, now: Mono) -> Poll {
        let poll = self.peek(now);
        if matches!(poll, Poll::Request { .. }) {
            self.in_flight = true;
            self.wake = false;
            self.last_sent = Some(now);
        }
        poll
    }

    /// [`Self::poll`] without claiming anything.
    #[must_use]
    pub fn peek(&self, now: Mono) -> Poll {
        if self.in_flight || self.free_slots == 0 {
            return Poll::Idle;
        }
        let mut due = self.due;
        // A wake never beats the randomized startup delay: it needs a
        // previous request to space itself from.
        if let (true, Some(sent)) = (self.wake, self.last_sent) {
            due = due.min(sent.plus(self.cfg.wake_floor()));
        }
        if now < due {
            return Poll::Wait(due.0.saturating_sub(now.0));
        }
        Poll::Request {
            free_slots: self.free_slots,
        }
    }

    /// The captain answered. An answer with work resets the streak and is due
    /// again at once; an empty one grows it and schedules the next request.
    pub fn on_answer(&mut self, now: Mono, got_work: bool, rng: &mut dyn Rng) {
        self.in_flight = false;
        if got_work {
            self.streak = 0;
            self.due = now;
        } else {
            self.due = now.plus(self.cfg.draw(self.streak, rng));
            self.streak = self.streak.saturating_add(1);
        }
    }

    /// The request failed (no answer, a non-2xx, a malformed body): not an
    /// empty answer. The streak is kept; the retry waits the current delay.
    pub fn on_error(&mut self, now: Mono, rng: &mut dyn Rng) {
        self.in_flight = false;
        self.due = now.plus(self.cfg.draw(self.streak.saturating_sub(1), rng));
    }
}

#[cfg(test)]
#[path = "backoff_tests.rs"]
mod tests;
