//! The typed roll-stall state (Issue #10713), as a plain value.
//!
//! #8998's detector ([`super::roll_stall`]) keeps its state as loose fields: a
//! `bool` for "declared unsatisfiable", an `Option` for when, and a handful of
//! counters whose meaning depends on whether an episode is running at all. That
//! shape was never visible outside the module (#9018), and it could not survive a
//! restart. [`StallState`] names the three states the detector can be in, so the
//! stall can be persisted (`auto_update_state.json`) and read back without
//! reaching into the detector's internals.
//!
//! It is deliberately a value with no behaviour and no dependency on
//! `roll_stall`: the conversion to and from the live tracker lives in
//! `roll_stall/persist.rs`, so replacing the detector (the pause-and-roll plan
//! retires it) only has to rewrite that conversion, not the persisted format.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Where the unsatisfiable-roll detector stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StallState {
    /// No episode is running. `retries` is the count of #9010 cooldown-released
    /// retries spent since a tick last saw the host idle; it outlives a cleared
    /// episode, which is why it is carried even here.
    None {
        /// Cooldown-released retries since the host was last seen idle.
        #[serde(default)]
        retries: u32,
    },
    /// An episode is counting drain deadlines that expire without the in-flight
    /// count improving, across roll lifetimes. Not yet declared.
    DrainDeadlines {
        /// The running episode.
        episode: StallEpisode,
    },
    /// The roll's wait condition was declared unsatisfiable and the roll was
    /// abandoned. Sticky until an idle observation or the cooldown, which runs
    /// from `declared_at`.
    Unsatisfiable {
        /// The episode that produced the declaration.
        episode: StallEpisode,
        /// When the declaration was made (the cooldown's origin).
        declared_at: DateTime<Utc>,
    },
}

impl Default for StallState {
    fn default() -> Self {
        Self::None { retries: 0 }
    }
}

/// One episode's accounting, as the detector keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StallEpisode {
    /// When the episode's first observation was made.
    pub since: DateTime<Utc>,
    /// Deadlines banked from rolls that have already ended.
    #[serde(default)]
    pub carried_deadlines: u32,
    /// The live roll's `refusals` at the last observation, if a roll was armed.
    #[serde(default)]
    pub live_refusals: Option<u32>,
    /// The lowest in-flight count seen in the episode.
    #[serde(default)]
    pub floor: Option<usize>,
    /// The deadline total when `floor` last improved.
    #[serde(default)]
    pub deadlines_at_floor: u32,
    /// The most recent roll target seen.
    #[serde(default)]
    pub target: Option<String>,
    /// Cooldown-released retries since the host was last seen idle.
    #[serde(default)]
    pub retries: u32,
}
