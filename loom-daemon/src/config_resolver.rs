//! Config resolution layer (#4039, Epic #3835 Phase 2).
//!
//! A single resolver over the tier chain: private/shared defaults, the
//! legacy `.loom/config.json`, tracked `.loom-project/project.json`, and
//! ignored `.loom-local/local.json`. See
//! `docs/design/config-resolution-tiers.md` for the full design (schema,
//! precedence table, and how this composes with the existing **env >
//! config > default** rule used by e.g. the `autonomous` block).
//!
//! **This module is additive only.** No existing call site is migrated onto
//! it in this PR — every current direct `.loom/config.json` reader (e.g.
//! [`crate::extract_configured_terminal_ids`],
//! [`crate::work_finder::read_work_finder_config`],
//! [`crate::main_health_gate::read_build_gate_config`]) is untouched. A
//! follow-up issue migrates call sites one at a time.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

/// Repo-relative path to the legacy, currently load-bearing config file.
/// Every existing repo has (at most) this tier populated.
pub const LEGACY_CONFIG_REL: &str = ".loom/config.json";

/// Repo-relative path to the tracked, project-specific config file
/// introduced by Epic #3835. Empty (absent) in every repo until a repo
/// migrates onto the new layout (Phase 6).
pub const PROJECT_CONFIG_REL: &str = ".loom-project/project.json";

/// Repo-relative path to the ignored, host-local config override file.
/// Empty (absent) in every repo until Phase 3+.
pub const LOCAL_CONFIG_REL: &str = ".loom-local/local.json";

/// Env var overriding the private/shared defaults file location. Set to a
/// non-empty path to point at an alternate file; set to the empty string to
/// disable this tier entirely (it is already a no-op on any host that
/// hasn't run the Phase 3 machine-level installer, so disabling it has no
/// practical effect today — the toggle exists for forward compatibility and
/// test isolation).
pub const PRIVATE_DEFAULTS_ENV: &str = "LOOM_CONFIG_DEFAULTS_FILE";

/// Home-relative default location of the private/shared defaults file. Epic
/// #3835 Phase 3 introduces the machine-level checkout this lives in; until
/// that phase ships the file simply does not exist on any host, so this
/// tier resolves to "contributes nothing" everywhere today.
const DEFAULT_PRIVATE_DEFAULTS_REL: &str = ".local/share/loom/config/defaults.json";

/// Resolve the path to the private/shared defaults file, honoring
/// [`PRIVATE_DEFAULTS_ENV`]. Returns `None` when the env var is explicitly
/// set to an empty string (tier disabled) or when no home directory can be
/// determined.
#[must_use]
pub fn private_defaults_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var(PRIVATE_DEFAULTS_ENV) {
        return if p.is_empty() {
            None
        } else {
            Some(PathBuf::from(p))
        };
    }
    dirs::home_dir().map(|h| h.join(DEFAULT_PRIVATE_DEFAULTS_REL))
}

/// Soft-fail JSON-object read: a missing file, an unreadable file, malformed
/// JSON, or a non-object top-level value all resolve to an empty object
/// (`{}`) — that tier simply contributes nothing to the merge. Never a hard
/// error, mirroring the soft-fail contract already used by
/// `work_finder::read_work_finder_config` and
/// `main_health_gate::read_build_gate_config`.
fn soft_read_json_object(path: &Path) -> Value {
    let text = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            log::debug!("config_resolver: could not read {}: {e}", path.display());
            return Value::Object(Map::new());
        }
    };

    match serde_json::from_str::<Value>(&text) {
        Ok(v @ Value::Object(_)) => v,
        Ok(_) => {
            log::warn!(
                "config_resolver: top-level of {} is not a JSON object; ignoring this tier",
                path.display()
            );
            Value::Object(Map::new())
        }
        Err(e) => {
            log::warn!("config_resolver: could not parse {}: {e}", path.display());
            Value::Object(Map::new())
        }
    }
}

