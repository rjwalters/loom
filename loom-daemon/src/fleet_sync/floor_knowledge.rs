//! What this host knows about the fleet floor, as three values, and the wake
//! that tells the self-update loop it changed (Issue #10885).
//!
//! [`super::loom_min_version`] answers `None` for three different situations:
//! no store is configured, the store carries no floor, and no pass has
//! completed yet. That was harmless while `None` meant "the floor has no
//! effect". Since #10885 a fleet host moves **only** when the floor moves, so
//! the self-update loop has to tell them apart:
//!
//! | [`FloorKnowledge`] | When | The self-update loop |
//! |---|---|---|
//! | [`NoStore`](FloorKnowledge::NoStore) | `fleet.repo` is not configured: not a fleet host | opt-in `autoUpdate`, settle-gated |
//! | [`Set`](FloorKnowledge::Set) | the last completed pass resolved a floor; before this process's first pass completes, the previous process's snapshot counts | rolls when below it, otherwise does nothing |
//! | [`Unknown`](FloorKnowledge::Unknown) | a store is configured but no floor is known | does nothing, and says why |
//!
//! `Unknown` fails closed. It covers a startup pass that has not completed (and
//! left no snapshot), a store whose `fleet.json` / `repos.yml` carries no
//! `loom_min_version`, a malformed floor with no last good value, and a store
//! that is configured but could not be started. A host that may have a floor
//! must not fall back to chasing the latest release.
//!
//! # The wake
//!
//! The floor is refreshed by the fleet-sync timer (`fleet.syncIntervalSecs`)
//! but acted on by the self-update loop (`autoUpdate.intervalSecs`, 900 s by
//! default). [`FloorWake`] closes that gap: a pass that resolves a floor
//! different from the one before it bumps a generation and notifies, and the
//! loop ticks at once. It fires on a **change of value** only, so a failing
//! floor roll is paced by its own backoff and not by the sync cadence.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use super::{cached_status, floor_cell, probe_status, FloorPass};
use crate::fleet_store::floor;

/// What this host knows about the fleet floor (see the module doc).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum FloorKnowledge {
    /// No fleet store is configured: this is not a fleet host and has no floor
    /// by definition.
    #[default]
    NoStore,
    /// A fleet store is configured, but no floor is known. The string says
    /// why, for the status note.
    Unknown(String),
    /// The floor in force, `X.Y.Z`.
    Set(String),
}

/// Whether this host reads a fleet store, as [`super::start`] found it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum StoreMode {
    /// [`super::start`] has not run in this process.
    #[default]
    Undetermined,
    /// `fleet.repo` is not configured.
    Absent,
    /// `fleet.repo` is configured (whether or not syncing could start).
    Configured,
}

#[derive(Debug, Default)]
struct Meta {
    mode: StoreMode,
    /// Why the last completed pass resolved no floor, when it reported one.
    last_error: Option<String>,
}

fn meta() -> &'static Mutex<Meta> {
    static META: OnceLock<Mutex<Meta>> = OnceLock::new();
    META.get_or_init(|| Mutex::new(Meta::default()))
}

/// Record whether this host reads a fleet store. A change wakes the
/// self-update loop.
pub(super) fn set_store_mode(mode: StoreMode) {
    let changed = meta().lock().is_ok_and(|mut m| {
        let changed = m.mode != mode;
        m.mode = mode;
        if mode != StoreMode::Configured {
            m.last_error = None;
        }
        changed
    });
    if changed {
        floor_wake().bump();
    }
}

/// Publish one pass's floor to the process-wide value, and wake the
/// self-update loop when it differs from what the previous pass resolved (or
/// when this is the first pass to resolve anything).
pub(super) fn record_floor(pass: &FloorPass) {
    let resolved = Some(pass.floor.clone());
    let changed = floor_cell().lock().is_ok_and(|mut cell| {
        let changed = *cell != resolved;
        *cell = resolved;
        changed
    });
    if let Ok(mut m) = meta().lock() {
        m.last_error.clone_from(&pass.error);
    }
    if changed {
        floor_wake().bump();
    }
}

/// What this host knows about the fleet floor right now (see the module doc).
/// The self-update loop reads it at the start of every tick.
#[must_use]
pub fn floor_knowledge() -> FloorKnowledge {
    let (mode, last_error) = meta()
        .lock()
        .map(|m| (m.mode, m.last_error.clone()))
        .unwrap_or_default();
    let resolved = floor_cell().lock().ok().and_then(|cell| cell.clone());
    knowledge_from(mode, resolved, last_error, || {
        cached_status()
            .or_else(probe_status)
            .and_then(|s| s.floor.floor)
    })
}

