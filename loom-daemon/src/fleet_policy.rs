//! Fleet-policy key classification and the "tracked tiers must not shadow the
//! fleet" guard (Issue #11441, slice 1).
//!
//! Tier 1 (the machine-level `defaults.json` rendered from fleet-gitops) is
//! where fleet behaviour is decided. Tier 2/3 (`.loom/config.json`,
//! `.loom-project/project.json`) are deep-merged **over** it, so any fleet key
//! a repo commits silently wins for that repo. [`CLASSIFICATION`] is the one
//! machine-readable source of which top-level keys are fleet policy; `loom-daemon
//! validate`, the CI guard (`validate --strict`) and `loom-daemon status` all
//! read it so the three cannot drift. The human table is in
//! `.loom/docs/fleet-config-lifecycle.md`.
//!
//! A deliberate per-repo override is allowed by an entry in
//! [`ALLOW_LIST_REL`] carrying a non-empty reason.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config_resolver::{
    get_path, resolve_effective_config, soft_read_json_object, LEGACY_CONFIG_REL,
    PROJECT_CONFIG_REL,
};

/// Repo-tracked allow-list of deliberate per-repo fleet-key overrides.
pub const ALLOW_LIST_REL: &str = ".loom/fleet-policy-allow.json";

/// Who owns a top-level config key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyClass {
    /// Fleet policy: set in fleet-gitops (tier 1), never in a tracked repo tier.
    Fleet,
    /// Genuinely repo-specific: belongs in the tracked repo tier.
    Repo,
    /// Host-specific: belongs in `.loom-local/local.json`.
    Host,
}

/// Every known top-level key of `defaults/config.json` and of the committed
/// loom config. Keys not listed are unclassified and never flagged.
pub const CLASSIFICATION: &[(&str, KeyClass)] = &[
    ("autonomous", KeyClass::Fleet),
    ("daemon", KeyClass::Fleet),
    ("fleet", KeyClass::Fleet),
    ("forge", KeyClass::Repo),
    ("guide", KeyClass::Fleet),
    ("health_monitoring", KeyClass::Host),
    ("observability", KeyClass::Fleet),
    ("offlineMode", KeyClass::Fleet),
    ("reflection", KeyClass::Repo),
    ("runtimes", KeyClass::Fleet),
    ("safehouse", KeyClass::Fleet),
    ("sweep", KeyClass::Fleet),
    ("branchProtection", KeyClass::Repo),
    ("buildGate", KeyClass::Repo),
    ("guards", KeyClass::Repo),
    ("labels", KeyClass::Repo),
    ("terminals", KeyClass::Repo),
    ("version", KeyClass::Repo),
    ("worktree", KeyClass::Host),
];

/// Classification of a top-level key, if known.
#[must_use]
pub fn class_of(key: &str) -> Option<KeyClass> {
    CLASSIFICATION
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, c)| *c)
}

/// One violation found by [`check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub key: String,
    pub file: PathBuf,
    pub message: String,
}

