//! Fleet-policy key classification and the "tracked tiers must not shadow the
//! fleet" guard (Issue #11441, slice 1).
//!
//! Tier 1 (the machine-level `defaults.json` rendered from fleet-gitops) is
//! where fleet behaviour is decided. Tier 2/3 (`.loom/config.json`,
//! `.loom-project/project.json`) are deep-merged **over** it, so any fleet key
//! a repo commits silently wins for that repo. [`CLASSIFICATION`] is the one
//! machine-readable source of which config keys are fleet policy — top-level
//! keys plus dotted sub-key carve-outs (longest prefix wins, see
//! [`class_of`]); `loom-daemon validate`, the CI guard (`validate --strict`)
//! and `loom-daemon status` all read it so the three cannot drift. The human
//! table is in `.loom/docs/fleet-config-lifecycle.md`.
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
/// loom config, plus dotted sub-key rows that carve a more specific class out
/// of a differently-classified parent (Issue #11451 slice 3). Keys not listed
/// are unclassified and never flagged; see [`class_of`] for how a dotted path
/// resolves against this table (longest prefix wins).
pub const CLASSIFICATION: &[(&str, KeyClass)] = &[
    ("autonomous", KeyClass::Fleet),
    // Genuinely per-repo (work_finder.rs / cli/skip_labels.rs, #6685): repos
    // pick which labels their own sweeps never take, so a repo-tier value is
    // legitimate, not a shadowing of fleet policy.
    ("autonomous.workFinder.extraSkipLabels", KeyClass::Repo),
    ("daemon", KeyClass::Fleet),
    ("fleet", KeyClass::Fleet),
    ("forge", KeyClass::Repo),
    // Fleet-shared credentials/connection policy: a repo-tier value would
    // silently re-point the fleet's forge wiring for that repo alone.
    ("forge.githubApp", KeyClass::Fleet),
    ("forge.identities", KeyClass::Fleet),
    ("forge.readPool", KeyClass::Fleet),
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

/// Classification of a dotted config path, **longest prefix wins**: the exact
/// row for the full path first, then its dot-boundary ancestor prefixes (so
/// `forge.githubAppX` never matches the `forge.githubApp` row — candidates are
/// only prefixes of the path's own segments). A Repo/Host sub-key under a
/// Fleet parent is a deliberate carve-out, and a Fleet sub-key under a Repo
/// parent (e.g. `forge.githubApp`) is still fleet policy. Paths covered by no
/// row are unclassified and never flagged.
#[must_use]
pub fn class_of(key: &str) -> Option<KeyClass> {
    let mut candidate = key;
    loop {
        if let Some((_, class)) = CLASSIFICATION.iter().find(|(k, _)| *k == candidate) {
            return Some(*class);
        }
        let dot = candidate.rfind('.')?;
        candidate = &candidate[..dot];
    }
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

/// Whether an allow-list entry (with a non-empty reason) covers a finding
/// path. An entry names either the exact dotted path or an ancestor of it, so
/// a top-level entry (`forge`) still covers a Fleet sub-key finding
/// (`forge.githubApp`) under it — the same covering relation the old
/// top-level-only classification had.
fn allow_covers(key: &str, allow: &[(String, String)]) -> bool {
    allow.iter().any(|(k, r)| {
        !r.trim().is_empty()
            && (k == key
                || key
                    .strip_prefix(k.as_str())
                    .is_some_and(|rest| rest.starts_with('.')))
    })
}

/// Is the subtree at `path` **entirely** carved out to Repo/Host by sub-key
/// rows, leaving no fleet policy in it? Only a non-empty object all of whose
/// children are carved (or recursively carved) qualifies — a leaf, array,
/// `null`, or empty object is fleet content by presence, matching the old
/// top-level guard's strictness. An `autonomous` holding only
/// `workFinder.extraSkipLabels` is fully carved; one that also sets
/// `workFinder.enabled` is not.
fn subtree_fully_carved(path: &str, v: &Value) -> bool {
    match v {
        Value::Object(map) if !map.is_empty() => map.iter().all(|(k, child)| {
            let child_path = format!("{path}.{k}");
            match class_of(&child_path) {
                Some(KeyClass::Repo) | Some(KeyClass::Host) => true,
                _ => subtree_fully_carved(&child_path, child),
            }
        }),
        _ => false,
    }
}

/// Dotted paths of the fleet nodes in one tracked-tier document: the topmost
/// node of each Fleet-classified subtree that is not fully carved out by
/// sub-key rows. Stops descending at a flagged node (one finding covers its
/// whole subtree) and at fully-carved Fleet nodes.
fn collect_fleet_paths(path: &str, v: &Value, out: &mut Vec<String>) {
    if class_of(path) == Some(KeyClass::Fleet) {
        if !subtree_fully_carved(path, v) {
            out.push(path.to_string());
        }
        return;
    }
    if let Value::Object(map) = v {
        for (k, child) in map {
            let child_path = format!("{path}.{k}");
            collect_fleet_paths(&child_path, child, out);
        }
    }
}

/// Fleet-class keys present in the tracked tiers that lack an allow-list
/// entry with a non-empty reason, plus allow-list entries with an empty
/// reason (reported against [`ALLOW_LIST_REL`]). A `null` value counts: the
/// resolver's merge replaces a lower tier's value with `null`, so a tracked
/// `"runtimes": null` clears fleet policy and needs a reason like any override.
///
/// Documents are walked as dotted paths and classified with [`class_of`] at
/// every node, so sub-key rows carve Repo content out of a Fleet parent
/// (`autonomous.workFinder.extraSkipLabels`) and flag Fleet content under a
/// Repo parent (`forge.githubApp`). `autonomous` holding only
/// `extraSkipLabels` produces no finding, while `autonomous` that also sets
/// `workFinder.enabled` is flagged once, at `autonomous`.
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
        let mut fleet_paths = Vec::new();
        for (key, val) in obj {
            collect_fleet_paths(key, val, &mut fleet_paths);
        }
        for path in fleet_paths {
            if allow_covers(&path, &allow) {
                continue;
            }
            out.push(Finding {
                key: path.clone(),
                file: file.clone(),
                message: format!(
                    "fleet-policy key `{path}` is committed in a tracked repo tier and \
                     shadows fleet-gitops; move it to fleet-gitops or add an entry with a \
                     reason to {ALLOW_LIST_REL}"
                ),
            });
        }
    }
    out
}