/// A loud, top-of-boot-block diagnosis of the **legacy** `.loom/config.json`
/// tier only (#6499) — the one tier every existing repo actually has
/// populated, and the one a host-local patch (e.g. `safehouse.socket`,
/// `observability.ingestKeyFile`, `daemon.delegatedTo`) lives in today ahead
/// of #5457's durable fix. `resolve_effective_config`'s own
/// [`soft_read_json_object`] already soft-fails a parse error to `{}` at
/// `WARN` on *every* call (there are dozens of call sites, so that WARN
/// repeats once per resolve, not once per boot) — this function exists
/// alongside it, not instead of it, so a caller that wants exactly one loud
/// diagnosis at daemon startup can get one without touching the low-level
/// per-tier soft-fail contract every other caller in this module relies on.
///
/// Returns `None` when the file is absent (nothing to diagnose — the normal
/// "no legacy config at all" case, not an error) or parses as a JSON object
/// (the healthy case). Returns `Some(<diagnosis>)` — a message naming the
/// concrete parse failure — for a present-but-unreadable file, malformed
/// JSON (including the literal `<<<<<<<`/`=======`/`>>>>>>>` conflict-marker
/// shape #6499 was filed over), or valid JSON whose top level is not an
/// object (the "wrong JSON" edge case `soft_read_json_object` already has a
/// separate branch for, e.g. an accidental top-level array).
#[must_use]
pub fn diagnose_legacy_config(repo_root: &Path) -> Option<String> {
    let path = repo_root.join(LEGACY_CONFIG_REL);

    let text = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => return Some(format!("could not read {}: {e}", path.display())),
    };

    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(_)) => None,
        Ok(_) => Some(format!(
            "top-level of {} is not a JSON object (expected `{{...}}`)",
            path.display()
        )),
        Err(e) => {
            let marker_hint = if text.contains("<<<<<<<") || text.contains(">>>>>>>") {
                " — contains literal '<<<<<<<'/'>>>>>>>' conflict markers, the signature of an \
                 abandoned `git stash pop` (or merge/cherry-pick) left unresolved in the working \
                 tree; see .loom/docs/troubleshooting.md for recovery"
            } else {
                ""
            };
            Some(format!("could not parse {}: {e}{marker_hint}", path.display()))
        }
    }
}

/// Deep-merge `overlay` onto `base`, returning the merged value.
///
/// Semantics (matching `jq`'s `*` operator exactly, so the Bash resolver can
/// implement this via `jq -n '$a * $b * $c * $d'` and stay in lockstep):
/// - When both sides are JSON objects, merge recursively key-by-key.
/// - Otherwise, `overlay` replaces `base` outright — including when
///   `overlay` is `null` (an explicit `null` in a higher tier means "clear
///   this key", not "leave the lower tier's value").
///
/// A tier that is entirely absent/malformed is represented as `{}` by
/// [`soft_read_json_object`] before it ever reaches this function, so an
/// absent tier is a true no-op merge (it never introduces a spurious
/// top-level `null`).
#[must_use]
pub fn deep_merge(base: &Value, overlay: &Value) -> Value {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            let mut merged = base_map.clone();
            for (k, v) in overlay_map {
                let merged_val = match merged.get(k) {
                    Some(existing) => deep_merge(existing, v),
                    None => v.clone(),
                };
                merged.insert(k.clone(), merged_val);
            }
            Value::Object(merged)
        }
        _ => overlay.clone(),
    }
}

