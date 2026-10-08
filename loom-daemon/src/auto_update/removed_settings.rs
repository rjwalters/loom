//! Settings the self-update loop no longer reads (Issue #10885).
//!
//! The roll window (#9132) is gone: a roll is a short pause, the fleet floor
//! decides when a fleet host moves, and the loop acts on the tick that sees
//! it. Its three config keys and three env vars are still set on live hosts
//! (the fleet store rendered `rollWindowSecs` and a per-host
//! `rollWindowOffsetSecs`), so they must keep loading. They are accepted and
//! ignored, and [`warning`] says so in one line at loop start.

/// One removed setting: its `autonomous.autoUpdate` key and its env override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Removed {
    /// The key under `autonomous.autoUpdate`.
    pub key: &'static str,
    /// The env var that overrode it.
    pub env: &'static str,
}

/// Every removed setting.
pub const REMOVED: [Removed; 3] = [
    Removed {
        key: "rollWindowSecs",
        env: "LOOM_AUTO_UPDATE_ROLL_WINDOW_SECS",
    },
    Removed {
        key: "rollWindowOffsetSecs",
        env: "LOOM_AUTO_UPDATE_ROLL_WINDOW_OFFSET_SECS",
    },
    Removed {
        key: "launchdLiveReload",
        env: "LOOM_AUTO_UPDATE_LAUNCHD_LIVE_RELOAD",
    },
];

/// The removed keys an `autonomous.autoUpdate` block still carries, whatever
/// their values.
#[must_use]
pub fn keys_in(block: &serde_json::Value) -> Vec<&'static str> {
    REMOVED
        .iter()
        .filter(|r| block.get(r.key).is_some())
        .map(|r| r.key)
        .collect()
}

/// The removed env vars that are still set, per `env` (a lookup, so tests
/// need not touch the process environment).
#[must_use]
pub fn envs_in(env: &dyn Fn(&str) -> Option<String>) -> Vec<&'static str> {
    REMOVED
        .iter()
        .filter(|r| env(r.env).is_some())
        .map(|r| r.env)
        .collect()
}

/// The one-line warning naming every removed setting still in use, or `None`
/// when there are none. `keys` is [`super::AutoUpdateConfig::removed_keys`].
#[must_use]
pub fn warning(keys: &[&'static str], env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let mut named: Vec<String> = keys
        .iter()
        .map(|key| format!("autonomous.autoUpdate.{key}"))
        .collect();
    named.extend(envs_in(env).into_iter().map(str::to_string));
    if named.is_empty() {
        return None;
    }
    Some(format!(
        "ignored: {} — roll windows were removed (#10885); the fleet floor (loom_min_version) \
         decides when a fleet host rolls, and it acts on the next tick. Remove these settings.",
        named.join(", ")
    ))
}

/// Log [`warning`] for `config` and the process environment, if there is one.
pub fn warn_if_set(config: &super::AutoUpdateConfig) {
    if let Some(line) = warning(&config.removed_keys, &|k| std::env::var(k).ok()) {
        log::warn!("auto_update: {line}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_block_carrying_the_removed_keys_lists_all_three() {
        let block = serde_json::json!({
            "enabled": true,
            "settleSecs": 3600,
            "rollWindowSecs": 21600,
            "rollWindowOffsetSecs": 900,
            "launchdLiveReload": false,
        });
        assert_eq!(
            keys_in(&block),
            [
                "rollWindowSecs",
                "rollWindowOffsetSecs",
                "launchdLiveReload"
            ]
        );
        assert!(keys_in(&serde_json::json!({ "enabled": true })).is_empty());
    }

    #[test]
    fn the_warning_names_every_key_and_env_var_still_set_on_one_line() {
        let set = |name: &str| name.contains("ROLL_WINDOW").then(|| "1".to_string());
        let line = warning(&["rollWindowSecs", "launchdLiveReload"], &set).unwrap();
        for named in [
            "autonomous.autoUpdate.rollWindowSecs",
            "autonomous.autoUpdate.launchdLiveReload",
            "LOOM_AUTO_UPDATE_ROLL_WINDOW_SECS",
            "LOOM_AUTO_UPDATE_ROLL_WINDOW_OFFSET_SECS",
        ] {
            assert!(line.contains(named), "{named}: {line}");
        }
        assert!(!line.contains("LOOM_AUTO_UPDATE_LAUNCHD_LIVE_RELOAD"), "{line}");
        assert!(line.starts_with("ignored: "), "{line}");
        assert!(!line.contains('\n'));
    }

    /// A host config that still carries the keys (the fleet store rendered
    /// them) loads, reads every live knob as before, and reports the keys.
    #[test]
    fn a_config_that_still_sets_the_keys_loads_and_reports_them() {
        let tmp = tempfile::tempdir().unwrap();
        let loom = tmp.path().join(".loom");
        std::fs::create_dir_all(&loom).unwrap();
        std::fs::write(
            loom.join("config.json"),
            r#"{"autonomous": {"autoUpdate": {"enabled": true, "settleSecs": 3600,
                "rollWindowSecs": 21600, "rollWindowOffsetSecs": 4500}}}"#,
        )
        .unwrap();
        let config = crate::auto_update::read_auto_update_config(tmp.path());
        assert_eq!(config.enabled, Some(true));
        assert_eq!(config.settle_secs, Some(3600));
        assert_eq!(config.removed_keys, ["rollWindowSecs", "rollWindowOffsetSecs"]);
    }

    #[test]
    fn all_three_env_vars_are_detected_and_nothing_set_means_no_warning() {
        let all = |_: &str| Some(String::new());
        assert_eq!(envs_in(&all).len(), 3);
        assert_eq!(warning(&[], &|_| None), None);
    }
}
