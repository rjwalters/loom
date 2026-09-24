//! The test-only writer for [`SweepRegistry`]'s filesystem-activity window
//! override (Issue #8487), split out of `sweep_registry/mod.rs` so the setter
//! sits next to [`make_quiet_dirty_git_worktree`](super::make_quiet_dirty_git_worktree),
//! its only real caller, rather than in an already-over-threshold parent
//! (`.loom/docs/file-size-policy.md`). The field itself necessarily stays on
//! the struct in `mod.rs`; everything that *writes* it lives here.
//!
//! Only compiled under `#[cfg(test)]` — `test_support`'s own `mod`
//! declaration in `sweep_registry/mod.rs` carries the gate, so nothing in
//! this file needs (or may repeat) a `#[cfg(test)]` of its own.

use super::super::SweepRegistry;
use std::time::Duration;

impl SweepRegistry {
    /// Override the filesystem-activity window this registry's
    /// [`worktree_in_use`](SweepRegistry::worktree_in_use) judges mtimes
    /// against, or `None` to resolve it from the environment per call
    /// (Issue #8487).
    ///
    /// Test-only: production leaves it `None` and reads
    /// [`ACTIVITY_WINDOW_ENV`](crate::worktree_activity::ACTIVITY_WINDOW_ENV)
    /// exactly as before. See the field's own doc comment for why it exists.
    pub(crate) fn set_activity_window(&mut self, window: Option<Duration>) {
        self.activity_window = window;
    }
}
