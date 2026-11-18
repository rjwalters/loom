//! Tests for the attended-subagent per-role model chain (issue #13956).

use super::*;
use serde_json::json;
use std::fs;

fn config_with_terminals(terminals: Value) -> Value {
    json!({ "terminals": terminals })
}

fn terminal(role_file: &str, model: Option<&str>) -> Value {
    let mut role_config = json!({ "roleFile": role_file });
    if let Some(m) = model {
        role_config["model"] = json!(m);
    }
    json!({ "id": "t", "roleConfig": role_config })
}

fn write_role_json(root: &std::path::Path, role: &str, suggested: Option<&str>) {
    let dir = root.join(".loom").join("roles");
    fs::create_dir_all(&dir).unwrap();
    let mut body = json!({ "name": "test role" });
    if let Some(m) = suggested {
        body["suggestedModel"] = json!(m);
    }
    fs::write(dir.join(format!("{role}.json")), body.to_string()).unwrap();
}

// ---------------------------------------------------------------------
// Tier 1: explicit override
// ---------------------------------------------------------------------

#[test]
fn explicit_param_wins_over_every_config_tier() {
    let dir = std::env::temp_dir().join(format!("rrm-t1-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    write_role_json(&dir, "judge", Some("opus"));
    let cfg = config_with_terminals(json!([terminal("judge.md", Some("sonnet"))]));

    let resolved = resolve_role_model(&cfg, &dir, "judge", Some("claude-opus-5"));
    assert_eq!(resolved.model.as_deref(), Some("claude-opus-5"));
    assert_eq!(resolved.source, SOURCE_PARAM);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn blank_explicit_param_is_unset() {
    // The same blank-means-unset contract `resolve_dispatch_model` applies.
    let resolved =
        resolve_role_model(&json!({}), std::path::Path::new("/nonexistent"), "judge", Some("   "));
    assert_eq!(resolved.source, SOURCE_INHERIT);
    assert_eq!(resolved.model, None);
}

// ---------------------------------------------------------------------
// Tier 2: terminals[].roleConfig.model
// ---------------------------------------------------------------------

#[test]
fn role_config_model_is_matched_by_rolefile_with_md_stripped() {
    let cfg = config_with_terminals(json!([
        terminal("curator.md", Some("haiku")),
        terminal("judge.md", Some("opus")),
    ]));
    let resolved = resolve_role_model(&cfg, std::path::Path::new("/nonexistent"), "judge", None);
    assert_eq!(resolved.model.as_deref(), Some("opus"));
    assert_eq!(resolved.source, SOURCE_ROLE_CONFIG);
}

#[test]
fn role_matching_is_case_and_suffix_insensitive() {
    let cfg = config_with_terminals(json!([terminal("Builder.md", Some("sonnet"))]));
    for asked in ["builder", "Builder", "BUILDER", "builder.md", " builder "] {
        let resolved = resolve_role_model(&cfg, std::path::Path::new("/nonexistent"), asked, None);
        assert_eq!(resolved.model.as_deref(), Some("sonnet"), "role {asked:?}");
        assert_eq!(resolved.source, SOURCE_ROLE_CONFIG, "role {asked:?}");
    }
}

#[test]
fn first_rolefile_match_decides() {
    let cfg = config_with_terminals(json!([
        terminal("judge.md", Some("sonnet")),
        terminal("judge.md", Some("opus")),
    ]));
    let resolved = resolve_role_model(&cfg, std::path::Path::new("/nonexistent"), "judge", None);
    assert_eq!(resolved.model.as_deref(), Some("sonnet"));
}

#[test]
fn blank_role_config_model_falls_through_to_suggested() {
    // The prose chain's "resolves to an empty string" clause: a blank tier-2
    // pin is not a pin.
    let dir = std::env::temp_dir().join(format!("rrm-blank2-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    write_role_json(&dir, "judge", Some("opus"));
    let cfg = config_with_terminals(json!([terminal("judge.md", Some("   "))]));

    let resolved = resolve_role_model(&cfg, &dir, "judge", None);
    assert_eq!(resolved.model.as_deref(), Some("opus"));
    assert_eq!(resolved.source, SOURCE_SUGGESTED);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn absent_model_on_matching_terminal_falls_through() {
    let dir = std::env::temp_dir().join(format!("rrm-absent2-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    write_role_json(&dir, "judge", Some("haiku"));
    let cfg = config_with_terminals(json!([terminal("judge.md", None)]));

    let resolved = resolve_role_model(&cfg, &dir, "judge", None);
    assert_eq!(resolved.model.as_deref(), Some("haiku"));
    assert_eq!(resolved.source, SOURCE_SUGGESTED);
    fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------
// Tier 3: .loom/roles/<role>.json suggestedModel
// ---------------------------------------------------------------------

#[test]
fn suggested_model_is_read_from_the_roles_json() {
    let dir = std::env::temp_dir().join(format!("rrm-t3-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    write_role_json(&dir, "builder", Some("opus"));

    let resolved = resolve_role_model(&json!({}), &dir, "builder", None);
    assert_eq!(resolved.model.as_deref(), Some("opus"));
    assert_eq!(resolved.source, SOURCE_SUGGESTED);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn missing_role_json_inherits() {
    let resolved =
        resolve_role_model(&json!({}), std::path::Path::new("/nonexistent"), "judge", None);
    assert_eq!(resolved.model, None);
    assert_eq!(resolved.source, SOURCE_INHERIT);
}

#[test]
fn malformed_role_json_soft_fails_to_inherit() {
    let dir = std::env::temp_dir().join(format!("rrm-malformed-{}", std::process::id()));
    let roles = dir.join(".loom").join("roles");
    fs::create_dir_all(&roles).unwrap();
    fs::write(roles.join("judge.json"), "{not json").unwrap();

    let resolved = resolve_role_model(&json!({}), &dir, "judge", None);
    assert_eq!(resolved.model, None);
    assert_eq!(resolved.source, SOURCE_INHERIT);
    fs::remove_dir_all(&dir).ok();
}

#[test]
fn blank_suggested_model_is_unset() {
    let dir = std::env::temp_dir().join(format!("rrm-blank3-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    write_role_json(&dir, "judge", Some("  "));

    let resolved = resolve_role_model(&json!({}), &dir, "judge", None);
    assert_eq!(resolved.model, None);
    assert_eq!(resolved.source, SOURCE_INHERIT);
    fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------
// Soft-fail / malformed config
// ---------------------------------------------------------------------

#[test]
fn malformed_config_shapes_soft_fail_to_inherit() {
    // terminals not an array; entries without roleConfig; roleConfig without
    // roleFile; non-string model — every deviation skips, never errors.
    for cfg in [
        json!({ "terminals": "not-an-array" }),
        json!({ "terminals": [{ "id": "t" }] }),
        json!({ "terminals": [{ "roleConfig": { "model": "opus" } }] }),
        json!({ "terminals": [{ "roleConfig": { "roleFile": "judge.md", "model": 7 } }] }),
    ] {
        let resolved =
            resolve_role_model(&cfg, std::path::Path::new("/nonexistent"), "judge", None);
        assert_eq!(resolved.model, None, "config {cfg}");
        assert_eq!(resolved.source, SOURCE_INHERIT, "config {cfg}");
    }
}

// ---------------------------------------------------------------------
// Effective-config tiers (the #13956 AC4 shape: the setting lives outside
// the resynced surfaces, so `.loom-local/local.json` governs)
// ---------------------------------------------------------------------

#[test]
fn local_json_override_governs_over_committed_config() {
    // resolve_role_model consumes an ALREADY-RESOLVED effective config; this
    // pins that the deep_merge order (committed config lost to local.json)
    // flows straight through: the same Value walk, one winner.
    let committed = config_with_terminals(json!([terminal("judge.md", Some("sonnet"))]));
    let local = config_with_terminals(json!([terminal("judge.md", Some("opus"))]));
    let merged = crate::config_resolver::deep_merge(&committed, &local);

    let resolved = resolve_role_model(&merged, std::path::Path::new("/nonexistent"), "judge", None);
    assert_eq!(resolved.model.as_deref(), Some("opus"));
    assert_eq!(resolved.source, SOURCE_ROLE_CONFIG);
}

// ---------------------------------------------------------------------
// Role normalization edge cases
// ---------------------------------------------------------------------

#[test]
fn empty_or_whitespace_role_inherits() {
    for role in ["", "   ", ".md"] {
        let resolved =
            resolve_role_model(&json!({}), std::path::Path::new("/nonexistent"), role, None);
        assert_eq!(resolved.model, None, "role {role:?}");
        assert_eq!(resolved.source, SOURCE_INHERIT, "role {role:?}");
    }
}

#[test]
fn normalize_role_strips_md_and_folds_case() {
    assert_eq!(normalize_role("judge"), "judge");
    assert_eq!(normalize_role("Judge.md"), "judge");
    assert_eq!(normalize_role("  BUILDER  "), "builder");
    assert_eq!(normalize_role("judge.md.md"), "judge"); // trim_end_matches strips repeats
}
