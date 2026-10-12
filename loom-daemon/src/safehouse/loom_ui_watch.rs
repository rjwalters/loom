//! `safehouse.unreachable` on loom-ui (#11295).
//!
//! Matrix is the channel the daily alert pick goes out on, and a dead
//! safehoused is silent: nothing narrates its own absence. This watches the
//! narration sink's connection-state cell and, once safehoused has been
//! unreachable (or refusing sends) for longer than [`SUSTAIN`], opens a
//! `safehouse.unreachable` incident on loom-ui; the first connected sample
//! after that resolves it. A blip shorter than [`SUSTAIN`] never posts.
//!
//! Spawned only when narration is enabled with a socket, so "not configured"
//! here means the sink has not connected yet and counts as unreachable.
//!
//! The tracker lives in memory, so a daemon restarted while the incident was
//! open would never resolve it. The first healthy sample after start therefore
//! always sends one `resolved`; for an incident loom-ui does not hold that is
//! a no-op (`outcome: unknown`).

use std::time::{Duration, Instant};

use super::{snapshot_state, SafehouseState, SharedSafehouseState};
use crate::loom_ui_alerts::{self, Alert, Severity, State};

/// How long safehoused must stay unreachable before it is an alert.
pub const SUSTAIN: Duration = Duration::from_secs(5 * 60);
/// How often the state cell is sampled.
pub const SAMPLE: Duration = Duration::from_secs(30);
/// The rule.
pub const RULE: &str = "safehouse.unreachable";

/// Debounce over state samples. Pure, injected clock.
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
}

/// The alert for `state`, describing `sample`.
#[must_use]
pub fn alert(state: State, sample: &SafehouseState) -> Alert {
    let title = match state {
        State::Open => "safehoused is unreachable: Matrix narration and the alert pick are not \
                        going out"
            .to_string(),
        State::Resolved => "safehoused is reachable again".to_string(),
    };
    let detail = match sample {
        SafehouseState::Unreachable { socket } => {
            format!("Cannot connect to safehoused at {}.", socket.display())
        }
        SafehouseState::SendRejected { socket, reason } => {
            format!("safehoused at {} rejects sends: {reason}", socket.display())
        }
        SafehouseState::NotConfigured => "The narration sink has not connected.".to_string(),
        SafehouseState::Connected { socket, .. } => {
            format!("Connected to safehoused at {}.", socket.display())
        }
    };
    Alert::new(RULE, "safehoused", title, Severity::Critical, state).with_detail(detail)
}

/// Sample `cell` every [`SAMPLE`] on `runtime` for the daemon's lifetime.
pub fn spawn(runtime: &tokio::runtime::Handle, cell: SharedSafehouseState) {
    runtime.spawn(async move {
        let mut tracker = Tracker::default();
        loop {
            tokio::time::sleep(SAMPLE).await;
            let sample = snapshot_state(&cell);
            let healthy = matches!(sample, SafehouseState::Connected { .. });
            if let Some(state) = tracker.observe(healthy, Instant::now(), SUSTAIN) {
                loom_ui_alerts::post(alert(state, &sample));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn sustained_unreachable_opens_once_and_reconnect_resolves() {
        let t0 = Instant::now();
        let mut t = settled(t0);
        assert_eq!(t.observe(false, t0, SUSTAIN), None);
        assert_eq!(t.observe(false, t0 + SUSTAIN, SUSTAIN), Some(State::Open));
        assert_eq!(t.observe(false, t0 + SUSTAIN * 2, SUSTAIN), None);
        assert_eq!(t.observe(true, t0 + SUSTAIN * 3, SUSTAIN), Some(State::Resolved));
        assert_eq!(t.observe(true, t0 + SUSTAIN * 4, SUSTAIN), None);
    }

    #[test]
    fn the_alert_names_the_socket() {
        let a = alert(
            State::Open,
            &SafehouseState::Unreachable {
                socket: "/run/safehoused.sock".into(),
            },
        );
        assert_eq!(a.rule, RULE);
        assert_eq!(a.severity, Severity::Critical);
        assert!(a.detail.unwrap().contains("/run/safehoused.sock"));
    }
}
