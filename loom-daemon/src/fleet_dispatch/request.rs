//! The worker's capacity-driven request loop (#12817): [`super::backoff`]
//! decides when, [`super::worker::request_next`] asks, and this joins them.
//!
//! One [`Loop`] per host. The caller reports capacity ([`Loop::note_capacity`],
//! from the step tick or a slot-freed notification: repeated reports of the
//! same capacity change nothing) and calls [`Loop::step`] whenever it
//! wakes, then sleeps for the returned [`Next`]. [`Loop::step`] keeps one
//! request in flight by construction (it blocks for the call), applies a
//! reloaded `fleetDispatch.request` block before every decision, and treats
//! an empty answer as backoff and a failure as neither (module docs of
//! [`super::backoff`]). While this host is in fallback, or fleet mode is off,
//! nothing is sent and the schedule does not advance.
//!
//! The candidates it returns still go through the worker's gates, live
//! pre-check and forge pre-claim. The loop sends nothing about them.

use std::path::Path;
use std::time::{Duration, Instant};

use super::backoff::{Config, Poll, Requester, Rng, SystemRng};
use super::table::Mono;
use super::worker::{self, Candidate, NextOutcome};

/// What a [`Loop::step`] did and when to call it again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Candidates the captain offered (empty when none, or nothing was asked).
    pub candidates: Vec<Candidate>,
    pub next: Next,
}

impl Step {
    fn idle(next: Next) -> Self {
        Self {
            candidates: Vec::new(),
            next,
        }
    }
}

/// When to step again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// After this long, or sooner on a capacity notification.
    After(Duration),
    /// On the next capacity notification (none free, or fleet mode off).
    OnNotification,
}

/// How a request goes out: [`worker::request_next`] in production.
pub trait Transport {
    fn request(&mut self, root: &Path, free_slots: u32, roles_ok: &[String]) -> NextOutcome;
}

/// The real thing.
#[derive(Debug, Clone, Copy, Default)]
pub struct Wire;

impl Transport for Wire {
    fn request(&mut self, root: &Path, free_slots: u32, roles_ok: &[String]) -> NextOutcome {
        worker::request_next(root, free_slots, roles_ok)
    }
}

/// The loop's state.
pub struct Loop<T: Transport = Wire, R: Rng = SystemRng> {
    requester: Requester,
    transport: T,
    rng: R,
    origin: Instant,
    roles_ok: Vec<String>,
    cfg: Config,
}

impl Loop {
    /// A loop on the process's clock and randomness: its first request waits
    /// a random startup delay.
    #[must_use]
    pub fn start(root: &Path) -> Self {
        Self::with(Wire, SystemRng, Instant::now(), root)
    }
}

impl<T: Transport, R: Rng> Loop<T, R> {
    /// A loop over `transport` and `rng`, `origin` being its time zero.
    pub fn with(transport: T, mut rng: R, origin: Instant, root: &Path) -> Self {
        let cfg = read_config(root, Config::default());
        let requester = Requester::new(cfg, Mono(Duration::ZERO), &mut rng);
        Self {
            requester,
            transport,
            rng,
            origin,
            roles_ok: Vec::new(),
            cfg,
        }
    }

    /// The host's free slots and runnable roles, as the step tick sees them.
    pub fn note_capacity(&mut self, free_slots: u32, roles_ok: &[String]) {
        self.roles_ok = roles_ok.to_vec();
        self.requester.on_capacity(free_slots);
    }

    /// The requester, for inspection.
    #[must_use]
    pub fn requester(&self) -> &Requester {
        &self.requester
    }

    /// Decide and, when due, ask, at the loop's clock (`origin` + elapsed).
    pub fn step(&mut self, root: &Path) -> Step {
        let now = Mono(self.origin.elapsed());
        self.step_at(root, now)
    }

    /// [`Self::step`] at an explicit `now`.
    pub fn step_at(&mut self, root: &Path, now: Mono) -> Step {
        self.reload(root, now);
        if worker::host_endpoint(root).is_none() {
            return Step::idle(Next::OnNotification);
        }
        let free_slots = match self.requester.poll(now) {
            Poll::Request { free_slots } => free_slots,
            Poll::Wait(w) => return Step::idle(Next::After(w)),
            Poll::Idle => return Step::idle(Next::OnNotification),
        };
        let outcome = self.transport.request(root, free_slots, &self.roles_ok);
        let candidates = match outcome {
            NextOutcome::Answered { candidates, .. } => {
                self.requester
                    .on_answer(now, !candidates.is_empty(), &mut self.rng);
                candidates
            }
            NextOutcome::Failed => {
                self.requester.on_error(now, &mut self.rng);
                Vec::new()
            }
            // Nothing was sent: put the in-flight mark back and look again
            // after one initial delay (fallback ends on heartbeats, not here).
            NextOutcome::Off | NextOutcome::Fallback => {
                self.requester.on_error(now, &mut self.rng);
                Vec::new()
            }
        };
        let next = match self.requester.peek(now) {
            Poll::Wait(w) => Next::After(w),
            Poll::Idle => Next::OnNotification,
            // Due at once (work received and capacity remains): the caller
            // steps again immediately.
            Poll::Request { .. } => Next::After(Duration::ZERO),
        };
        Step { candidates, next }
    }

    /// Apply `fleetDispatch.request` from the effective config when it
    /// changed; an invalid block keeps the last good one.
    fn reload(&mut self, root: &Path, now: Mono) {
        let cfg = read_config(root, self.cfg);
        if cfg != self.cfg {
            log::info!("fleet_dispatch: work-request backoff reloaded: {cfg:?}");
            self.cfg = cfg;
            self.requester.set_config(cfg, now);
        }
    }
}

/// The effective config's block, or `last` when it is invalid.
fn read_config(root: &Path, last: Config) -> Config {
    let effective = crate::config_resolver::resolve_effective_config(root);
    Config::from_effective(&effective).unwrap_or_else(|e| {
        log::warn!("fleet_dispatch: ignoring an invalid fleetDispatch.request block: {e}");
        last
    })
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;
