//! Test-only isolation for the machine-wide build slot (#11014).
//!
//! The slot lives at `~/.loom/locks/build-slot` unless [`BUILD_SLOT_DIR_ENV`]
//! overrides it. A test that reaches [`super::acquire`] (or the
//! `deep_clean` / `docker_image_clean` seams that call [`super::slot_dir`])
//! without an override takes the HOST's real slot: it contends with a live
//! daemon's gate and, when an earlier killed gate left a `slot-0` behind,
//! stalls at 0% CPU for the full bounded wait. #11014 hit exactly that: some
//! `main_health_gate` tests `remove_var`-ed the override at their end, and the
//! runner tests that never set one fell through to `$HOME`.
//!
//! Two pieces close it:
//!
//! - [`BuildSlotEnvGuard`] — the one helper every slot-touching test uses. It
//!   points the slot at a per-test temp dir (or disables slots), and on drop
//!   RESTORES every variable it touched to its prior value instead of
//!   unsetting it. Hold it for the whole test, and mark the test `#[serial]`:
//!   under plain `cargo test` the environment is process-wide.
//! - [`forbid_host_slot_dir`] — called by [`super::slot_dir`] in test builds
//!   only. It panics when the resolved directory is the host's real
//!   `$HOME/.loom/locks/build-slot`, so a regression fails loudly at the
//!   offending test instead of quietly touching host state.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::{
    BUILD_SLOTS_ENV, BUILD_SLOT_DIR_ENV, BUILD_SLOT_HELD_ENV, BUILD_SLOT_STALE_SECS_ENV,
    BUILD_SLOT_WAIT_SECS_ENV,
};

/// Every build-slot knob a test might change. The guard snapshots all of them,
/// so a test that sets one more knob after creating the guard still gets it
/// restored.
const KNOBS: [&str; 5] = [
    BUILD_SLOT_DIR_ENV,
    BUILD_SLOTS_ENV,
    BUILD_SLOT_HELD_ENV,
    BUILD_SLOT_WAIT_SECS_ENV,
    BUILD_SLOT_STALE_SECS_ENV,
];

/// RAII isolation of the build-slot environment for one test.
pub(crate) struct BuildSlotEnvGuard {
    saved: Vec<(&'static str, Option<OsString>)>,
    dir: PathBuf,
    // Deleted after `Drop::drop` has restored the environment.
    _tmp: tempfile::TempDir,
}

impl BuildSlotEnvGuard {
    /// One real slot in a fresh temp dir. The ambient knobs an agent's own
    /// environment may carry (`LOOM_BUILD_SLOTS=0`, `LOOM_BUILD_SLOT_HELD=1`,
    /// custom wait/stale) are cleared, so the test sees the defaults.
    pub(crate) fn isolated() -> Self {
        let tmp = tempfile::tempdir().expect("create a temp build-slot dir");
        let dir = tmp.path().join("build-slot");
        let saved = KNOBS.iter().map(|k| (*k, std::env::var_os(k))).collect();
        for knob in KNOBS {
            std::env::remove_var(knob);
        }
        std::env::set_var(BUILD_SLOT_DIR_ENV, &dir);
        Self {
            saved,
            dir,
            _tmp: tmp,
        }
    }

    /// Slots disabled (`LOOM_BUILD_SLOTS=0`). The temp dir override stays in
    /// place too, so nothing can reach `$HOME` even if a seam ignores the count.
    pub(crate) fn disabled() -> Self {
        let guard = Self::isolated();
        std::env::set_var(BUILD_SLOTS_ENV, "0");
        guard
    }

    /// The per-test slot directory (`<tmp>/build-slot`; created on first acquire).
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for BuildSlotEnvGuard {
    fn drop(&mut self) {
        for (knob, prior) in &self.saved {
            match prior {
                Some(v) => std::env::set_var(knob, v),
                None => std::env::remove_var(knob),
            }
        }
    }
}

/// Run `f` under [`BuildSlotEnvGuard::isolated`] — for a test whose only
/// slot-touching step is one call.
pub(crate) fn with_isolated_slot<T>(f: impl FnOnce() -> T) -> T {
    let _slots = BuildSlotEnvGuard::isolated();
    f()
}

/// Panic when a test resolves the slot to the host's real
/// `$HOME/.loom/locks/build-slot`. Compiled into test builds only.
pub(crate) fn forbid_host_slot_dir(resolved: Option<&Path>) {
    let Some(resolved) = resolved else {
        return;
    };
    let host = super::resolve_slot_dir(None, dirs::home_dir());
    if host.as_deref() == Some(resolved) {
        panic!(
            "test resolved the build slot to the host's real {} (#11014). Hold a \
             `crate::build_slot::test_support::BuildSlotEnvGuard` for the whole test \
             (`isolated()` or `disabled()`) and mark it #[serial]",
            resolved.display()
        );
    }
}
