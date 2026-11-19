//! Since when each workspace has been held, and why (Issue #11564).
//!
//! The work finder folds its per-root holds into one cause per root every
//! tick ([`super::causes_per_root`]) and then
//! forgets them. An ETA reader needs the hold as a fact with a start: a repo
//! held `install_incompatible` for 70 hours looks, tick by tick, exactly like
//! one held for a minute. [`HaltClock`] keeps that start.
//!
//! - `since` is the first tick at which this process saw the root held **with
//!   this cause**. A cause change restarts it; a tick that sees the root not
//!   held clears it.
//! - `since_lower_bound` is set when the hold was already in force on the
//!   first tick this process observed: it may predate the process, so
//!   `since` is only a lower bound on how long it has lasted. It mirrors
//!   `fleet.state`'s `held_since_lower_bound`.
//!
//! [`publish`] is the one call the multi-workspace loop makes; it stores the
//! tick's halts for `fleet.state` ([`current`]). Nothing here reads the forge
//! or the disk.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{DateTime, Utc};

use super::HaltCause;

/// One held root, as the clock last saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootHalt {
    /// The workspace root.
    pub root: PathBuf,
    /// The closed-vocabulary cause; `None` for a hold no cause names.
    pub cause: Option<HaltCause>,
    /// The first tick that saw the root held with `cause`.
    pub since: DateTime<Utc>,
    /// The hold was in force on the clock's first tick, so `since` is a lower
    /// bound.
    pub since_lower_bound: bool,
}

/// The per-root hold starts, across ticks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HaltClock {
    /// Whether any tick has been observed.
    ticked: bool,
    /// Held roots, by root.
    held: BTreeMap<PathBuf, RootHalt>,
}

impl HaltClock {
    /// A clock that has seen no tick.
    #[must_use]
    pub const fn new() -> Self {
        HaltClock {
            ticked: false,
            held: BTreeMap::new(),
        }
    }

    /// Observe one tick at `now`: `roots` is every root the tick considered,
    /// each with whether it is held and the cause, when one is named. A root
    /// held with the cause it had last tick keeps its `since`; one newly held,
    /// or held with a different cause, starts at `now`; one not held (or not
    /// in `roots`) is dropped. Pure.
    pub fn observe<'a>(
        &mut self,
        roots: impl IntoIterator<Item = (&'a PathBuf, bool, Option<HaltCause>)>,
        now: DateTime<Utc>,
    ) {
        let first = !self.ticked;
        let mut held = BTreeMap::new();
        for (root, is_held, cause) in roots {
            if !is_held {
                continue;
            }
            let halt = match self.held.remove(root) {
                Some(was) if was.cause == cause => was,
                // A changed cause restarts the clock: the new hold was seen
                // to begin now, so it is exact.
                Some(_) => RootHalt {
                    root: root.clone(),
                    cause,
                    since: now,
                    since_lower_bound: false,
                },
                None => RootHalt {
                    root: root.clone(),
                    cause,
                    since: now,
                    since_lower_bound: first,
                },
            };
            held.insert(root.clone(), halt);
        }
        self.held = held;
        self.ticked = true;
    }

    /// The held roots, by root; `None` before the first tick.
    #[must_use]
    pub fn halts(&self) -> Option<Vec<RootHalt>> {
        self.ticked.then(|| self.held.values().cloned().collect())
    }
}

static CLOCK: Mutex<HaltClock> = Mutex::new(HaltClock::new());

/// Record `causes` on the process clock at the current instant, and return it. The
/// multi-workspace loop's single call: the routing slice it derives
/// (`cause.is_some()`) is the one recorded here.
#[must_use]
pub fn publish(causes: Vec<Option<HaltCause>>, roots: &[PathBuf]) -> Vec<Option<HaltCause>> {
    let tick = roots
        .iter()
        .zip(&causes)
        .map(|(root, cause)| (root, cause.is_some(), *cause));
    CLOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .observe(tick, Utc::now());
    causes
}

/// The held roots as of the work finder's last tick; `None` before its first
/// tick in this process.
#[must_use]
pub fn current() -> Option<Vec<RootHalt>> {
    CLOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .halts()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "since_tests.rs"]
mod tests;