/// Parse the allow-list: `{"allow": [{"key": "...", "reason": "..."}]}`.
/// Returns `(key, reason)` pairs; a malformed file yields no entries.
#[must_use]
pub fn read_allow_list(repo_root: &Path) -> Vec<(String, String)> {
    let doc = soft_read_json_object(&repo_root.join(ALLOW_LIST_REL));
    doc.get("allow")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| {
                    let key = e.get("key")?.as_str()?.to_string();
                    let reason = e.get("reason").and_then(Value::as_str).unwrap_or("");
                    Some((key, reason.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The tracked repo tiers (tier 2 then 3), whose content must not shadow the fleet.
fn tracked_tiers(repo_root: &Path) -> [PathBuf; 2] {
    [
        repo_root.join(LEGACY_CONFIG_REL),
        repo_root.join(PROJECT_CONFIG_REL),
    ]
}

/// Fleet-class keys present in the tracked tiers that lack an allow-list
/// entry with a non-empty reason, plus allow-list entries with an empty
/// reason (reported against [`ALLOW_LIST_REL`]). A `null` value counts: the
/// resolver's merge replaces a lower tier's value with `null`, so a tracked
/// `"runtimes": null` clears fleet policy and needs a reason like any override.
#[must_use]
pub fn check(repo_root: &Path) -> Vec<Finding> {
    let allow = read_allow_list(repo_root);
    let mut out = Vec::new();
    for (key, reason) in &allow {
        if reason.trim().is_empty() {
            out.push(Finding {
                key: key.clone(),
                file: repo_root.join(ALLOW_LIST_REL),
                message: format!("allow-list entry `{key}` has no reason"),
            });
        }
    }
    for file in tracked_tiers(repo_root) {
        let doc = soft_read_json_object(&file);
        let Some(obj) = doc.as_object() else { continue };
        for key in obj.keys() {
            if class_of(key) != Some(KeyClass::Fleet) {
                continue;
            }
            let allowed = allow.iter().any(|(k, r)| k == key && !r.trim().is_empty());
            if !allowed {
                out.push(Finding {
                    key: key.clone(),
                    file: file.clone(),
                    message: format!(
                        "fleet-policy key `{key}` is committed in a tracked repo tier and \
                         shadows fleet-gitops; move it to fleet-gitops or add an entry with a \
                         reason to {ALLOW_LIST_REL}"
                    ),
                });
            }
        }
    }
    out
}

/// A repo-tier leaf of a fleet key that differs from tier 1.
#[derive(Debug, Clone, PartialEq)]
pub struct Override {
    pub key: String,
    pub file: PathBuf,
    /// Tier-1 value (`None` when the fleet tier does not set the key).
    pub tier1: Option<Value>,
    pub effective: Value,
}

fn collect_leaves(prefix: &str, v: &Value, out: &mut Vec<(String, Value)>) {
    match v {
        Value::Object(m) => {
            for (k, child) in m {
                if k.starts_with('_') {
                    continue;
                }
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                collect_leaves(&path, child, out);
            }
        }
        other => out.push((prefix.to_string(), other.clone())),
    }
}

/// Pure core of [`overrides`]: compare each fleet-key leaf of the tracked
/// tier contributions against `tier1`. A leaf equal to tier 1 is not an override.
#[must_use]
pub fn overrides_against(
    tier1: &Value,
    tiers: &[(PathBuf, Value)],
    effective: &Value,
) -> Vec<Override> {
    let mut out = Vec::new();
    for (file, doc) in tiers {
        let Some(obj) = doc.as_object() else { continue };
        for (key, val) in obj {
            if class_of(key) != Some(KeyClass::Fleet) {
                continue;
            }
            let mut leaves = Vec::new();
            collect_leaves(key, val, &mut leaves);
            for (path, leaf) in leaves {
                let t1 = get_path(tier1, &path).filter(|v| !v.is_null());
                if t1 == Some(&leaf) {
                    continue;
                }
                out.push(Override {
                    key: path.clone(),
                    file: file.clone(),
                    tier1: t1.cloned(),
                    effective: get_path(effective, &path).cloned().unwrap_or(Value::Null),
                });
            }
        }
    }
    out
}

/// Repo-tier overrides of fleet keys for `repo_root`, against the real tier 1.
#[must_use]
pub fn overrides(repo_root: &Path) -> Vec<Override> {
    let tier1 = crate::config_resolver::private_defaults_path()
        .map(|p| soft_read_json_object(&p))
        .unwrap_or(Value::Null);
    let tiers: Vec<(PathBuf, Value)> = tracked_tiers(repo_root)
        .into_iter()
        .map(|p| {
            let v = soft_read_json_object(&p);
            (p, v)
        })
        .collect();
    overrides_against(&tier1, &tiers, &resolve_effective_config(repo_root))
}

/// Human-readable report lines for [`overrides`]; empty when there are none.
#[must_use]
pub fn render_overrides(repo_root: &Path) -> Vec<String> {
    overrides(repo_root)
        .into_iter()
        .map(|o| {
            let t1 = o
                .tier1
                .map_or_else(|| "(unset)".to_string(), |v| v.to_string());
            format!(
                "{}: fleet (tier 1) = {t1}, effective = {} [{}]",
                o.key,
                o.effective,
                o.file.display()
            )
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config_resolver::PRIVATE_DEFAULTS_ENV;
    use serde_json::json;
    use serial_test::serial;
    use tempfile::tempdir;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn classification_has_no_duplicate_keys() {
        for (i, (k, _)) in CLASSIFICATION.iter().enumerate() {
            assert!(CLASSIFICATION[i + 1..].iter().all(|(o, _)| o != k), "duplicate {k}");
        }
    }

    #[test]
    fn classification_covers_every_top_level_key_of_the_shipped_and_committed_configs() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        for rel in ["defaults/config.json", ".loom/config.json"] {
            let doc = soft_read_json_object(&root.join(rel));
            for key in doc.as_object().unwrap().keys() {
                assert!(class_of(key).is_some(), "{rel}: top-level key `{key}` is unclassified");
            }
        }
    }

    #[test]
    fn template_seeds_no_fleet_policy_keys_beyond_the_inert_ones() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let doc = soft_read_json_object(&root.join("defaults/config.json"));
        for key in doc.as_object().unwrap().keys() {
            assert_ne!(class_of(key), Some(KeyClass::Fleet), "template seeds fleet key `{key}`");
        }
    }

    #[test]
    fn fleet_key_in_tracked_tier_without_allow_entry_is_a_finding() {
        let dir = tempdir().unwrap();
        write(
            &dir.path().join(LEGACY_CONFIG_REL),
            r#"{"runtimes":{"default":"codex"},"buildGate":{}}"#,
        );
        let f = check(dir.path());
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].key, "runtimes");
    }

    #[test]
    fn allow_entry_with_reason_clears_the_finding() {
        let dir = tempdir().unwrap();
        write(&dir.path().join(LEGACY_CONFIG_REL), r#"{"runtimes":{"default":"codex"}}"#);
        write(
            &dir.path().join(ALLOW_LIST_REL),
            r#"{"allow":[{"key":"runtimes","reason":"migrating, see #11441"}]}"#,
        );
        assert!(check(dir.path()).is_empty());
    }

    #[test]
    fn allow_entry_with_empty_reason_is_rejected_and_does_not_allow() {
        let dir = tempdir().unwrap();
        write(&dir.path().join(PROJECT_CONFIG_REL), r#"{"autonomous":{"x":1}}"#);
        write(
            &dir.path().join(ALLOW_LIST_REL),
            r#"{"allow":[{"key":"autonomous","reason":"  "}]}"#,
        );
        let f = check(dir.path());
        assert_eq!(f.len(), 2, "{f:?}");
    }

    #[test]
    fn repo_and_comment_keys_are_not_findings() {
        let dir = tempdir().unwrap();
        write(
            &dir.path().join(LEGACY_CONFIG_REL),
            r#"{"_comment":"x","terminals":[],"version":"2","buildGate":{}}"#,
        );
        assert!(check(dir.path()).is_empty());
    }

    #[test]
    fn null_fleet_key_in_either_tracked_tier_is_a_finding() {
        // The resolver's merge replaces a lower tier with null, so a null
        // fleet key clears fleet-gitops policy and must not bypass the guard.
        for rel in [LEGACY_CONFIG_REL, PROJECT_CONFIG_REL] {
            let dir = tempdir().unwrap();
            write(&dir.path().join(rel), r#"{"runtimes":null,"autonomous":null}"#);
            let f = check(dir.path());
            let mut keys: Vec<_> = f.iter().map(|x| x.key.as_str()).collect();
            keys.sort_unstable();
            assert_eq!(keys, ["autonomous", "runtimes"], "{rel}: {f:?}");
            assert!(f.iter().all(|x| x.file == dir.path().join(rel)), "{rel}: {f:?}");
        }
    }

    #[test]
    fn null_fleet_key_is_cleared_only_by_a_reasoned_allow_entry() {
        for rel in [LEGACY_CONFIG_REL, PROJECT_CONFIG_REL] {
            let dir = tempdir().unwrap();
            write(&dir.path().join(rel), r#"{"runtimes":null,"autonomous":null}"#);
            write(
                &dir.path().join(ALLOW_LIST_REL),
                r#"{"allow":[{"key":"runtimes","reason":"cleared on purpose, see #11441"},
                            {"key":"autonomous","reason":""}]}"#,
            );
            let f = check(dir.path());
            // `autonomous` stays a finding (empty reason) and the empty-reason
            // entry is itself reported; reasoned `runtimes` is allowed.
            assert_eq!(f.len(), 2, "{rel}: {f:?}");
            assert!(f.iter().all(|x| x.key == "autonomous"), "{rel}: {f:?}");
            assert!(f.iter().any(|x| x.file == dir.path().join(rel)), "{rel}: {f:?}");
        }
    }

    #[test]
    fn same_value_as_tier1_is_not_an_override_but_a_differing_leaf_is() {
        let tier1 = json!({"runtimes": {"default": "claude", "preference": ["a"]}});
        let repo = json!({"runtimes": {"default": "claude", "preference": ["b"], "_comment": "x"},
                          "terminals": []});
        let effective = crate::config_resolver::deep_merge(&tier1, &repo);
        let o = overrides_against(&tier1, &[(PathBuf::from("r.json"), repo)], &effective);
        assert_eq!(o.len(), 1, "{o:?}");
        assert_eq!(o[0].key, "runtimes.preference");
        assert_eq!(o[0].tier1, Some(json!(["a"])));
        assert_eq!(o[0].effective, json!(["b"]));
    }

    #[test]
    #[serial(loom_config_env)]
    fn repo_without_a_loom_config_resolves_from_tier1() {
        let host = tempdir().unwrap();
        let tier1 = host.path().join("defaults.json");
        write(&tier1, r#"{"runtimes":{"defaultModelProfile":"zai-glm"}}"#);
        std::env::set_var(PRIVATE_DEFAULTS_ENV, &tier1);
        let repo = tempdir().unwrap();
        let effective = resolve_effective_config(repo.path());
        let o = overrides(repo.path());
        std::env::remove_var(PRIVATE_DEFAULTS_ENV);
        assert_eq!(effective, json!({"runtimes":{"defaultModelProfile":"zai-glm"}}));
        assert!(o.is_empty());
    }
}
