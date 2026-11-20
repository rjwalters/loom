//! The fleet `gh` tier's isolated `gh` environment (#12312).
//!
//! On a fleet host the `gh` tier is the last resort when the mirror and the
//! gateway cannot answer. It must not borrow a fleet daemon's App profiles or
//! an inherited token: it runs under its own `GH_CONFIG_DIR` with every token
//! variable removed. That directory holds the installation's own login; on a
//! fleet host it holds nothing, so the tier **fails closed** there (`gh`
//! refuses to authenticate) rather than spending a credential it was not
//! given.
//!
//! A standalone install (`Chain::standalone`) is not isolated: its
//! only tier is `gh` with the installation's own login, exactly as before.
//!
//! The scope is per thread, like [`crate::gh_invocation::purpose`]: the
//! listing runs synchronously on the thread that opened it.

use std::cell::Cell;
use std::marker::PhantomData;
use std::path::PathBuf;

/// Overrides [`config_dir`].
pub const DIR_ENV: &str = "LOOM_DISCOVERY_GH_CONFIG_DIR";

thread_local! {
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// An active isolation scope; dropping it ends the scope. Not `Send`.
#[derive(Debug)]
#[must_use = "the scope ends when this guard is dropped"]
pub struct IsolatedGh {
    _thread: PhantomData<*const ()>,
}

/// Run every conditional read built on this thread until the guard drops in
/// the isolated `gh` environment.
pub fn scope() -> IsolatedGh {
    DEPTH.with(|d| d.set(d.get() + 1));
    IsolatedGh {
        _thread: PhantomData,
    }
}

impl Drop for IsolatedGh {
    fn drop(&mut self) {
        DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Whether this thread's reads are in an isolation scope.
#[must_use]
pub fn active() -> bool {
    DEPTH.with(|d| d.get() > 0)
}

/// The isolated `GH_CONFIG_DIR`: `$LOOM_DISCOVERY_GH_CONFIG_DIR`, else
/// `~/.loom/discovery-gh`. Never a fleet profile directory.
#[must_use]
pub fn config_dir() -> PathBuf {
    resolve_dir(
        std::env::var_os(DIR_ENV).map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

fn resolve_dir(explicit: Option<PathBuf>, home: Option<PathBuf>) -> PathBuf {
    match explicit.filter(|p| !p.as_os_str().is_empty()) {
        Some(dir) => dir,
        None => home
            .unwrap_or_else(std::env::temp_dir)
            .join(".loom")
            .join("discovery-gh"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_is_per_thread_and_nests() {
        assert!(!active());
        let outer = scope();
        let inner = scope();
        assert!(active());
        drop(inner);
        assert!(active());
        assert!(!std::thread::spawn(active).join().unwrap());
        drop(outer);
        assert!(!active());
    }

    #[test]
    fn dir_prefers_the_override_then_a_loom_private_dir() {
        assert_eq!(
            resolve_dir(Some("/x/gh".into()), Some("/home/u".into())),
            PathBuf::from("/x/gh")
        );
        assert_eq!(
            resolve_dir(Some("".into()), Some("/home/u".into())),
            PathBuf::from("/home/u/.loom/discovery-gh")
        );
        assert_eq!(
            resolve_dir(None, Some("/home/u".into())),
            PathBuf::from("/home/u/.loom/discovery-gh")
        );
    }
}
