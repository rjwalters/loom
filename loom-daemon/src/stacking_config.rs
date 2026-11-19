//! `stacking.onConflict`: the conflict-stacking switch and depth cap
//! (#12758, sub-issue A of epic #12757).
//!
//! The value is read through [`crate::config_resolver::resolve_effective_config`]
//! and [`crate::config_resolver::get_path`], so every tier applies:
//! fleet-gitops sets the fleet default in the machine defaults tier
//! (`fleet/defaults.json`, written as each host's defaults file) and a
//! per-repo override in the per-repo fleet tier
//! ([`crate::config_resolver::FLEET_REPO_CONFIG_REL`], #11738), which beats
//! the repo's committed `.loom/config.json`. No env override and no new tier.
//!
//! This module only resolves the value. Using it (opening a conflicting PR on
//! the open PR's branch) is #12759.

use std::path::Path;

use serde_json::Value;

use crate::config_resolver::{get_path, resolve_effective_config};

/// Dotted config key of the block this module reads.
pub const STACKING_ON_CONFLICT_KEY: &str = "stacking.onConflict";

/// Code default for `stacking.onConflict.enabled`. Off, so a host outside the
/// fleet sees no change. The fleet default (on) is set in fleet-gitops.
pub const DEFAULT_STACKING_ENABLED: bool = false;

/// Code default for `stacking.onConflict.maxDepth`. A starting guess from the
/// 2026-10-18 discussion, not a measured value.
pub const DEFAULT_STACKING_MAX_DEPTH: u32 = 3;

/// Effective `stacking.onConflict` for one repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackingOnConflict {
    /// Whether a conflicting PR is stacked on the open PR it conflicts with.
    pub enabled: bool,
    /// Deepest stack allowed. `0` is valid and means never stack.
    pub max_depth: u32,
}

impl Default for StackingOnConflict {
    fn default() -> Self {
        Self {
            enabled: DEFAULT_STACKING_ENABLED,
            max_depth: DEFAULT_STACKING_MAX_DEPTH,
        }
    }
}

/// Resolve `stacking.onConflict` for the repo at `main_root`.
///
/// Pass the **main clone** root, never a Builder worktree: the per-repo fleet
/// tier (`.loom-local/fleet-repo.json`) is git-ignored and exists only in the
/// main clone.
#[must_use]
pub fn stacking_on_conflict(main_root: &Path) -> StackingOnConflict {
    from_effective_config(&resolve_effective_config(main_root))
}