/// The fleet-class keys the pre-#11445 install template seeded into every new
/// `.loom/config.json`, with their exact seed values (recovered from
/// `git show 0c1c33da2^:defaults/config.json`). The upgrade path in
/// [`crate::init`] removes such a key from an installed config **only** when
/// its committed value equals the seed exactly — a single field changed by an
/// operator makes the value real configuration, which stays in place and is
/// reported by [`check`] so the operator can migrate or allow-list it.
#[must_use]
pub fn legacy_template_seeds() -> Vec<(&'static str, Value)> {
    vec![
        ("offlineMode", Value::Bool(false)),
        ("runtimes", serde_json::json!({ "default": "claude" })),
        (
            "safehouse",
            serde_json::json!({
                "enabled": false,
                "socket": null,
                "room": null,
                "persona": "loom_daemon",
                "operatorMention": null,
                "workerPersonas": [],
                "mcpCommand": "safehouse-mcp",
                "peerClaimTtlSecs": 120,
                "chatops": {
                    "enabled": false,
                    "allowedSenders": [],
                    "room": null,
                    "confirmTtlSecs": 120
                }
            }),
        ),
    ]
}

/// Remove legacy template-seeded fleet keys (see [`legacy_template_seeds`])
/// from an existing consumer config in place, returning the dotted names
/// removed. A key is removed only when (a) the current template no longer
/// provides it (otherwise the merge's template base would re-add it and the
/// removal could not stick), and (b) the committed value equals the seed
/// exactly. `autonomous` has no seed to compare and is never touched.
pub fn strip_legacy_template_seeds(existing: &mut Value, template: &Value) -> Vec<String> {
    let mut removed = Vec::new();
    let (Some(existing_obj), Some(template_obj)) = (existing.as_object_mut(), template.as_object())
    else {
        return removed;
    };
    for (key, seed) in legacy_template_seeds() {
        if template_obj.contains_key(key) {
            continue;
        }
        if existing_obj.get(key) == Some(&seed) {
            existing_obj.remove(key);
            removed.push(key.to_string());
        }
    }
    removed
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
    fn template_seeds_no_fleet_classified_sub_keys() {
        // Sub-key rows (#11451) classify nested paths too, so the shipped
        // template must not hide a Fleet sub-key under a Repo parent (e.g. a
        // `forge.githubApp` seed would re-introduce the shadowing this guard
        // exists to stop).
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let doc = soft_read_json_object(&root.join("defaults/config.json"));
        for (key, val) in doc.as_object().unwrap() {
            let mut paths = Vec::new();
            collect_fleet_paths(key, val, &mut paths);
            assert!(paths.is_empty(), "template seeds fleet key(s) {paths:?}");
        }
    }

    #[test]
    fn classification_matches_only_at_dot_boundaries_and_longest_prefix_wins() {
        assert_eq!(class_of("forge.githubApp"), Some(KeyClass::Fleet));
        assert_eq!(class_of("forge.identities"), Some(KeyClass::Fleet));
        assert_eq!(class_of("forge.readPool"), Some(KeyClass::Fleet));
        assert_eq!(class_of("forge.type"), Some(KeyClass::Repo), "via `forge`");
        // The row is `forge.githubApp`, not a raw string prefix: `…AppX` must
        // not match it and fall through to `forge` (Repo).
        assert_eq!(class_of("forge.githubAppX"), Some(KeyClass::Repo));
        assert_eq!(class_of("autonomous.workFinder.extraSkipLabels"), Some(KeyClass::Repo));
        assert_eq!(class_of("autonomous.workFinder"), Some(KeyClass::Fleet), "via `autonomous`");
        assert_eq!(class_of("autonomous.workFinder.enabled"), Some(KeyClass::Fleet));
        assert_eq!(class_of("unknown"), None);
        assert_eq!(class_of("unknown.sub"), None);
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
    fn repo_sub_key_extra_skip_labels_in_a_tracked_tier_is_not_flagged() {
        // AC (issue #11451): `extraSkipLabels` is a documented per-repo knob
        // (#6685), so `autonomous` holding only it produces no finding.
        let dir = tempdir().unwrap();
        write(
            &dir.path().join(LEGACY_CONFIG_REL),
            r#"{"autonomous":{"workFinder":{"extraSkipLabels":["journal"]}}}"#,
        );
        assert!(check(dir.path()).is_empty());
    }

    #[test]
    fn autonomous_with_extra_skip_labels_plus_any_other_knob_is_flagged_once_at_the_parent() {
        // Gotcha from the issue: the Repo carve-out must not let a genuinely
        // fleet knob (`workFinder.enabled`) hide beside it.
        let dir = tempdir().unwrap();
        write(
            &dir.path().join(LEGACY_CONFIG_REL),
            r#"{"autonomous":{"workFinder":{"extraSkipLabels":["journal"],"enabled":true}}}"#,
        );
        let f = check(dir.path());
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].key, "autonomous");
    }

    #[test]
    fn forge_fleet_sub_keys_are_flagged_and_forge_type_is_not() {
        // AC (issue #11451): `forge.githubApp`/`identities`/`readPool` are
        // Fleet under the Repo `forge` parent; `forge.type` stays repo-side.
        let dir = tempdir().unwrap();
        write(
            &dir.path().join(LEGACY_CONFIG_REL),
            r#"{"forge":{"type":"github","githubApp":{"appId":1},"identities":{},"readPool":{}}}"#,
        );
        let f = check(dir.path());
        let mut keys: Vec<_> = f.iter().map(|x| x.key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["forge.githubApp", "forge.identities", "forge.readPool"], "{f:?}");
    }

    #[test]
    fn dotted_allow_entry_covers_only_its_own_subtree() {
        let dir = tempdir().unwrap();
        write(
            &dir.path().join(LEGACY_CONFIG_REL),
            r#"{"forge":{"githubApp":{"appId":1},"identities":{}}}"#,
        );
        write(
            &dir.path().join(ALLOW_LIST_REL),
            r#"{"allow":[{"key":"forge.githubApp","reason":"migrating, see #11441"}]}"#,
        );
        let f = check(dir.path());
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].key, "forge.identities");
    }

    #[test]
    fn top_level_allow_entry_covers_fleet_sub_keys() {
        let dir = tempdir().unwrap();
        write(&dir.path().join(LEGACY_CONFIG_REL), r#"{"forge":{"githubApp":{"appId":1}}}"#);
        write(
            &dir.path().join(ALLOW_LIST_REL),
            r#"{"allow":[{"key":"forge","reason":"migrating, see #11441"}]}"#,
        );
        assert!(check(dir.path()).is_empty());
    }

    #[test]
    fn legacy_template_seeds_match_the_pre_11445_template() {
        // Values recovered from `git show 0c1c33da2^:defaults/config.json`.
        let seeds: std::collections::HashMap<_, _> = legacy_template_seeds().into_iter().collect();
        assert_eq!(seeds["offlineMode"], Value::Bool(false));
        assert_eq!(seeds["runtimes"], json!({ "default": "claude" }));
        assert_eq!(seeds["safehouse"]["enabled"], Value::Bool(false));
        assert_eq!(seeds["safehouse"]["persona"], "loom_daemon");
        assert_eq!(seeds["safehouse"]["mcpCommand"], "safehouse-mcp");
        assert_eq!(seeds["safehouse"]["peerClaimTtlSecs"], json!(120));
        assert_eq!(seeds["safehouse"]["chatops"]["confirmTtlSecs"], json!(120));
        assert_eq!(seeds["safehouse"]["chatops"]["allowedSenders"], json!([]));
    }

    #[test]
    fn strip_removes_exact_seeds_and_keeps_modified_values() {
        let template = json!({ "version": "2" });
        let mut existing = json!({
            "offlineMode": false,
            "runtimes": { "default": "codex" },
            "safehouse": {
                "enabled": true,
                "socket": null,
                "room": null,
                "persona": "loom_daemon",
                "operatorMention": null,
                "workerPersonas": [],
                "mcpCommand": "safehouse-mcp",
                "peerClaimTtlSecs": 120,
                "chatops": { "enabled": false, "allowedSenders": [], "room": null,
                             "confirmTtlSecs": 120 }
            },
            "autonomous": { "roleRunner": { "onIdle": ["auditor"] } }
        });
        let removed = strip_legacy_template_seeds(&mut existing, &template);
        assert_eq!(removed, ["offlineMode"]);
        assert_eq!(existing["runtimes"]["default"], "codex", "a modified seed value is content");
        assert_eq!(existing["safehouse"]["enabled"], Value::Bool(true));
        assert_eq!(existing["autonomous"]["roleRunner"]["onIdle"], json!(["auditor"]));
    }

    #[test]
    fn strip_is_a_no_op_when_the_template_still_provides_the_key() {
        // If the template re-provided a seeded key, the merge's base would
        // re-add it and the removal could not stick — so the strip must not
        // fire (this also keeps the merge byte-idempotent for such a template).
        let template = json!({ "offlineMode": false });
        let mut existing = json!({ "offlineMode": false });
        assert!(strip_legacy_template_seeds(&mut existing, &template).is_empty());
        assert_eq!(existing["offlineMode"], Value::Bool(false));
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
