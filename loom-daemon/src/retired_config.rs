//! Leftover configuration for subsystems Loom no longer ships (Issue #11112).
//!
//! The Matrix fleet-comms integration (`safehouse`: narration, ChatOps, the
//! peer-claim channel) was removed. A host that still carries a `safehouse`
//! block in `.loom/config.json` (or any other config tier), or still exports
//! `LOOM_SAFEHOUSE_*` / `SAFEHOUSED_SOCKET`, must start cleanly: the keys are
//! ignored, and the daemon says so **once** per process so an operator knows
//! the block can be deleted. Nothing else in the daemon reads these keys.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// The retired top-level config key.
pub const RETIRED_CONFIG_KEY: &str = "safehouse";

/// Env-var prefix of the retired integration's overrides.
const RETIRED_ENV_PREFIX: &str = "LOOM_SAFEHOUSE_";

/// The retired integration's socket override.
const RETIRED_SOCKET_ENV: &str = "SAFEHOUSED_SOCKET";

static WARNED: AtomicBool = AtomicBool::new(false);

/// Every leftover setting present for `repo_root`: `safehouse` when the
/// effective config carries that key, plus each matching env var (sorted).
/// Empty on a clean host.
#[must_use]
pub fn leftover_keys(repo_root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    if effective.get(RETIRED_CONFIG_KEY).is_some() {
        found.push(RETIRED_CONFIG_KEY.to_string());
    }
    let mut env: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| k.starts_with(RETIRED_ENV_PREFIX) || k == RETIRED_SOCKET_ENV)
        .collect();
    env.sort();
    found.extend(env);
    found
}

/// Log one warning naming the leftover settings, the first time this is
/// called in the process with any present. Returns `true` when it warned.
/// Never fails: the keys are ignored either way.
pub fn warn_once(repo_root: &Path) -> bool {
    let keys = leftover_keys(repo_root);
    if keys.is_empty() || WARNED.swap(true, Ordering::SeqCst) {
        return false;
    }
    log::warn!(
        "retired_config: ignoring leftover settings for the removed Matrix fleet-comms \
         integration: {}. Loom coordinates through forge labels and lease records only \
         (#11112); delete these settings to silence this warning.",
        keys.join(", ")
    );
    true
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::tempdir;

    fn clear_env() {
        let keys: Vec<String> = std::env::vars_os()
            .filter_map(|(k, _)| k.into_string().ok())
            .filter(|k| k.starts_with(RETIRED_ENV_PREFIX) || k == RETIRED_SOCKET_ENV)
            .collect();
        for k in keys {
            std::env::remove_var(k);
        }
    }

    /// A config with a leftover block and a leftover env var are both
    /// reported (and so ignored, not rejected). The host's own private
    /// defaults tier is out of the test's control, so the assertion is on
    /// what the test planted rather than on exact equality.
    #[test]
    #[serial]
    fn a_leftover_block_and_env_are_reported_once() {
        clear_env();
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            r#"{"safehouse": {"enabled": true, "socket": "/tmp/x.sock"}}"#,
        )
        .unwrap();
        std::env::set_var("LOOM_SAFEHOUSE_ENABLED", "1");
        let keys = leftover_keys(dir.path());
        assert_eq!(keys.first().map(String::as_str), Some(RETIRED_CONFIG_KEY));
        assert!(keys.iter().any(|k| k == "LOOM_SAFEHOUSE_ENABLED"), "{keys:?}");

        // One warning per process: the first call warns, every later one
        // (another workspace, another tick) stays silent.
        WARNED.store(false, Ordering::SeqCst);
        assert!(warn_once(dir.path()));
        assert!(!warn_once(dir.path()));
        clear_env();
    }
}
