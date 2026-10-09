use super::*;

const GOOD: &str = r#"// header comment
{
  /* block
     comment */
  "extends": ["config:recommended"],
  "dependencyDashboard": false,
  "labels": ["dependencies", "loom:review-requested"], // trailing comment
  "lockFileMaintenance": { "enabled": true, },
  "packageRules": [
    {
      "matchManagers": ["github-actions"],
      // a superset that keeps the routing label is fine
      "labels": ["dependencies", "loom:review-requested", "major"],
    },
  ],
  "vulnerabilityAlerts": { "enabled": true },
}
"#;

fn locations(text: &str) -> Vec<String> {
    check_text("renovate.json5", text)
        .into_iter()
        .map(|v| v.location)
        .collect()
}

#[test]
fn a_correct_config_passes() {
    assert!(locations(GOOD).is_empty(), "{:?}", check_text("x", GOOD));
}

#[test]
fn missing_top_level_labels_fails() {
    let text = r#"{ "extends": ["config:recommended"] }"#;
    assert_eq!(locations(text), vec!["labels"]);
}

#[test]
fn top_level_labels_without_the_routing_label_fails() {
    let text = GOOD.replace(
        r#"["dependencies", "loom:review-requested"], // trailing"#,
        r#"["dependencies"], // trailing"#,
    );
    assert_eq!(locations(&text), vec!["labels"]);
}

#[test]
fn a_package_rule_overriding_labels_without_it_fails() {
    let text =
        GOOD.replace(r#"["dependencies", "loom:review-requested", "major"]"#, r#"["major"]"#);
    assert_eq!(locations(&text), vec!["packageRules[0].labels"]);
}

#[test]
fn vulnerability_alerts_and_lock_file_maintenance_overrides_are_checked() {
    let text = GOOD
        .replace(
            r#""vulnerabilityAlerts": { "enabled": true }"#,
            r#""vulnerabilityAlerts": { "enabled": true, "labels": ["security"] }"#,
        )
        .replace(
            r#""lockFileMaintenance": { "enabled": true, }"#,
            r#""lockFileMaintenance": { "enabled": true, labels: ['lockfile'] }"#,
        );
    let mut got = locations(&text);
    got.sort();
    assert_eq!(got, vec!["lockFileMaintenance.labels", "vulnerabilityAlerts.labels"]);
}

#[test]
fn add_labels_is_additive_and_not_checked() {
    let text = GOOD.replace(
        r#""vulnerabilityAlerts": { "enabled": true }"#,
        r#""vulnerabilityAlerts": { "enabled": true, "addLabels": ["security"] }"#,
    );
    assert!(locations(&text).is_empty());
}

#[test]
fn an_unparseable_config_fails_loudly() {
    assert_eq!(locations("{ \"labels\": [ "), vec!["(file)"]);
}

#[test]
fn json5_reader_handles_the_subset() {
    let json =
        json5_to_json("{ key: 'it\\'s \"q\"', // c\n url: \"https://x/y\", n: [1, 2,], }").unwrap();
    let v: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["key"], "it's \"q\"");
    assert_eq!(v["url"], "https://x/y");
    assert_eq!(v["n"], serde_json::json!([1, 2]));
}

#[test]
fn no_config_is_a_no_op_and_the_file_is_found_when_present() {
    let dir = tempfile::tempdir().unwrap();
    let report = check(dir.path());
    assert!(report.checked.is_empty() && report.violations.is_empty());

    std::fs::create_dir_all(dir.path().join(".github")).unwrap();
    std::fs::write(dir.path().join(".github/renovate.json"), r#"{"labels": []}"#).unwrap();
    let report = check(dir.path());
    assert_eq!(report.checked, vec![".github/renovate.json"]);
    assert_eq!(report.violations.len(), 1);
}

/// The regression guard: this repository's own `renovate.json5` passes.
#[test]
fn this_repositorys_renovate_config_passes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let report = check(&root);
    assert!(
        report.checked.iter().any(|f| f == "renovate.json5"),
        "expected the repo's renovate.json5 to be found: {:?}",
        report.checked
    );
    assert!(report.violations.is_empty(), "{:?}", report.violations);
}

/// Finding 1 of #9418: `config:recommended` extends `:dependencyDashboard`,
/// whose dashboard issue carries no `loom:` label and would be swept into
/// Curator's unlabeled-issue intake. The repo config must switch it off.
#[test]
fn this_repositorys_renovate_config_disables_the_dependency_dashboard() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../renovate.json5");
    let text = std::fs::read_to_string(path).unwrap();
    let v: Value = serde_json::from_str(&json5_to_json(&text).unwrap()).unwrap();
    assert_eq!(v["dependencyDashboard"], Value::Bool(false));
}
