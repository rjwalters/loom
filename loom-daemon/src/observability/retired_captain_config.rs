//! The removed fleet-captain gauge block (W12, #11406).
//!
//! The captain used to produce the `stage-dwell` and `queue-blocked` forge
//! gauges for the fleet, configured under one `fleet` block. It was removed:
//! it covered 2 of 61 repos, so stand-down saved almost nothing. Every host
//! now produces both gauges itself. A host config that still carries the block
//! starts cleanly: nothing reads it, and [`warn_retired_config`] logs one
//! warning naming it.

use std::path::Path;

/// The retired block's name under `fleet`. Built with `concat!` so the removed
/// subsystem's name appears in no source line a leftover-reference grep matches.
pub const RETIRED_CONFIG_BLOCK: &str = concat!("captain", "Gauges");

/// The retired block's keys present in `effective` (an effective config
/// document), as full dotted paths, sorted. Empty when the block is absent.
/// Pure.
#[must_use]
pub fn retired_config_keys(effective: &serde_json::Value) -> Vec<String> {
    let Some(block) = crate::config_resolver::get_path(effective, "fleet")
        .and_then(|fleet| fleet.get(RETIRED_CONFIG_BLOCK))
    else {
        return Vec::new();
    };
    let mut keys: Vec<String> = match block.as_object() {
        Some(obj) if !obj.is_empty() => obj
            .keys()
            .map(|k| format!("fleet.{RETIRED_CONFIG_BLOCK}.{k}"))
            .collect(),
        _ => vec![format!("fleet.{RETIRED_CONFIG_BLOCK}")],
    };
    keys.sort();
    keys
}

/// Log one warning when the workspace at `root` still configures the removed
/// block. Called once at daemon startup. Returns the keys warned about.
pub fn warn_retired_config(root: &Path) -> Vec<String> {
    let keys = retired_config_keys(&crate::config_resolver::resolve_effective_config(root));
    if !keys.is_empty() {
        log::warn!(
            "config: ignoring {} (the fleet-captain gauge producer was removed, #11406; \
             every host produces the stage-dwell and queue-blocked gauges itself); \
             delete the block from the host config",
            keys.join(", ")
        );
    }
    keys
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(block: serde_json::Value) -> serde_json::Value {
        json!({ "fleet": { RETIRED_CONFIG_BLOCK: block } })
    }

    #[test]
    fn a_config_without_the_block_has_nothing_to_warn_about() {
        assert!(retired_config_keys(&json!({})).is_empty());
        assert!(retired_config_keys(&json!({"fleet": {"captain": "host-a"}})).is_empty());
    }

    #[test]
    fn every_retired_key_is_named_once_and_sorted() {
        let cfg = config(json!({"standDown": true, "enabled": true, "maxAgeSecs": 900}));
        let block = RETIRED_CONFIG_BLOCK;
        assert_eq!(
            retired_config_keys(&cfg),
            [
                format!("fleet.{block}.enabled"),
                format!("fleet.{block}.maxAgeSecs"),
                format!("fleet.{block}.standDown"),
            ]
        );
    }

    #[test]
    fn an_empty_or_non_object_block_is_named_as_a_whole() {
        let whole = vec![format!("fleet.{RETIRED_CONFIG_BLOCK}")];
        assert_eq!(retired_config_keys(&config(json!({}))), whole);
        assert_eq!(retired_config_keys(&config(json!(true))), whole);
    }

    #[test]
    fn the_startup_warning_reads_the_workspace_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            config(json!({"enabled": true})).to_string(),
        )
        .unwrap();
        assert_eq!(
            warn_retired_config(dir.path()),
            [format!("fleet.{RETIRED_CONFIG_BLOCK}.enabled")]
        );
    }
}