/// Resolve the full effective config tree for `repo_root` by deep-merging,
/// lowest to highest precedence:
///
/// 1. Private/shared defaults ([`private_defaults_path`]) — a no-op tier on
///    every host until Epic #3835 Phase 3 ships.
/// 2. Legacy `.loom/config.json` — the sole tier every existing repo has.
/// 3. Tracked `.loom-project/project.json` — new in #4039; empty until a
///    repo migrates (Phase 6).
/// 4. Ignored `.loom-local/local.json` — new in #4039; host-local override.
///
/// A missing or malformed file at **any** tier soft-fails to that tier
/// contributing nothing, and this function never aborts the caller. When
/// only tier 2 has content — the state of every repo today — the result is
/// exactly that file's parsed content: byte-for-byte identical to reading
/// `.loom/config.json` directly, satisfying the #4039 behavior-preservation
/// requirement.
#[must_use]
pub fn resolve_effective_config(repo_root: &Path) -> Value {
    let mut effective = Value::Object(Map::new());

    if let Some(defaults_path) = private_defaults_path() {
        effective = deep_merge(&effective, &soft_read_json_object(&defaults_path));
    }

    effective = deep_merge(&effective, &soft_read_json_object(&repo_root.join(LEGACY_CONFIG_REL)));
    effective = deep_merge(&effective, &soft_read_json_object(&repo_root.join(PROJECT_CONFIG_REL)));
    effective = deep_merge(&effective, &soft_read_json_object(&repo_root.join(LOCAL_CONFIG_REL)));

    effective
}

/// The tier files consulted by [`resolve_effective_config`], **highest
/// precedence first** — the reverse of the merge order, i.e. the order in which
/// to search for "who actually supplied this value".
///
/// The private/shared defaults tier is included only when
/// [`private_defaults_path`] resolves (it is disabled by an empty
/// [`PRIVATE_DEFAULTS_ENV`], and absent on any host without the machine-level
/// checkout). Paths are returned whether or not the file exists — a caller that
/// cares reads it with the same soft-fail contract the merge uses.
#[must_use]
pub fn tier_paths_by_precedence(repo_root: &Path) -> Vec<PathBuf> {
    let mut paths = vec![
        repo_root.join(LOCAL_CONFIG_REL),
        repo_root.join(PROJECT_CONFIG_REL),
        repo_root.join(LEGACY_CONFIG_REL),
    ];
    paths.extend(private_defaults_path());
    paths
}

/// Which tier file actually supplied `dotted` — the highest-precedence tier in
/// which the key is present and non-null, or `None` when no tier sets it (the
/// caller's env/default fallback is what supplied the value).
///
/// This exists because a **multi-workspace** daemon makes "which
/// `.loom/config.json` did that number come from?" a real operator question
/// (#4512): the effective config is a merge across up to four files, and a knob
/// that reads as `3` might be a repo's committed value, a host-local override,
/// or the built-in default. Reporting the provenance alongside the value —
/// rather than only the value — is the same fix `token_pool_dir` applied to the
/// token pool's directory ambiguity in #4292.
///
/// Soft-fail throughout: an unreadable or malformed tier simply cannot be the
/// source, exactly as it contributes nothing to the merge.
#[must_use]
pub fn source_of(repo_root: &Path, dotted: &str) -> Option<PathBuf> {
    tier_paths_by_precedence(repo_root)
        .into_iter()
        .find(|path| get_path(&soft_read_json_object(path), dotted).is_some_and(|v| !v.is_null()))
}

/// Look up a dotted key path (e.g. `"autonomous.workFinder.enabled"`) in an
/// already-resolved effective config tree. Returns `None` on any missing
/// segment or when a non-object value is indexed further.
///
/// Callers apply their own env-override / built-in-default fallback on top
/// of this — exactly as before. This resolver replaces only the "config"
/// position in the existing **env > config > default** chain; it does not
/// itself know about any env var.
#[must_use]
pub fn get_path<'a>(config: &'a Value, dotted: &str) -> Option<&'a Value> {
    let mut cur = config;
    for segment in dotted.split('.') {
        cur = cur.get(segment)?;
    }
    Some(cur)
}

/// Dotted key read by [`daemon_delegated_to`].
const DAEMON_DELEGATED_TO_KEY: &str = "daemon.delegatedTo";

