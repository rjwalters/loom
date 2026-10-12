//! The dispatch gate a daemon roll closes, and the count of dispatches that
//! are mid-spawn (issue #10831, Judge finding 3 on #10974; design
//! `docs/design/daemon-roll-pause-resume.md` §7 H4 step 1).
//!
//! # The window this closes
//!
//! A dispatch is split so the registry mutex is released while the spawned
//! child is polled for its account line (#6592): `begin_prepared_issue_dispatch`
//! claims the lock, flips the label and spawns the child; up to
//! `TOKEN_NAME_CAPTURE_TIMEOUT` later `finish_issue_dispatch` records the
//! `Running` entry. In between, the agent exists and is in no registry entry.
//! A roll that snapshots the registry in that window does not see it: the
//! agent is in neither the pause manifest nor the reaper hold, and it is
//! still running when the daemon exits.
//!
//! `SweepState::Pending` does not describe that window. Nothing constructs
//! it (`types.rs` reserves it for a future async spawn), so a wait on it
//! waits for nothing.
//!
//! # What H4 does instead
//!
//! - It **closes the gate** on every registry when the pause is requested
//!   ([`SweepRegistry::close_for_roll`]). `begin_prepared_issue_dispatch` is
//!   the one function every dispatch goes through (IPC, the work finder, the
//!   reaper's crash-resume, PR sets), and it refuses while the gate is closed,
//!   before any claim, label or spawn. The gate is checked under the registry
//!   mutex, so a dispatch that passed the drain flag earlier and is still
//!   preparing is refused too.
//! - It then **waits for [`SweepRegistry::mid_spawn_dispatches`] to reach
//!   zero**. Every `Spawned` hand-back carries a [`MidSpawn`] guard inside
//!   its `PreparedIssueDispatch`; the count falls when that value is dropped,
//!   which is at the end of `finish_issue_dispatch` (still under the mutex,
//!   after the entry is recorded) or wherever an abandoned dispatch is
//!   dropped. With the gate closed the count can only fall, and the wait is
//!   bounded by H4's existing settle window.
//!
//! A pause that stands down reopens the gate. The gate is closed *by* a pause
//! run (its manifest id) and stays closed while any run holds it, so a
//! superseded run that stands down late does not reopen the gate its
//! replacement has closed.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::SweepRegistry;
use crate::types::SweepKind;

/// One registry's roll gate.
#[derive(Debug, Default)]
pub(crate) struct RollGate {
    closed_by: BTreeSet<String>,
    mid_spawn: Arc<AtomicUsize>,
}

/// Held by a dispatch from its spawn until its registry entry is recorded.
#[derive(Debug)]
pub(crate) struct MidSpawn(Arc<AtomicUsize>);

impl Drop for MidSpawn {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl RollGate {
    /// Refuse a new dispatch of `kind` while a roll's pause holds the gate.
    ///
    /// # Errors
    /// With the operator-facing reason when the gate is closed.
    pub(crate) fn admit(&self, kind: &SweepKind) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.closed_by.is_empty(),
            "dispatch of {kind:?} refused: the daemon is pausing every agent for a version roll \
             (#10831) and starts nothing new until it has restarted or the pause stands down"
        );
        Ok(())
    }

    /// Count a dispatch as mid-spawn until the returned guard is dropped.
    pub(crate) fn enter(&self) -> MidSpawn {
        self.mid_spawn.fetch_add(1, Ordering::SeqCst);
        MidSpawn(Arc::clone(&self.mid_spawn))
    }
}

impl SweepRegistry {
    /// Close this registry's dispatch gate for pause run `owner`, or take
    /// `owner`'s hold off it. The gate is open only when no run holds it.
    pub(crate) fn close_for_roll(&mut self, owner: &str, closed: bool) {
        if closed {
            self.roll_gate.closed_by.insert(owner.to_string());
        } else {
            self.roll_gate.closed_by.remove(owner);
        }
    }

    /// Whether a roll's pause holds this registry's dispatch gate.
    #[must_use]
    pub(crate) fn closed_for_roll(&self) -> bool {
        !self.roll_gate.closed_by.is_empty()
    }

    /// Dispatches whose child is spawned and whose registry entry is not yet
    /// recorded.
    #[must_use]
    pub(crate) fn mid_spawn_dispatches(&self) -> usize {
        self.roll_gate.mid_spawn.load(Ordering::SeqCst)
    }
}
