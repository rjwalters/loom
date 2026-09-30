//! Tests for the legacy Loom-shipped permission strip (issue #9447): the
//! malformed `Bash(./scripts/**:*)` / `Bash(./.loom/scripts/**:*)` rules an
//! earlier install wrote must be replaced on reinstall and removed on
//! uninstall, without touching user-authored rules.

use super::*;

#[test]
fn test_merge_settings_drops_legacy_loom_permissions() {
    // #9447: a consumer installed before the fix carries the malformed
    // `**:*` rules; a reinstall must replace them, not keep them alongside
    // their successors, and must leave user rules alone.
    let existing: serde_json::Value = serde_json::from_str(
        r#"{
            "permissions": {
                "allow": [
                    "Bash(./scripts/**:*)",
                    "Bash(make:*)",
                    "Bash(./.loom/scripts/**:*)"
                ]
            }
        }"#,
    )
    .unwrap();

    let loom_defaults: serde_json::Value = serde_json::from_str(
        r#"{
            "permissions": {
                "allow": ["Bash(./scripts/*)", "Bash(./.loom/scripts/*)"]
            }
        }"#,
    )
    .unwrap();

    let merged = merge_settings_json(&existing, &loom_defaults);
    let allow: Vec<&str> = merged["permissions"]["allow"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();

    assert!(allow.contains(&"Bash(./scripts/*)"));
    assert!(allow.contains(&"Bash(./.loom/scripts/*)"));
    assert!(allow.contains(&"Bash(make:*)"));
    assert!(!allow.contains(&"Bash(./scripts/**:*)"));
    assert!(!allow.contains(&"Bash(./.loom/scripts/**:*)"));
    assert_eq!(allow.len(), 3);
}

#[test]
fn test_remove_loom_permissions_strips_legacy_rules() {
    // #9447: uninstall must strip the legacy Loom-shipped rules even though
    // the current defaults no longer list them, and keep user rules.
    let mut settings: serde_json::Value = serde_json::from_str(
        r#"{
            "permissions": {
                "allow": [
                    "Bash(./scripts/*)",
                    "Bash(./.loom/scripts/*)",
                    "Bash(./scripts/**:*)",
                    "Bash(./.loom/scripts/**:*)",
                    "Bash(make:*)"
                ]
            }
        }"#,
    )
    .unwrap();

    let loom_defaults: serde_json::Value = serde_json::from_str(
        r#"{
            "permissions": {
                "allow": ["Bash(./scripts/*)", "Bash(./.loom/scripts/*)"]
            }
        }"#,
    )
    .unwrap();

    remove_loom_permissions(&mut settings, &loom_defaults);

    let allow = settings["permissions"]["allow"].as_array().unwrap();
    assert_eq!(allow, &vec![serde_json::json!("Bash(make:*)")]);
}