/// [`floor_knowledge`] over plain values. `resolved` is the process-wide floor
/// (`None` until a pass has completed); `snapshot` reads the floor the
/// previous process recorded, and is only called when no pass has completed.
fn knowledge_from(
    mode: StoreMode,
    resolved: Option<Option<String>>,
    last_error: Option<String>,
    snapshot: impl FnOnce() -> Option<String>,
) -> FloorKnowledge {
    match (mode, resolved) {
        (StoreMode::Absent, _) => FloorKnowledge::NoStore,
        (StoreMode::Undetermined, _) => {
            FloorKnowledge::Unknown("fleet-sync has not started in this process".to_string())
        }
        (StoreMode::Configured, Some(Some(floor))) => FloorKnowledge::Set(floor),
        (StoreMode::Configured, Some(None)) => {
            FloorKnowledge::Unknown(last_error.unwrap_or_else(|| {
                format!("the fleet store carries no `{}`, which is required", floor::KEY)
            }))
        }
        (StoreMode::Configured, None) => snapshot().map_or_else(
            || {
                FloorKnowledge::Unknown(
                    "no fleet-sync pass has completed in this process, and no earlier snapshot \
                     records a floor"
                        .to_string(),
                )
            },
            FloorKnowledge::Set,
        ),
    }
}

/// The signal a floor change sends to the self-update loop: a generation the
/// loop compares, and a notification that ends its wait early.
///
/// The generation is what makes a wake safe to miss or to see twice. The loop
/// records it when a tick starts and waits for it to differ, so a change that
/// lands mid-tick is picked up as soon as the tick ends, and a change the tick
/// already saw does not cause a second one.
#[derive(Debug, Default)]
pub struct FloorWake {
    generation: AtomicU64,
    notify: tokio::sync::Notify,
}

impl FloorWake {
    /// A wake nothing has bumped.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a change and wake every waiter.
    pub fn bump(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// The current generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Resolve once the generation differs from `seen`. Returns at once when
    /// it already does.
    pub async fn changed_since(&self, seen: u64) {
        loop {
            // Created before the comparison: a `Notified` receives a
            // `notify_waiters` call made any time after it exists, so a bump
            // between the comparison and the await is not lost.
            let notified = self.notify.notified();
            if self.generation() != seen {
                return;
            }
            notified.await;
        }
    }
}

/// The process-wide wake [`record_floor`] bumps.
#[must_use]
pub fn floor_wake() -> &'static FloorWake {
    static WAKE: OnceLock<FloorWake> = OnceLock::new();
    WAKE.get_or_init(FloorWake::new)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn never() -> Option<String> {
        panic!("the snapshot is read only when no pass has completed")
    }

    #[test]
    fn a_host_with_no_store_has_no_floor_whatever_else_is_recorded() {
        for resolved in [None, Some(None), Some(Some("0.19.888".to_string()))] {
            assert_eq!(
                knowledge_from(StoreMode::Absent, resolved, None, never),
                FloorKnowledge::NoStore
            );
        }
    }

    #[test]
    fn a_completed_pass_decides_and_a_store_without_the_field_is_unknown() {
        assert_eq!(
            knowledge_from(StoreMode::Configured, Some(Some("0.19.888".to_string())), None, never),
            FloorKnowledge::Set("0.19.888".to_string())
        );
        // An old fleet.json during the transition: loaded, but no floor.
        let FloorKnowledge::Unknown(why) =
            knowledge_from(StoreMode::Configured, Some(None), None, never)
        else {
            panic!("a store with no floor is unknown, never `NoStore`");
        };
        assert!(why.contains("loom_min_version"), "{why}");
        // A malformed floor with no last good value says so.
        assert_eq!(
            knowledge_from(
                StoreMode::Configured,
                Some(None),
                Some("fleet.json: malformed".to_string()),
                never
            ),
            FloorKnowledge::Unknown("fleet.json: malformed".to_string())
        );
    }

    #[test]
    fn before_the_first_pass_the_previous_snapshot_counts_and_nothing_is_unknown() {
        assert_eq!(
            knowledge_from(StoreMode::Configured, None, None, || Some("0.19.830".to_string())),
            FloorKnowledge::Set("0.19.830".to_string())
        );
        // The startup pass hit its cap and no earlier process left a snapshot.
        let FloorKnowledge::Unknown(why) =
            knowledge_from(StoreMode::Configured, None, None, || None)
        else {
            panic!("expected unknown");
        };
        assert!(why.contains("no fleet-sync pass has completed"), "{why}");
        // Fail closed when `start` never ran at all.
        assert!(matches!(
            knowledge_from(StoreMode::Undetermined, None, None, never),
            FloorKnowledge::Unknown(_)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn the_wake_resolves_on_a_bump_and_not_before() {
        let wake = FloorWake::new();
        let seen = wake.generation();
        // Paused time auto-advances only while every task is idle, so this
        // timeout elapsing proves the wait did not resolve on its own.
        let waited =
            tokio::time::timeout(Duration::from_secs(3600), wake.changed_since(seen)).await;
        assert!(waited.is_err(), "an unchanged generation never resolves");

        wake.bump();
        // A bump that landed before the wait began is still seen.
        tokio::time::timeout(Duration::from_secs(1), wake.changed_since(seen))
            .await
            .expect("a changed generation resolves at once");
        // And is not seen twice.
        let seen = wake.generation();
        assert!(tokio::time::timeout(Duration::from_secs(3600), wake.changed_since(seen))
            .await
            .is_err());
    }
}