/// [`stacking_on_conflict`] over an already-resolved effective config tree.
///
/// Each field falls back to its default on its own, so a bad `maxDepth` does
/// not discard a valid `enabled`. A field that is present but invalid (a
/// non-bool `enabled`; a negative, non-integer, string, or over-`u32`
/// `maxDepth`) is logged at `WARN` naming the key. An absent or `null` field,
/// or an absent block, is the default with no warning.
#[must_use]
pub fn from_effective_config(effective: &Value) -> StackingOnConflict {
    let mut out = StackingOnConflict::default();
    let block = match get_path(effective, STACKING_ON_CONFLICT_KEY) {
        None | Some(Value::Null) => return out,
        Some(Value::Object(block)) => block,
        Some(other) => {
            log::warn!(
                "config: {STACKING_ON_CONFLICT_KEY} must be an object, got {other}; \
                 using defaults (enabled={DEFAULT_STACKING_ENABLED}, \
                 maxDepth={DEFAULT_STACKING_MAX_DEPTH})"
            );
            return out;
        }
    };

    match block.get("enabled") {
        None | Some(Value::Null) => {}
        Some(Value::Bool(b)) => out.enabled = *b,
        Some(other) => log::warn!(
            "config: {STACKING_ON_CONFLICT_KEY}.enabled must be a bool, got {other}; \
             using {DEFAULT_STACKING_ENABLED}"
        ),
    }

    match block.get("maxDepth") {
        None | Some(Value::Null) => {}
        Some(v) => match v.as_u64().and_then(|n| u32::try_from(n).ok()) {
            Some(n) => out.max_depth = n,
            None => log::warn!(
                "config: {STACKING_ON_CONFLICT_KEY}.maxDepth must be a non-negative integer, \
                 got {v}; using {DEFAULT_STACKING_MAX_DEPTH}"
            ),
        },
    }

    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config_resolver::{FLEET_REPO_CONFIG_REL, LEGACY_CONFIG_REL, PRIVATE_DEFAULTS_ENV};
    use serde_json::json;
    use serial_test::serial;
    use tempfile::tempdir;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    fn on(enabled: bool, max_depth: u32) -> StackingOnConflict {
        StackingOnConflict { enabled, max_depth }
    }

    /// Resolve for `root` with the machine defaults tier pointed at
    /// `defaults` (or disabled when `None`).
    fn resolve_with_defaults(root: &Path, defaults: Option<&Path>) -> StackingOnConflict {
        match defaults {
            Some(p) => std::env::set_var(PRIVATE_DEFAULTS_ENV, p),
            None => std::env::set_var(PRIVATE_DEFAULTS_ENV, ""),
        }
        let got = stacking_on_conflict(root);
        std::env::remove_var(PRIVATE_DEFAULTS_ENV);
        got
    }

    const FLEET_DEFAULT_ON: &str =
        r#"{"stacking": {"onConflict": {"enabled": true, "maxDepth": 3}}}"#;

    #[test]
    #[serial(loom_config_env)]
    fn absent_config_is_off_with_depth_three() {
        let repo = tempdir().unwrap();
        assert_eq!(resolve_with_defaults(repo.path(), None), on(false, 3));
    }

    #[test]
    #[serial(loom_config_env)]
    fn fleet_default_on_reaches_a_repo_that_sets_nothing() {
        let repo = tempdir().unwrap();
        let host = tempdir().unwrap();
        let defaults = host.path().join("defaults.json");
        write(&defaults, FLEET_DEFAULT_ON);
        assert_eq!(resolve_with_defaults(repo.path(), Some(&defaults)), on(true, 3));
    }

    #[test]
    #[serial(loom_config_env)]
    fn per_repo_fleet_tier_turns_one_repo_off() {
        let host = tempdir().unwrap();
        let defaults = host.path().join("defaults.json");
        write(&defaults, r#"{"stacking": {"onConflict": {"enabled": true, "maxDepth": 5}}}"#);

        let overridden = tempdir().unwrap();
        write(
            &overridden.path().join(FLEET_REPO_CONFIG_REL),
            r#"{"stacking": {"onConflict": {"enabled": false}}}"#,
        );
        let other = tempdir().unwrap();

        assert_eq!(
            resolve_with_defaults(overridden.path(), Some(&defaults)),
            on(false, 5),
            "the override sets only enabled; maxDepth still comes from the fleet default"
        );
        assert_eq!(resolve_with_defaults(other.path(), Some(&defaults)), on(true, 5));
    }

    #[test]
    #[serial(loom_config_env)]
    fn per_repo_fleet_tier_beats_committed_config() {
        let repo = tempdir().unwrap();
        write(
            &repo.path().join(LEGACY_CONFIG_REL),
            r#"{"stacking": {"onConflict": {"enabled": true, "maxDepth": 4}}}"#,
        );
        write(
            &repo.path().join(FLEET_REPO_CONFIG_REL),
            r#"{"stacking": {"onConflict": {"enabled": false, "maxDepth": 1}}}"#,
        );
        assert_eq!(resolve_with_defaults(repo.path(), None), on(false, 1));
    }

    #[test]
    fn max_depth_zero_is_valid() {
        let cfg = json!({"stacking": {"onConflict": {"enabled": true, "maxDepth": 0}}});
        assert_eq!(from_effective_config(&cfg), on(true, 0));
    }

    #[test]
    fn invalid_max_depth_falls_back_and_keeps_enabled() {
        for bad in [
            json!(-1),
            json!("3"),
            json!(2.5),
            json!(true),
            json!(u64::from(u32::MAX) + 1),
        ] {
            let cfg = json!({"stacking": {"onConflict": {"enabled": true, "maxDepth": bad}}});
            assert_eq!(from_effective_config(&cfg), on(true, 3), "maxDepth = {bad}");
        }
    }

    #[test]
    fn invalid_enabled_falls_back_and_keeps_max_depth() {
        for bad in [json!("yes"), json!(1), json!(null)] {
            let cfg = json!({"stacking": {"onConflict": {"enabled": bad, "maxDepth": 7}}});
            assert_eq!(from_effective_config(&cfg), on(false, 7), "enabled = {bad}");
        }
    }

    #[test]
    fn non_object_block_falls_back_to_defaults() {
        for bad in [json!(true), json!("on"), json!([1, 2])] {
            let cfg = json!({"stacking": {"onConflict": bad}});
            assert_eq!(from_effective_config(&cfg), on(false, 3), "onConflict = {bad}");
        }
        let cfg = json!({"stacking": "on"});
        assert_eq!(from_effective_config(&cfg), on(false, 3));
    }
}
