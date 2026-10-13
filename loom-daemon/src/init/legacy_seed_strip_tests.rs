// Upgrade-time strip of legacy template-seeded fleet keys (#11451, slice 3 of
// #11441). Its own file rather than `tests.rs`: that module is over the
// `scripts/check-file-size-budget.sh` threshold and frozen at its recorded
// size, so additions go to a sibling module.
//
// Repos installed before #11445 still commit the exact fleet keys the old
// template seeded; the upgrade merge removes exactly those, so
// `validate --fleet-policy-only` reports only real per-repo overrides.

use super::tests::{config_log_lines, setup_config_merge_repo};
use super::*;
use tempfile::TempDir;

/// The exact fleet block the pre-#11445 template seeded into every new
/// `.loom/config.json` (recovered from `git show 0c1c33da2^:defaults/config.json`).
const LEGACY_SEED_CONFIG: &str = r#"{
  "version": "2",
  "offlineMode": false,
  "runtimes": {"default": "claude"},
  "safehouse": {
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
  }
}"#;

#[test]
fn test_upgrade_strips_exact_template_seed_fleet_keys_and_is_idempotent() {
    // 2AMLogic/marketing shape (inline fixture, no network): a consumer
    // config that is the old template seed verbatim. The upgrade must leave
    // no fleet-class key behind, name what it removed, and be byte-stable
    // on the second run.
    let temp = TempDir::new().unwrap();
    // The current template no longer seeds any fleet key.
    let template = r#"{"version": "2", "terminals": []}"#;
    let (workspace, defaults) = setup_config_merge_repo(&temp, template);
    let config_path = workspace.join(".loom").join("config.json");
    fs::create_dir_all(workspace.join(".loom")).unwrap();
    fs::write(&config_path, LEGACY_SEED_CONFIG).unwrap();

    let records = crate::test_log_capture::capture_logs(|| {
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
            .expect("upgrade init");
    });
    let merged: Value = serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    for key in ["offlineMode", "runtimes", "safehouse"] {
        assert!(merged.get(key).is_none(), "seeded `{key}` survived the upgrade: {merged}");
    }
    assert_eq!(merged["version"], "2");
    assert_eq!(merged["terminals"], Value::Array(vec![]));
    assert!(
        crate::fleet_policy::check(&workspace).is_empty(),
        "a fully seeded config must upgrade to zero fleet-policy findings"
    );
    let stripped: Vec<_> = config_log_lines(&records)
        .into_iter()
        .filter(|(_, m)| m.contains("stripped legacy template seed"))
        .collect();
    assert_eq!(stripped.len(), 1, "the strip must be logged exactly once: {stripped:?}");
    for key in ["offlineMode", "runtimes", "safehouse"] {
        assert!(stripped[0].1.contains(key), "stripped key `{key}` must be named: {stripped:?}");
    }

    // Second run: nothing left to strip — byte-identical output, no second
    // strip line.
    let after_first = fs::read_to_string(&config_path).unwrap();
    let records2 = crate::test_log_capture::capture_logs(|| {
        initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
            .expect("second upgrade init");
    });
    assert_eq!(
        after_first,
        fs::read_to_string(&config_path).unwrap(),
        "the upgrade must be byte-idempotent"
    );
    assert!(
        config_log_lines(&records2)
            .iter()
            .all(|(_, m)| !m.contains("stripped legacy template seed")),
        "second run must not strip again: {:?}",
        config_log_lines(&records2)
    );
}

#[test]
fn test_upgrade_keeps_modified_fleet_values_and_reports_them() {
    // 2AMLogic/gf180-sram shape (inline fixture, no network): the old seed
    // with operator modifications (`runtimes.default`, `safehouse.enabled`)
    // plus a genuine `autonomous.roleRunner.onIdle` override. Only the
    // untouched seed (`offlineMode`) is stripped; everything else survives
    // and validate reports it.
    let temp = TempDir::new().unwrap();
    let template = r#"{"version": "2", "terminals": []}"#;
    let (workspace, defaults) = setup_config_merge_repo(&temp, template);
    let config_path = workspace.join(".loom").join("config.json");
    fs::create_dir_all(workspace.join(".loom")).unwrap();
    // The old seed with two operator modifications plus a genuine override.
    fs::write(
        &config_path,
        r#"{
  "version": "2",
  "offlineMode": false,
  "runtimes": {"default": "codex"},
  "safehouse": {
    "enabled": true,
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
  },
  "autonomous": {"roleRunner": {"onIdle": ["auditor"]}}
}"#,
    )
    .unwrap();

    initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
        .expect("upgrade init");
    let merged: Value = serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert!(merged.get("offlineMode").is_none(), "untouched seed must be stripped: {merged}");
    assert_eq!(
        merged["runtimes"]["default"], "codex",
        "a modified runtimes value is operator content, not seed"
    );
    assert_eq!(merged["safehouse"]["enabled"], Value::Bool(true));
    assert_eq!(merged["autonomous"]["roleRunner"]["onIdle"], serde_json::json!(["auditor"]));

    let mut keys: Vec<String> = crate::fleet_policy::check(&workspace)
        .into_iter()
        .map(|f| f.key)
        .collect();
    keys.sort();
    // The retained `safehouse` block is no longer fleet policy: the
    // integration is retired (#11112) and the daemon ignores the block with
    // one boot-time warning, so `validate` does not flag it.
    assert_eq!(
        keys,
        ["autonomous", "runtimes"],
        "exactly the retained fleet content must be reported"
    );
}

#[test]
fn test_upgrade_does_not_strip_on_the_invalid_json_fallback_branch() {
    // The strip is a merge-branch behaviour: an unparseable consumer file
    // takes the loud fallback branch and must never quietly prune first.
    let temp = TempDir::new().unwrap();
    let template = r#"{"version": "2", "terminals": []}"#;
    let (workspace, defaults) = setup_config_merge_repo(&temp, template);
    fs::create_dir_all(workspace.join(".loom")).unwrap();
    let corrupt = r#"{"offlineMode": false, "runtimes""#;
    fs::write(workspace.join(".loom").join("config.json"), corrupt).unwrap();

    initialize_workspace(workspace.to_str().unwrap(), defaults.to_str().unwrap(), false)
        .expect("fallback init must not abort");
    let backup = fs::read_to_string(workspace.join(".loom").join("config.json.bak")).unwrap();
    assert_eq!(backup, corrupt, "the rescue copy must hold the original bytes");
    assert!(
        backup.contains("offlineMode"),
        "the fallback branch must not strip before saving: {backup}"
    );
}
