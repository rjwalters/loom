//! A single-key condition that must hold for a while before it is an alert
//! (#11295): safehoused unreachable, the rate-limit breaker open, llm-monitor's
//! `ranking.json` stale.
//!
//! [`Tracker`] debounces healthy/unhealthy samples: it opens once the
//! condition has held for `sustain`, and the first healthy sample after that
//! resolves it. A blip shorter than `sustain` never posts.
//!
//! The tracker lives in memory, so a daemon restarted while the incident was
//! open would never resolve it. The first healthy sample after start therefore
//! always sends one `resolved`; for an incident loom-ui does not hold that is
//! a no-op (`outcome: unknown`).

use std::time::{Duration, Instant};

use super::{Alert, State};

/// Debounce over samples. Pure, injected clock.
#[derive(Debug)]
pub struct Tracker {
    bad_since: Option<Instant>,
    /// This process opened the incident.
    open: bool,
    /// A previous process may have left it open: resolve on the first healthy
    /// sample.
    startup_resolve_owed: bool,
}

impl Default for Tracker {
    fn default() -> Self {
        Self {
            bad_since: None,
            open: false,
            startup_resolve_owed: true,
        }
    }
}

impl Tracker {
    /// Feed one sample; returns the transition to post, if any.
    pub fn observe(&mut self, healthy: bool, now: Instant, sustain: Duration) -> Option<State> {
        if healthy {
            self.bad_since = None;
            let owed =
                std::mem::take(&mut self.open) | std::mem::take(&mut self.startup_resolve_owed);
            return owed.then_some(State::Resolved);
        }
        let since = *self.bad_since.get_or_insert(now);
        if !self.open && now.duration_since(since) >= sustain {
            self.open = true;
            self.startup_resolve_owed = false;
            return Some(State::Open);
        }
        None
    }

    /// Whether this process has the incident open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open
    }
}

/// Sample `probe` every `every` for the daemon's lifetime, posting `alert` for
/// each transition. `probe` returns `Ok(())` when healthy (or the condition
/// does not apply) and `Err(why)` when not; `alert` gets the `why` of the
/// sample that caused the transition, if any.
pub fn spawn_watch<S, P, A>(
    every: Duration,
    sustain: Duration,
    probe: P,
    alert: A,
) -> tokio::task::JoinHandle<()>
where
    S: Send + 'static,
    P: Fn() -> Result<(), S> + Send + 'static,
    A: Fn(State, Option<&S>) -> Alert + Send + 'static,
{
    tokio::spawn(async move {
        let mut tracker = Tracker::default();
        loop {
            tokio::time::sleep(every).await;
            let sample = probe();
            if let Some(state) = tracker.observe(sample.is_ok(), Instant::now(), sustain) {
                super::post(alert(state, sample.as_ref().err()));
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUSTAIN: Duration = Duration::from_secs(300);
    const SAMPLE: Duration = Duration::from_secs(30);

    /// A tracker past its startup resolve.
    fn settled(t0: Instant) -> Tracker {
        let mut t = Tracker::default();
        assert_eq!(t.observe(true, t0, SUSTAIN), Some(State::Resolved));
        t
    }

    #[test]
    fn the_first_healthy_sample_after_start_resolves_once() {
        // A restart forgets an open incident; the startup resolve closes it.
        let mut t = Tracker::default();
        let t0 = Instant::now();
        assert_eq!(t.observe(true, t0, SUSTAIN), Some(State::Resolved));
        assert_eq!(t.observe(true, t0 + SAMPLE, SUSTAIN), None);
    }

    #[test]
    fn unhealthy_at_start_opens_after_sustain_and_resolves_once() {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        assert_eq!(t.observe(false, t0, SUSTAIN), None);
        assert_eq!(t.observe(false, t0 + SUSTAIN, SUSTAIN), Some(State::Open));
        assert!(t.is_open());
        assert_eq!(t.observe(false, t0 + SUSTAIN * 2, SUSTAIN), None);
        assert_eq!(t.observe(true, t0 + SUSTAIN * 3, SUSTAIN), Some(State::Resolved));
        assert_eq!(t.observe(true, t0 + SUSTAIN * 4, SUSTAIN), None);
    }

    #[test]
    fn a_short_blip_posts_nothing() {
        let t0 = Instant::now();
        let mut t = settled(t0);
        assert_eq!(t.observe(false, t0, SUSTAIN), None);
        assert_eq!(t.observe(false, t0 + Duration::from_secs(60), SUSTAIN), None);
        assert_eq!(t.observe(true, t0 + Duration::from_secs(90), SUSTAIN), None);
    }

    #[test]
    fn sustained_opens_once_and_recovery_resolves() {
        let t0 = Instant::now();
        let mut t = settled(t0);
        assert_eq!(t.observe(false, t0, SUSTAIN), None);
        assert_eq!(t.observe(false, t0 + SUSTAIN, SUSTAIN), Some(State::Open));
        assert_eq!(t.observe(false, t0 + SUSTAIN * 2, SUSTAIN), None);
        assert_eq!(t.observe(true, t0 + SUSTAIN * 3, SUSTAIN), Some(State::Resolved));
        assert_eq!(t.observe(true, t0 + SUSTAIN * 4, SUSTAIN), None);
    }

    #[test]
    fn zero_sustain_opens_on_the_first_bad_sample() {
        let t0 = Instant::now();
        let mut t = settled(t0);
        assert_eq!(t.observe(false, t0, Duration::ZERO), Some(State::Open));
    }
}
