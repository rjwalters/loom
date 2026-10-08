//! Conversion between the live [`RollStallTracker`] and the persisted
//! [`StallState`] value (Issue #10713).
//!
//! A child module so it can read the tracker's private fields without widening
//! them, and so `roll_stall.rs` itself only gains the `mod` line. When the
//! detector is replaced, this file goes with it; the persisted format in
//! `super::super::stall_state` does not have to change.

use super::RollStallTracker;
use crate::auto_update::stall_state::{StallEpisode, StallState};

impl RollStallTracker {
    /// The detector's state as a typed value. The resolved knobs (threshold,
    /// cooldown) are configuration, not state, and the one-shot retry note is a
    /// log line already owed to this process, so neither is part of it.
    pub(in crate::auto_update) fn stall_state(&self) -> StallState {
        let Some(since) = self.since else {
            return StallState::None {
                retries: self.retries,
            };
        };
        let episode = StallEpisode {
            since,
            carried_deadlines: self.carried_deadlines,
            live_refusals: self.live_refusals,
            floor: self.floor,
            deadlines_at_floor: self.deadlines_at_floor,
            target: self.target.clone(),
            retries: self.retries,
        };
        match (self.unsatisfiable, self.declared_at) {
            (true, Some(declared_at)) => StallState::Unsatisfiable {
                episode,
                declared_at,
            },
            _ => StallState::DrainDeadlines { episode },
        }
    }

    /// Replace the detector's state with `state`, keeping the resolved knobs.
    pub(in crate::auto_update) fn restore_stall_state(&mut self, state: StallState) {
        let (episode, declared_at) = match state {
            StallState::None { retries } => {
                self.clear_episode(retries);
                return;
            }
            StallState::DrainDeadlines { episode } => (episode, None),
            StallState::Unsatisfiable {
                episode,
                declared_at,
            } => (episode, Some(declared_at)),
        };
        *self = Self {
            threshold: self.threshold,
            cooldown_secs: self.cooldown_secs,
            carried_deadlines: episode.carried_deadlines,
            live_refusals: episode.live_refusals,
            floor: episode.floor,
            deadlines_at_floor: episode.deadlines_at_floor,
            since: Some(episode.since),
            target: episode.target,
            unsatisfiable: declared_at.is_some(),
            declared_at,
            retries: episode.retries,
            retry_note: None,
        };
    }
}
