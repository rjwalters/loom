//! Library-test fixture: one process-env guard shared by every suite that
//! needs to isolate itself from the ambient environment.
//!
//! The guard snapshots each named variable, clears or overrides it, and puts
//! the snapshot back on drop, so a `#[serial]` test cannot leak its settings
//! into the next one and a fleet host's exported overrides cannot decide the
//! suite. Storage is `OsString`, so non-UTF-8 values round-trip.
//!
//! The env is process-global: keep `#[serial]` (or the owning env lock) on
//! every test that holds one.

use std::ffi::{OsStr, OsString};

/// Restores the variables it touched, in reverse order, when dropped.
pub(crate) struct EnvGuard(Vec<(&'static str, Option<OsString>)>);

impl EnvGuard {
    /// Snapshot `keys` without changing them.
    pub(crate) fn save(keys: &[&'static str]) -> Self {
        Self(keys.iter().map(|k| (*k, std::env::var_os(k))).collect())
    }

    /// Snapshot `keys`, then remove each from the environment.
    pub(crate) fn clear(keys: &[&'static str]) -> Self {
        let guard = Self::save(keys);
        for key in keys {
            std::env::remove_var(key);
        }
        guard
    }

    /// Snapshot `key`, then set it to `value`.
    pub(crate) fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let guard = Self::save(&[key]);
        std::env::set_var(key, value);
        guard
    }

    /// Snapshot `key`, then set (`Some`) or remove (`None`) it. Chainable, so
    /// one guard can own several overrides.
    pub(crate) fn and_set(mut self, key: &'static str, value: Option<&OsStr>) -> Self {
        self.0.push((key, std::env::var_os(key)));
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
        self
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..).rev() {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    const A: &str = "LOOM_ENV_TEST_SUPPORT_A";
    const B: &str = "LOOM_ENV_TEST_SUPPORT_B";

    #[test]
    #[serial]
    fn clear_removes_and_drop_restores() {
        std::env::set_var(A, "ambient");
        std::env::remove_var(B);
        {
            let _g = EnvGuard::clear(&[A, B]);
            assert!(std::env::var_os(A).is_none());
            std::env::set_var(B, "leaked");
        }
        assert_eq!(std::env::var(A).as_deref(), Ok("ambient"));
        assert!(std::env::var_os(B).is_none());
        std::env::remove_var(A);
    }

    #[test]
    #[serial]
    fn set_overrides_and_repeated_keys_restore_the_original() {
        std::env::set_var(A, "ambient");
        {
            let _g = EnvGuard::set(A, "one")
                .and_set(A, Some(OsStr::new("two")))
                .and_set(B, None);
            assert_eq!(std::env::var(A).as_deref(), Ok("two"));
        }
        assert_eq!(std::env::var(A).as_deref(), Ok("ambient"));
        std::env::remove_var(A);
    }
}
