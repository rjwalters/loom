//! The process-wide memo behind [`super`] — and its test seams.
//!
//! Production keeps one `Mutex`-guarded [`State`]; no lock is ever held across
//! a `git` or `gh` spawn (callers copy out, compute, then write back). Test
//! builds keep the state, the on/off switch, the environment the resolver
//! reads and the clock **per thread**, so parallel tests can never observe
//! each other's records — the same isolation the ETag store and the call sink
//! use (`set_test_daemon_store_dir`, `set_test_sink_dir`).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use super::base::{BaseRepo, ConfigFp};
use super::installation::Snapshot;
use super::record::Record;
use super::GhRepoEnv;

/// `(root, env)` — the base-repo memo key.
pub(super) type RootEnv = (PathBuf, GhRepoEnv);

#[derive(Debug, Default)]
pub(super) struct State {
    /// The resolved base repo per `(root, env)`, valid while its fingerprint
    /// still matches (`None` = the checkout has no usable remote).
    pub(super) base_memo: HashMap<RootEnv, (ConfigFp, Option<BaseRepo>)>,
    /// `origin`'s `(host, owner/repo)` per root, for the ETag store's key.
    pub(super) origin_memo: HashMap<PathBuf, (ConfigFp, (String, String))>,
    /// Canonical records by [`super::record::record_key`] (the hot layer over
    /// the disk store).
    pub(super) records: HashMap<String, Record>,
    /// Roots whose local answer disagreed with gh's own: every site keeps its
    /// legacy forge call for the life of the process.
    pub(super) legacy: HashSet<RootEnv>,
    /// Ambiguous roots already cross-checked at this fingerprint.
    pub(super) crosschecked: HashSet<(PathBuf, GhRepoEnv, ConfigFp)>,
    /// `(root, canonical full name)` pairs already warned about.
    pub(super) warned: HashSet<(PathBuf, String)>,
    /// `confirm_owner` answers per `(root, env, pass)`: the canonical owner,
    /// or `None` when the confirm read failed.
    pub(super) confirms: HashMap<(PathBuf, GhRepoEnv, u64), Option<String>>,
    /// `(pass, record key)` confirmed against the forge inside that pass.
    pub(super) confirmed: HashSet<(u64, String)>,
    /// Installation snapshots by credential key (the hot layer over the disk
    /// store, W8).
    pub(super) snapshots: HashMap<String, Snapshot>,
    /// Credential keys whose latest snapshot revalidation failed (one warn
    /// line per streak).
    pub(super) snapshot_failing: HashSet<String>,
}

#[cfg(not(test))]
pub(super) fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    use std::sync::{Mutex, OnceLock};
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    let lock = STATE.get_or_init(|| Mutex::new(State::default()));
    let mut guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut guard)
}

#[cfg(not(test))]
pub(super) fn default_on() -> bool {
    true
}

#[cfg(not(test))]
pub(super) fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

#[cfg(not(test))]
pub(super) fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Extra environment for the resolver's own `git` children (none in
/// production: they see exactly what `gh`'s `git` would).
#[cfg(not(test))]
pub(super) fn git_env() -> Vec<(String, String)> {
    Vec::new()
}

#[cfg(not(test))]
pub(super) fn count_git_fork() {}

#[cfg(test)]
pub(crate) use test_seams::*;

#[cfg(test)]
mod test_seams {
    use super::State;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    thread_local! {
        static STATE: RefCell<State> = RefCell::new(State::default());
        static ENABLED: Cell<bool> = const { Cell::new(false) };
        static ENV: RefCell<Option<HashMap<String, String>>> = const { RefCell::new(None) };
        static GIT_ENV: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
        static CLOCK_OFFSET: Cell<i64> = const { Cell::new(0) };
        static GIT_FORKS: Cell<u64> = const { Cell::new(0) };
    }

    pub(in crate::forge_repo_facts) fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
        STATE.with(|s| f(&mut s.borrow_mut()))
    }

    /// Off unless this test thread opted in (the crate's many fake-`gh` tests
    /// keep their exact pre-facts call sequences).
    pub(in crate::forge_repo_facts) fn default_on() -> bool {
        ENABLED.with(Cell::get)
    }

    /// The opted-in environment map when set, else the process environment.
    pub(in crate::forge_repo_facts) fn env_var(name: &str) -> Option<String> {
        ENV.with(|e| match e.borrow().as_ref() {
            Some(map) => map.get(name).cloned(),
            None => std::env::var(name).ok(),
        })
    }

    pub(in crate::forge_repo_facts) fn now() -> i64 {
        chrono::Utc::now().timestamp() + CLOCK_OFFSET.with(Cell::get)
    }

    pub(in crate::forge_repo_facts) fn git_env() -> Vec<(String, String)> {
        GIT_ENV.with(|g| g.borrow().clone())
    }

    pub(in crate::forge_repo_facts) fn count_git_fork() {
        GIT_FORKS.with(|c| c.set(c.get() + 1));
    }

    /// Turn repo facts on (or off) for THIS test thread, resetting its state.
    pub(crate) fn set_test_enabled(on: bool) {
        ENABLED.with(|e| e.set(on));
        STATE.with(|s| *s.borrow_mut() = State::default());
        CLOCK_OFFSET.with(|c| c.set(0));
        GIT_FORKS.with(|c| c.set(0));
    }

    /// Replace the environment the resolver reads (`LOOM_REPO`, `GH_REPO`,
    /// `LOOM_REPO_FACTS…`) for THIS thread; `None` = the process environment.
    pub(crate) fn set_test_env(vars: Option<&[(&str, &str)]>) {
        let map = vars.map(|v| {
            v.iter()
                .map(|(k, val)| ((*k).to_string(), (*val).to_string()))
                .collect()
        });
        ENV.with(|e| *e.borrow_mut() = map);
    }

    /// Environment for the resolver's `git` children on THIS thread (a test
    /// points `GIT_CONFIG_GLOBAL` at a fixture so the host's own global
    /// config cannot leak into the answer).
    pub(crate) fn set_test_git_env(vars: &[(&str, &str)]) {
        let v = vars
            .iter()
            .map(|(k, val)| ((*k).to_string(), (*val).to_string()))
            .collect();
        GIT_ENV.with(|g| *g.borrow_mut() = v);
    }

    /// Move THIS thread's clock forward.
    pub(crate) fn advance_test_clock(secs: i64) {
        CLOCK_OFFSET.with(|c| c.set(c.get() + secs));
    }

    /// `git` children the resolver spawned on THIS thread.
    pub(crate) fn test_git_forks() -> u64 {
        GIT_FORKS.with(Cell::get)
    }
}
