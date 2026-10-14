//! Which attempt a role tick is on (#11726): `1 +` the number of consecutive
//! launched ticks of that role in that workspace that failed or were blocked.
//!
//! The 2026-10-12 incident is the case this exists for: a Codex-run tick whose
//! every tool call a stale hook denied exited 0, read as a success, and the
//! next tick drew Codex again. Since #11727 such a tick is a
//! `RoleTickOutcome::Failure` (`hook-denied`), and every failure site in
//! `role_runner::launch` reports through `HoldFeed::failure` — which also
//! calls [`record_failure`] here. So a blocked attempt 1 is followed by a draw
//! from the attempt-2 table, whatever the failure class (deterministic or
//! transient: the operator ruling counts both).
//!
//! A success resets the count. A pre-spawn skip (nothing launched) does not
//! count either way. The table lives in daemon memory, keyed
//! `(workspace root, canonical role)`, so a daemon restart starts every role
//! at attempt 1 again — the same lifetime as the launch-failure hold.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

type Key = (PathBuf, String);

fn global() -> &'static Mutex<HashMap<Key, u32>> {
    static FAILURES: OnceLock<Mutex<HashMap<Key, u32>>> = OnceLock::new();
    FAILURES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_global<T>(f: impl FnOnce(&mut HashMap<Key, u32>) -> T) -> T {
    let mut guard = global()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut guard)
}

fn key(root: &Path, role: &str) -> Key {
    let role = crate::runtime_admission::canonical_role(role).unwrap_or(role);
    (root.to_path_buf(), role.to_string())
}

/// The attempt the next tick of `role` in `root` is on (`>= 1`).
#[must_use]
pub fn current(root: &Path, role: &str) -> u32 {
    let failures = with_global(|t| t.get(&key(root, role)).copied().unwrap_or(0));
    failures.saturating_add(1)
}

/// A launched tick of `role` failed or was blocked: the next one is the next
/// attempt. Returns the new attempt number.
pub fn record_failure(root: &Path, role: &str) -> u32 {
    let failures = with_global(|t| {
        let n = t.entry(key(root, role)).or_insert(0);
        *n = n.saturating_add(1);
        *n
    });
    failures.saturating_add(1)
}

/// A launched tick of `role` succeeded: back to attempt 1.
pub fn record_success(root: &Path, role: &str) {
    with_global(|t| t.remove(&key(root, role)));
}

/// Forget every key (test hygiene: the table is process-global).
#[cfg(test)]
pub fn reset_for_tests() {
    with_global(HashMap::clear);
}
