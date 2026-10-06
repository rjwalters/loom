//! Test-only, crate-wide isolation for `LOOM_CODEX_PROFILE_ROOT` (issue #9964).
//!
//! The variable is process-global, and every `#[test]` in this crate links into
//! one multi-threaded binary. Tests used to redirect it with bare
//! `set_var`/`remove_var` pairs serialized by a patchwork of *different*
//! `serial_test` keys (default, `codex_home_env`, `codex_profile_root`,
//! `loom_shared_tokens_dir_env`) plus some with no key at all. Distinct keys do
//! not exclude each other, so one test could unset the variable between another
//! test's `set_var` and its write — and the write then fell back to the real
//! `~/.loom/codex-profiles` (a fixture `alice` profile leaked onto an operator
//! Mac). A panic before a trailing `remove_var` leaked the redirect instead.
//!
//! Two pieces close that class:
//!
//! * [`lock`] — one crate-wide lock that **every** test mutating the variable
//!   holds for its whole scope, whatever `serial_test` key it also carries.
//!   Re-entrant per thread, so a test holding a multi-variable env guard can
//!   still call a helper that takes its own [`ProfileRootEnv`].
//! * [`ProfileRootEnv`] — an RAII redirect that holds the lock and restores the
//!   prior value on drop, including during a panic unwind.
//!
//! `paths::codex_profile_root()` panics under `cfg(test)` when the variable is
//! unset, so a test that forgets the redirect fails loudly instead of falling
//! back to the operator's home.

use std::cell::{Cell, RefCell};
use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::paths::CODEX_PROFILE_ROOT_ENV;

static LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    /// Number of live [`ProfileRootLock`]s on this thread.
    static DEPTH: Cell<usize> = const { Cell::new(0) };
    /// The real mutex guard, held while `DEPTH > 0`. Kept here rather than in
    /// any one handle so handles may drop in any order on the owning thread.
    static HELD: RefCell<Option<MutexGuard<'static, ()>>> = const { RefCell::new(None) };
}

/// Proof that the current thread holds the crate-wide profile-root lock.
/// `!Send`: it must be dropped on the thread that took it.
#[must_use = "the lock is released as soon as this is dropped"]
pub(crate) struct ProfileRootLock {
    _not_send: std::marker::PhantomData<*const ()>,
}

/// Take the crate-wide `LOOM_CODEX_PROFILE_ROOT` lock (re-entrant per thread).
///
/// Multi-variable test env guards that save/clear/restore this variable among
/// others hold one of these for their lifetime.
pub(crate) fn lock() -> ProfileRootLock {
    if DEPTH.with(Cell::get) == 0 {
        // A test that panicked while holding the lock poisons it; the env it
        // touched was still restored by its guard's `Drop`, so the poison
        // carries no information here.
        let guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        HELD.with(|held| *held.borrow_mut() = Some(guard));
    }
    DEPTH.with(|depth| depth.set(depth.get() + 1));
    ProfileRootLock {
        _not_send: std::marker::PhantomData,
    }
}

impl Drop for ProfileRootLock {
    fn drop(&mut self) {
        let remaining = DEPTH.with(|depth| {
            let next = depth.get().saturating_sub(1);
            depth.set(next);
            next
        });
        if remaining == 0 {
            // Take the guard out before dropping it so the `RefCell` borrow is
            // released first.
            let guard = HELD.with(|held| held.borrow_mut().take());
            drop(guard);
        }
    }
}

/// RAII redirect of `LOOM_CODEX_PROFILE_ROOT`: holds [`lock`] and restores the
/// variable's prior value (or its absence) on drop — including on panic.
#[must_use = "the redirect is undone as soon as this is dropped"]
pub(crate) struct ProfileRootEnv {
    prior: Option<OsString>,
    // Declared last so it drops after `Drop::drop` restored the variable.
    _lock: ProfileRootLock,
}

impl ProfileRootEnv {
    /// Point the variable at `value` (a tempdir, or `""` to disable the root).
    pub(crate) fn set(value: impl AsRef<OsStr>) -> Self {
        let lock = lock();
        let prior = std::env::var_os(CODEX_PROFILE_ROOT_ENV);
        std::env::set_var(CODEX_PROFILE_ROOT_ENV, value);
        Self { prior, _lock: lock }
    }

    /// Remove the variable for the guard's scope. Only for tests that assert
    /// the unset behavior itself — `codex_profile_root()` panics while unset.
    pub(crate) fn unset() -> Self {
        let lock = lock();
        let prior = std::env::var_os(CODEX_PROFILE_ROOT_ENV);
        std::env::remove_var(CODEX_PROFILE_ROOT_ENV);
        Self { prior, _lock: lock }
    }
}

impl Drop for ProfileRootEnv {
    fn drop(&mut self) {
        match self.prior.take() {
            Some(value) => std::env::set_var(CODEX_PROFILE_ROOT_ENV, value),
            None => std::env::remove_var(CODEX_PROFILE_ROOT_ENV),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens_pool::paths::codex_profile_root;

    #[test]
    fn codex_profile_root_panics_instead_of_resolving_the_real_home_under_test() {
        let _env = ProfileRootEnv::unset();
        let result = std::panic::catch_unwind(codex_profile_root);
        assert!(result.is_err(), "an unset root must panic under cfg(test), never fall back");
    }

    #[test]
    fn an_explicit_root_and_an_empty_override_still_resolve() {
        let dir = tempfile::tempdir().unwrap();
        {
            let _env = ProfileRootEnv::set(dir.path());
            assert_eq!(codex_profile_root().as_deref(), Some(dir.path()));
        }
        let _env = ProfileRootEnv::set("");
        assert_eq!(codex_profile_root(), None);
    }

    #[test]
    fn the_guard_restores_the_prior_value_after_a_panic_and_frees_the_lock() {
        let outer = tempfile::tempdir().unwrap();
        let inner = tempfile::tempdir().unwrap();
        let outer_path = outer.path().to_path_buf();
        let inner_path = inner.path().to_path_buf();
        // Run in a dedicated thread so the outer redirect is the "prior" value
        // and the panic cannot be confused with this test's own outcome.
        let joined = std::thread::spawn(move || {
            let _outer = ProfileRootEnv::set(&outer_path);
            let caught = std::panic::catch_unwind(|| {
                let _inner = ProfileRootEnv::set(&inner_path);
                assert_eq!(std::env::var_os(CODEX_PROFILE_ROOT_ENV), Some(inner_path.into()));
                panic!("boom inside a guarded scope");
            });
            assert!(caught.is_err());
            std::env::var_os(CODEX_PROFILE_ROOT_ENV)
        })
        .join()
        .unwrap();
        assert_eq!(joined, Some(outer.path().as_os_str().to_owned()));
        // Every handle on that thread is gone, so the lock can be taken again.
        drop(lock());
    }

    #[test]
    fn the_lock_is_released_when_a_guarded_thread_panics_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let result = std::thread::spawn(move || {
            let _env = ProfileRootEnv::set(&path);
            panic!("test body panics while holding the redirect");
        })
        .join();
        assert!(result.is_err());
        // The poisoned lock is still acquirable, and the variable is not left
        // pointing at the dead test's tempdir.
        let _lock = lock();
        assert_ne!(
            std::env::var_os(CODEX_PROFILE_ROOT_ENV),
            Some(dir.path().as_os_str().to_owned())
        );
    }
}