/// Read the `daemon.delegatedTo` config key for `repo_root` (issue #5345):
/// when set, this repo declares that daemon-*administration* actions
/// (workspace registry mutation, token-pool bootstrap) are performed from
/// the named delegate repo instead, and every gated CLI entry point refuses
/// with a pointer to it. Daemon-*client* actions (dispatch, list, status,
/// cancel) are unaffected — see the call sites in
/// `cli::workspace_fleet::handle_workspace_command` and
/// `cli::tokens::handle_tokens_command`.
///
/// Soft-fails to `None` — the default-off, behavior-preserving case for
/// every repo today — on a missing key, a missing/malformed config file, or
/// a non-string value, matching the rest of this module's tolerant-parse
/// contract (see [`resolve_effective_config`]/[`soft_read_json_object`]).
#[must_use]
pub fn daemon_delegated_to(repo_root: &Path) -> Option<String> {
    let effective = resolve_effective_config(repo_root);
    get_path(&effective, DAEMON_DELEGATED_TO_KEY)
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use serial_test::serial;
    use tempfile::tempdir;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    // ===== deep_merge =====

    #[test]
    fn test_deep_merge_disjoint_keys_union() {
        let base = json!({"a": 1});
        let overlay = json!({"b": 2});
        assert_eq!(deep_merge(&base, &overlay), json!({"a": 1, "b": 2}));
    }

    #[test]
    fn test_deep_merge_nested_objects_merge_recursively() {
        let base = json!({"a": {"x": 1}});
        let overlay = json!({"a": {"y": 2}});
        assert_eq!(deep_merge(&base, &overlay), json!({"a": {"x": 1, "y": 2}}));
    }

    #[test]
    fn test_deep_merge_scalar_overlay_replaces_base() {
        let base = json!({"a": 1});
        let overlay = json!({"a": 2});
        assert_eq!(deep_merge(&base, &overlay), json!({"a": 2}));
    }

    #[test]
    fn test_deep_merge_explicit_null_overlay_clears_key() {
        let base = json!({"a": 1});
        let overlay = json!({"a": null});
        assert_eq!(deep_merge(&base, &overlay), json!({"a": null}));
    }

    #[test]
    fn test_deep_merge_empty_overlay_is_noop() {
        let base = json!({"a": {"x": 1}, "b": [1, 2]});
        let overlay = json!({});
        assert_eq!(deep_merge(&base, &overlay), base);
    }

    #[test]
    fn test_deep_merge_array_overlay_replaces_not_concatenates() {
        let base = json!({"a": [1, 2]});
        let overlay = json!({"a": [3]});
        assert_eq!(deep_merge(&base, &overlay), json!({"a": [3]}));
    }

    // ===== soft_read_json_object =====

    #[test]
    fn test_soft_read_missing_file_is_empty_object() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nope.json");
        assert_eq!(soft_read_json_object(&path), json!({}));
    }

    #[test]
    fn test_soft_read_malformed_json_is_empty_object() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.json");
        write(&path, "not valid json");
        assert_eq!(soft_read_json_object(&path), json!({}));
    }

    #[test]
    fn test_soft_read_non_object_top_level_is_empty_object() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("array.json");
        write(&path, "[1, 2, 3]");
        assert_eq!(soft_read_json_object(&path), json!({}));
    }

    #[test]
    fn test_soft_read_valid_object_passes_through() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ok.json");
        write(&path, r#"{"a": 1}"#);
        assert_eq!(soft_read_json_object(&path), json!({"a": 1}));
    }

    // ===== diagnose_legacy_config (#6499) =====

    #[test]
    fn test_diagnose_legacy_config_missing_file_is_none() {
        let dir = tempdir().unwrap();
        assert_eq!(diagnose_legacy_config(dir.path()), None);
    }

    #[test]
    fn test_diagnose_legacy_config_valid_object_is_none() {
        let dir = tempdir().unwrap();
        write(&dir.path().join(LEGACY_CONFIG_REL), r#"{"nextAgentNumber": 3}"#);
        assert_eq!(diagnose_legacy_config(dir.path()), None);
    }

    #[test]
    fn test_diagnose_legacy_config_malformed_json_names_the_parse_error() {
        let dir = tempdir().unwrap();
        write(&dir.path().join(LEGACY_CONFIG_REL), "{not valid json");
        let diagnosis = diagnose_legacy_config(dir.path());
        assert!(diagnosis.is_some());
        let msg = diagnosis.unwrap();
        assert!(msg.contains("could not parse"), "{msg}");
        assert!(msg.contains(LEGACY_CONFIG_REL), "{msg}");
    }

    #[test]
    fn test_diagnose_legacy_config_conflict_markers_get_a_specific_hint() {
        let dir = tempdir().unwrap();
        write(
            &dir.path().join(LEGACY_CONFIG_REL),
            "{\n  \"safehouse\": {\n<<<<<<< Updated upstream\n    \"room\": \"a\"\n=======\n    \"room\": \"b\"\n>>>>>>> Stashed changes\n  }\n}\n",
        );
        let diagnosis = diagnose_legacy_config(dir.path());
        assert!(diagnosis.is_some());
        let msg = diagnosis.unwrap();
        assert!(msg.contains("conflict markers"), "{msg}");
        assert!(msg.contains("git stash pop"), "{msg}");
    }

    #[test]
    fn test_diagnose_legacy_config_non_object_top_level_is_flagged() {
        let dir = tempdir().unwrap();
        write(&dir.path().join(LEGACY_CONFIG_REL), "[1, 2, 3]");
        let diagnosis = diagnose_legacy_config(dir.path());
        assert!(diagnosis.is_some());
        assert!(diagnosis.unwrap().contains("not a JSON object"));
    }

    #[test]
    fn test_diagnose_legacy_config_valid_non_conflict_malformed_json_has_no_marker_hint() {
        let dir = tempdir().unwrap();
        write(&dir.path().join(LEGACY_CONFIG_REL), r#"{"a": 1,}"#);
        let diagnosis = diagnose_legacy_config(dir.path());
        assert!(diagnosis.is_some());
        assert!(!diagnosis.unwrap().contains("conflict markers"));
    }

    // ===== private_defaults_path =====

    #[test]
    #[serial(loom_config_env)]
    fn test_private_defaults_path_env_override() {
        // `PRIVATE_DEFAULTS_ENV` (`LOOM_CONFIG_DEFAULTS_FILE`) is
        // process-global, and dozens of tests across this crate's *other*
        // modules already mutate it — all correctly serialized under the
        // shared, named `loom_config_env` `#[serial]` key (not the crate-wide
        // default/unnamed key). Every test in *this* file must use the same
        // named key too, or it races those other modules' tests despite both
        // sides believing they're serialized (issue #6177: a bare `#[serial]`
        // here — the default/unnamed key — does not exclude
        // `#[serial(loom_config_env)]` tests, which is exactly what caused
        // this file's flake).
        std::env::set_var(PRIVATE_DEFAULTS_ENV, "/tmp/custom-defaults.json");
        let resolved = private_defaults_path();
        std::env::remove_var(PRIVATE_DEFAULTS_ENV);
        assert_eq!(resolved, Some(PathBuf::from("/tmp/custom-defaults.json")));
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_private_defaults_path_empty_env_disables_tier() {
        std::env::set_var(PRIVATE_DEFAULTS_ENV, "");
        let resolved = private_defaults_path();
        std::env::remove_var(PRIVATE_DEFAULTS_ENV);
        assert_eq!(resolved, None);
    }

    // ===== resolve_effective_config: behavior preservation =====

    #[test]
    #[serial(loom_config_env)]
    fn test_resolve_only_legacy_tier_present_matches_legacy_content_exactly() {
        let dir = tempdir().unwrap();
        // Disable the private-defaults tier for deterministic test output
        // (it would otherwise read whatever happens to exist at
        // ~/.local/share/loom/config/defaults.json on the CI/dev host).
        std::env::set_var(PRIVATE_DEFAULTS_ENV, "");

        write(
            &dir.path().join(LEGACY_CONFIG_REL),
            r#"{"nextAgentNumber": 3, "autonomous": {"exampleTierProbe": 2}}"#,
        );

        let effective = resolve_effective_config(dir.path());
        std::env::remove_var(PRIVATE_DEFAULTS_ENV);

        assert_eq!(effective, json!({"nextAgentNumber": 3, "autonomous": {"exampleTierProbe": 2}}));
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_resolve_no_files_present_is_empty_object() {
        let dir = tempdir().unwrap();
        std::env::set_var(PRIVATE_DEFAULTS_ENV, "");
        let effective = resolve_effective_config(dir.path());
        std::env::remove_var(PRIVATE_DEFAULTS_ENV);
        assert_eq!(effective, json!({}));
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_resolve_malformed_legacy_config_soft_fails_never_aborts() {
        let dir = tempdir().unwrap();
        std::env::set_var(PRIVATE_DEFAULTS_ENV, "");
        write(&dir.path().join(LEGACY_CONFIG_REL), "{not json");

        write(&dir.path().join(PROJECT_CONFIG_REL), r#"{"buildGate": {"enabled": true}}"#);

        let effective = resolve_effective_config(dir.path());
        std::env::remove_var(PRIVATE_DEFAULTS_ENV);

        // Legacy tier contributed nothing (malformed), but the project tier
        // still resolved fine — one bad tier never blocks the others.
        assert_eq!(effective, json!({"buildGate": {"enabled": true}}));
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_resolve_precedence_local_overrides_project_overrides_legacy() {
        let dir = tempdir().unwrap();
        std::env::set_var(PRIVATE_DEFAULTS_ENV, "");

        write(&dir.path().join(LEGACY_CONFIG_REL), r#"{"a": "legacy", "shared": 1}"#);
        write(&dir.path().join(PROJECT_CONFIG_REL), r#"{"a": "project", "shared": 2}"#);
        write(&dir.path().join(LOCAL_CONFIG_REL), r#"{"a": "local"}"#);

        let effective = resolve_effective_config(dir.path());
        std::env::remove_var(PRIVATE_DEFAULTS_ENV);

        // `a` was set at all three tiers -> local (highest) wins.
        assert_eq!(effective.get("a"), Some(&json!("local")));
        // `shared` was overridden by project but not local -> project's
        // value wins over legacy's.
        assert_eq!(effective.get("shared"), Some(&json!(2)));
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_resolve_disjoint_keys_across_tiers_all_present() {
        let dir = tempdir().unwrap();
        std::env::set_var(PRIVATE_DEFAULTS_ENV, "");

        write(&dir.path().join(LEGACY_CONFIG_REL), r#"{"legacyOnly": 1}"#);
        write(&dir.path().join(PROJECT_CONFIG_REL), r#"{"projectOnly": 2}"#);
        write(&dir.path().join(LOCAL_CONFIG_REL), r#"{"localOnly": 3}"#);

        let effective = resolve_effective_config(dir.path());
        std::env::remove_var(PRIVATE_DEFAULTS_ENV);

        assert_eq!(effective, json!({"legacyOnly": 1, "projectOnly": 2, "localOnly": 3}));
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_resolve_nested_autonomous_block_merges_across_tiers() {
        let dir = tempdir().unwrap();
        std::env::set_var(PRIVATE_DEFAULTS_ENV, "");

        write(
            &dir.path().join(LEGACY_CONFIG_REL),
            r#"{"autonomous": {"workFinder": {"enabled": true}}}"#,
        );
        write(&dir.path().join(LOCAL_CONFIG_REL), r#"{"autonomous": {"exampleTierProbe": 4}}"#);

        let effective = resolve_effective_config(dir.path());
        std::env::remove_var(PRIVATE_DEFAULTS_ENV);

        assert_eq!(
            effective,
            json!({"autonomous": {"workFinder": {"enabled": true}, "exampleTierProbe": 4}})
        );
    }

    // ===== get_path =====

    #[test]
    fn test_get_path_resolves_nested_dotted_key() {
        let config = json!({"autonomous": {"workFinder": {"enabled": true}}});
        assert_eq!(get_path(&config, "autonomous.workFinder.enabled"), Some(&json!(true)));
    }

    #[test]
    fn test_get_path_missing_segment_returns_none() {
        let config = json!({"autonomous": {}});
        assert_eq!(get_path(&config, "autonomous.workFinder.enabled"), None);
    }

    #[test]
    fn test_get_path_indexing_through_scalar_returns_none() {
        let config = json!({"a": 1});
        assert_eq!(get_path(&config, "a.b"), None);
    }

    #[test]
    fn test_get_path_top_level_key() {
        let config = json!({"nextAgentNumber": 3});
        assert_eq!(get_path(&config, "nextAgentNumber"), Some(&json!(3)));
    }

    // ===== daemon_delegated_to (#5345) =====

    #[test]
    #[serial(loom_config_env)]
    fn test_daemon_delegated_to_reads_the_configured_string() {
        std::env::set_var(PRIVATE_DEFAULTS_ENV, "");
        let dir = tempdir().unwrap();
        write(
            &dir.path().join(LEGACY_CONFIG_REL),
            r#"{"daemon": {"delegatedTo": "/Users/alice/GitHub/other-repo"}}"#,
        );
        let delegate = daemon_delegated_to(dir.path());
        std::env::remove_var(PRIVATE_DEFAULTS_ENV);
        assert_eq!(delegate, Some("/Users/alice/GitHub/other-repo".to_string()));
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_daemon_delegated_to_default_off_when_key_absent() {
        std::env::set_var(PRIVATE_DEFAULTS_ENV, "");
        let dir = tempdir().unwrap();
        write(&dir.path().join(LEGACY_CONFIG_REL), r#"{"nextAgentNumber": 3}"#);
        let delegate = daemon_delegated_to(dir.path());
        std::env::remove_var(PRIVATE_DEFAULTS_ENV);
        assert_eq!(delegate, None);
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_daemon_delegated_to_missing_config_file_is_none() {
        std::env::set_var(PRIVATE_DEFAULTS_ENV, "");
        let dir = tempdir().unwrap();
        let delegate = daemon_delegated_to(dir.path());
        std::env::remove_var(PRIVATE_DEFAULTS_ENV);
        assert_eq!(delegate, None);
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_daemon_delegated_to_wrong_type_soft_fails_to_none() {
        std::env::set_var(PRIVATE_DEFAULTS_ENV, "");
        let dir = tempdir().unwrap();
        write(&dir.path().join(LEGACY_CONFIG_REL), r#"{"daemon": {"delegatedTo": 42}}"#);
        let delegate = daemon_delegated_to(dir.path());
        std::env::remove_var(PRIVATE_DEFAULTS_ENV);
        assert_eq!(delegate, None);
    }

    // ===== Cross-language conformance fixture (#4039 AC) =====
    //
    // The same fixture tree (`defaults/scripts/tests/fixtures/config_resolver/`,
    // relocated from `loom-tools/tests/fixtures/config_resolver/` when #4970
    // retired the Python `loom-tools` package) must resolve to the identical
    // effective config from Rust and Bash. See that directory's README.md for
    // what each tier exercises. Bash:
    // `defaults/scripts/tests/test-config-resolver.sh`.

    // ===== source_of / tier_paths_by_precedence (#4512) =====

    #[test]
    #[serial(loom_config_env)]
    fn test_tier_paths_are_ordered_highest_precedence_first() {
        std::env::set_var(PRIVATE_DEFAULTS_ENV, "");
        let root = tempdir().unwrap();
        let paths = tier_paths_by_precedence(root.path());
        std::env::remove_var(PRIVATE_DEFAULTS_ENV);
        // Reverse of the merge order in `resolve_effective_config`, so the first
        // tier that sets a key is the one that actually supplied it.
        assert_eq!(
            paths,
            vec![
                root.path().join(LOCAL_CONFIG_REL),
                root.path().join(PROJECT_CONFIG_REL),
                root.path().join(LEGACY_CONFIG_REL),
            ]
        );
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_source_of_names_the_legacy_tier_when_only_it_sets_the_key() {
        std::env::set_var(PRIVATE_DEFAULTS_ENV, "");
        let root = tempdir().unwrap();
        write(
            &root.path().join(LEGACY_CONFIG_REL),
            r#"{"autonomous": {"workFinder": {"maxConcurrent": 8}}}"#,
        );
        let src = source_of(root.path(), "autonomous.workFinder.maxConcurrent");
        std::env::remove_var(PRIVATE_DEFAULTS_ENV);
        assert_eq!(src, Some(root.path().join(LEGACY_CONFIG_REL)));
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_source_of_prefers_the_highest_precedence_tier_that_sets_the_key() {
        std::env::set_var(PRIVATE_DEFAULTS_ENV, "");
        let root = tempdir().unwrap();
        write(
            &root.path().join(LEGACY_CONFIG_REL),
            r#"{"autonomous": {"workFinder": {"maxConcurrent": 8}}}"#,
        );
        write(
            &root.path().join(LOCAL_CONFIG_REL),
            r#"{"autonomous": {"workFinder": {"maxConcurrent": 20}}}"#,
        );
        let src = source_of(root.path(), "autonomous.workFinder.maxConcurrent");
        std::env::remove_var(PRIVATE_DEFAULTS_ENV);
        assert_eq!(
            src,
            Some(root.path().join(LOCAL_CONFIG_REL)),
            "the host-local override is what dispatch actually sees"
        );
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_source_of_is_none_when_no_tier_sets_the_key_or_sets_it_null() {
        std::env::set_var(PRIVATE_DEFAULTS_ENV, "");
        let root = tempdir().unwrap();
        assert_eq!(source_of(root.path(), "autonomous.workFinder.maxConcurrent"), None);

        // An explicit `null` is "not set" — it must not be reported as a source.
        write(
            &root.path().join(LEGACY_CONFIG_REL),
            r#"{"autonomous": {"workFinder": {"maxConcurrent": null}}}"#,
        );
        let src = source_of(root.path(), "autonomous.workFinder.maxConcurrent");
        std::env::remove_var(PRIVATE_DEFAULTS_ENV);
        assert_eq!(src, None);
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_source_of_skips_a_malformed_tier_instead_of_erroring() {
        std::env::set_var(PRIVATE_DEFAULTS_ENV, "");
        let root = tempdir().unwrap();
        write(&root.path().join(LOCAL_CONFIG_REL), "{ this is not json");
        write(
            &root.path().join(LEGACY_CONFIG_REL),
            r#"{"autonomous": {"workFinder": {"maxConcurrent": 8}}}"#,
        );
        let src = source_of(root.path(), "autonomous.workFinder.maxConcurrent");
        std::env::remove_var(PRIVATE_DEFAULTS_ENV);
        assert_eq!(
            src,
            Some(root.path().join(LEGACY_CONFIG_REL)),
            "a malformed tier contributes nothing to the merge, so it cannot be the source"
        );
    }

    fn conformance_fixture_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("defaults")
            .join("scripts")
            .join("tests")
            .join("fixtures")
            .join("config_resolver")
    }

    #[test]
    #[serial(loom_config_env)]
    fn test_conformance_fixture_matches_expected_json() {
        std::env::set_var(PRIVATE_DEFAULTS_ENV, "");
        let fixture_dir = conformance_fixture_dir();

        let effective = resolve_effective_config(&fixture_dir);
        std::env::remove_var(PRIVATE_DEFAULTS_ENV);

        let expected_text = std::fs::read_to_string(fixture_dir.join("expected.json"))
            .expect("conformance fixture expected.json must exist and be readable");
        let expected: Value = serde_json::from_str(&expected_text)
            .expect("conformance fixture expected.json must be valid JSON");

        assert_eq!(
            effective, expected,
            "Rust resolver diverged from the cross-language conformance fixture's expected.json"
        );
    }
}
