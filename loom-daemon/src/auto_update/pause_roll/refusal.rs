//! Rate limit on a refused roll's alert (issue #10831, Judge finding on
//! #10974).
//!
//! The auto-update tick calls [`super::start_pause_roll`] every time it sees a
//! release to roll to. While the previous roll's pause manifest is still live
//! (`resume-pending`), every one of those calls is refused, for up to the
//! manifest's max age (15 minutes). The refusal is correct; an ERROR line and
//! a `daemon.roll.refused` event on every tick is noise that buries the first
//! one. So a refusal is alerted **once per key** (the manifest id), then at
//! most once per [`REPEAT_AFTER`] while the same key keeps being refused.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long a standing refusal stays quiet between alerts.
pub(super) const REPEAT_AFTER: Duration = Duration::from_secs(300);

/// The last refusal alerted: its key and when.
#[derive(Debug, Default)]
pub(super) struct RefusalLimiter {
    last: Option<(String, Instant)>,
}

impl RefusalLimiter {
    /// Whether a refusal for `key` at `now` should be alerted. A new key
    /// always is; the same key again only after [`REPEAT_AFTER`].
    pub(super) fn should_alert(&mut self, key: &str, now: Instant) -> bool {
        let quiet = self.last.as_ref().is_some_and(|(last, at)| {
            last == key && now.saturating_duration_since(*at) < REPEAT_AFTER
        });
        if !quiet {
            self.last = Some((key.to_string(), now));
        }
        !quiet
    }
}

/// [`RefusalLimiter::should_alert`] on the process-wide limiter.
pub(super) fn should_alert(key: &str) -> bool {
    static LIMITER: OnceLock<Mutex<RefusalLimiter>> = OnceLock::new();
    LIMITER
        .get_or_init(|| Mutex::new(RefusalLimiter::default()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .should_alert(key, Instant::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_standing_refusal_alerts_once_then_every_five_minutes() {
        let mut limiter = RefusalLimiter::default();
        let t0 = Instant::now();
        assert!(limiter.should_alert("rp-1", t0), "the first refusal is alerted");
        // Every 30 s tick for the next five minutes is quiet.
        for tick in 1..10 {
            assert!(!limiter.should_alert("rp-1", t0 + Duration::from_secs(30 * tick)));
        }
        assert!(limiter.should_alert("rp-1", t0 + REPEAT_AFTER), "then once per 5 min");
        assert!(!limiter.should_alert("rp-1", t0 + REPEAT_AFTER + Duration::from_secs(30)));
        // The next roll's manifest is a new refusal.
        assert!(limiter.should_alert("rp-2", t0 + REPEAT_AFTER + Duration::from_secs(60)));
    }
}
