//! Config keys retired with the old dispatch paths (#11911), and the one
//! warning each gets.
//!
//! Step dispatch (design #11894) replaced the sweep tick and the item-role
//! intervals (#11909); #11911 deleted what was left of them. A host whose
//! config still sets one of their keys must keep starting, so a retired key is
//! **parsed and ignored, never rejected**, and warned about once per process.
//!
//! | Key | Replaced by |
//! |---|---|
//! | `autonomous.roleRunner.championMode` (any value but `"non-item"`) | Champion's role-runner run is non-item only ([`crate::role_runner::champion_mode`]) |
//! | `autonomous.roleRunner.demandWidth` (the whole block, #9392, #10630, #10632) | the step draw: Judge and Doctor are steps, so there is no role width, reservation or per-repo lane to size |
//! | `autonomous.roleRunner.maxConcurrent`, `LOOM_ROLE_RUNNER_MAX_CONCURRENT` (#6102) | one host budget, `autonomous.workFinder.maxConcurrent` ([`crate::host_budget`]) |
//!
//! The strict `hyperparameters` block (`crate::hyperparams`) is deliberately
//! **not** touched: it rejects unknown keys at startup, so no key leaves it in
//! the release that retires the behaviour behind it.

use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock};

/// The retired `autonomous.roleRunner` keys whose mere presence is warned
/// about, with what replaced each. `championMode` is not here: its accepted
/// value stays quiet ([`crate::role_runner::champion_mode::warn_champion_mode`]).
pub const RETIRED_ROLE_RUNNER_KEYS: [(&str, &str); 2] = [
    (
        "demandWidth",
        "Judge and Doctor run as steps the work finder draws, so there is no \
         demand width, Champion-first reservation or per-repository lane to size",
    ),
    (
        "maxConcurrent",
        "role-runner runs and steps share one host budget, \
         autonomous.workFinder.maxConcurrent",
    ),
];

/// The retired role-runner ceiling's environment override (#6102).
pub const RETIRED_ROLE_RUNNER_MAX_CONCURRENT_ENV: &str = "LOOM_ROLE_RUNNER_MAX_CONCURRENT";

fn warned() -> &'static Mutex<BTreeSet<String>> {
    static WARNED: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    WARNED.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// Log `message` at WARN the first time `key` is seen in this process.
/// Returns whether it logged. Config is re-read every tick, so without this a
/// retired key would warn on every tick of every root.
pub fn warn_once(key: &str, message: &str) -> bool {
    let first = warned()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key.to_string());
    if first {
        log::warn!("{message}");
    }
    first
}

/// The retired keys `block` (an `autonomous.roleRunner` object) sets. Pure.
#[must_use]
pub fn retired_role_runner_keys(block: &serde_json::Value) -> Vec<&'static str> {
    RETIRED_ROLE_RUNNER_KEYS
        .iter()
        .filter(|(key, _)| block.get(key).is_some())
        .map(|(key, _)| *key)
        .collect()
}

/// Warn, once per key, about every retired key `block` sets and about the
/// retired env override. Never fails and never changes what is parsed.
pub fn warn_role_runner_block(block: &serde_json::Value) {
    crate::role_runner::champion_mode::warn_champion_mode(block);
    for key in retired_role_runner_keys(block) {
        let why = RETIRED_ROLE_RUNNER_KEYS
            .iter()
            .find(|(k, _)| *k == key)
            .map_or("", |(_, why)| why);
        warn_once(
            &format!("autonomous.roleRunner.{key}"),
            &format!("config: autonomous.roleRunner.{key} is retired and ignored: {why} (#11911)"),
        );
    }
    if std::env::var_os(RETIRED_ROLE_RUNNER_MAX_CONCURRENT_ENV).is_some() {
        warn_once(
            RETIRED_ROLE_RUNNER_MAX_CONCURRENT_ENV,
            &format!(
                "config: {RETIRED_ROLE_RUNNER_MAX_CONCURRENT_ENV} is retired and ignored: \
                 role-runner runs and steps share one host budget, \
                 autonomous.workFinder.maxConcurrent (#11911)"
            ),
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn retired_keys_are_found_whatever_their_value() {
        let block = serde_json::json!({
            "enabled": true,
            "demandWidth": {"enabled": false, "laneK": 4},
            "maxConcurrent": "not even a number",
            "roles": ["champion"],
        });
        assert_eq!(retired_role_runner_keys(&block), vec!["demandWidth", "maxConcurrent"]);
        assert!(retired_role_runner_keys(&serde_json::json!({"enabled": true})).is_empty());
        assert!(retired_role_runner_keys(&serde_json::Value::Null).is_empty());
    }

    #[test]
    fn warn_once_logs_each_key_once() {
        let key = "retired_config::tests::warn_once_logs_each_key_once";
        assert!(warn_once(key, "first"));
        assert!(!warn_once(key, "second"));
        assert!(warn_once(&format!("{key}/other"), "a different key"));
    }

    /// Parsing a block full of retired keys never panics and never errors:
    /// a host whose config still sets them keeps starting.
    #[test]
    fn warning_never_fails() {
        warn_role_runner_block(&serde_json::json!({
            "championMode": "all",
            "demandWidth": {"enabled": true},
            "maxConcurrent": 9,
        }));
        warn_role_runner_block(&serde_json::json!({"championMode": 7}));
        warn_role_runner_block(&serde_json::Value::Null);
    }
}
