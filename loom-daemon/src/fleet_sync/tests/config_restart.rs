//! Tests for the since-boot restart-required demand (#12424). Pure: tier files
//! are temp files and `compare` is called directly, so no process-wide cell,
//! marker or `~/.loom` is touched.

use std::path::Path;

use chrono::{Duration, Utc};
use serde_json::{json, Value};

use super::*;

/// A restart-required key and a live one (see `fleet_store::reload::TABLE`).
const RESTART: &str = "autonomous.workFinder.enabled";
const LIVE: &str = "autonomous.roleRunner.enabled";

fn files(dir: &Path) -> TierFiles {
    TierFiles {
        files: vec![
            (Tier::Machine, dir.join("defaults.json")),
            (Tier::Local, dir.join("local.json")),
        ],
    }
}

fn put(dir: &Path, name: &str, value: &Value) {
    std::fs::write(dir.join(name), serde_json::to_vec(value).unwrap()).unwrap();
}

fn cfg(restart: bool, live: bool) -> Value {
    json!({"autonomous": {"workFinder": {"enabled": restart}, "roleRunner": {"enabled": live}}})
}

#[test]
fn no_change_is_no_demand() {
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), "defaults.json", &cfg(false, false));
    put(dir.path(), "local.json", &cfg(false, false));
    let boot = BootSnapshot::take(files(dir.path()));
    assert_eq!(compare(&boot, None, Utc::now()), None);
}

#[test]
fn restart_required_change_is_a_demand_of_paths() {
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), "defaults.json", &cfg(false, false));
    put(dir.path(), "local.json", &cfg(false, false));
    let boot = BootSnapshot::take(files(dir.path()));
    put(dir.path(), "defaults.json", &cfg(true, false));
    let demand = compare(&boot, None, Utc::now()).unwrap();
    assert_eq!(demand.paths, vec![RESTART.to_string()]);
}

#[test]
fn live_only_change_is_no_demand() {
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), "defaults.json", &cfg(false, false));
    put(dir.path(), "local.json", &cfg(false, false));
    let boot = BootSnapshot::take(files(dir.path()));
    put(dir.path(), "defaults.json", &cfg(false, true));
    assert_eq!(compare(&boot, None, Utc::now()), None, "{LIVE} reloads live");
}

#[test]
fn change_then_revert_clears_the_demand() {
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), "defaults.json", &cfg(false, false));
    put(dir.path(), "local.json", &cfg(false, false));
    let boot = BootSnapshot::take(files(dir.path()));
    put(dir.path(), "defaults.json", &cfg(true, false));
    let first = compare(&boot, None, Utc::now());
    assert!(first.is_some());
    put(dir.path(), "defaults.json", &cfg(false, false));
    assert_eq!(compare(&boot, first.as_ref(), Utc::now()), None);
}

#[test]
fn file_missing_at_boot_then_present_is_whole_file() {
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), "local.json", &cfg(false, false));
    let boot = BootSnapshot::take(files(dir.path()));
    put(dir.path(), "defaults.json", &cfg(false, false));
    let demand = compare(&boot, None, Utc::now()).unwrap();
    assert_eq!(demand.paths, vec!["machine tier (whole file)".to_string()]);
}

#[test]
fn file_becoming_unparseable_is_whole_file() {
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), "defaults.json", &cfg(false, false));
    put(dir.path(), "local.json", &cfg(false, false));
    let boot = BootSnapshot::take(files(dir.path()));
    std::fs::write(dir.path().join("local.json"), "{ not json").unwrap();
    let demand = compare(&boot, None, Utc::now()).unwrap();
    assert_eq!(demand.paths, vec!["host-local tier (whole file)".to_string()]);
}

#[test]
fn both_tiers_changed_is_one_demand() {
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), "defaults.json", &cfg(false, false));
    put(dir.path(), "local.json", &cfg(false, false));
    let boot = BootSnapshot::take(files(dir.path()));
    put(dir.path(), "defaults.json", &cfg(true, false));
    put(dir.path(), "local.json", &cfg(true, false));
    let demand = compare(&boot, None, Utc::now()).unwrap();
    assert_eq!(demand.paths, vec![RESTART.to_string()], "same path de-duplicated");
}

#[test]
fn fingerprint_moves_on_a_further_value_change_and_first_seen_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), "defaults.json", &json!({"fleet": {"autoApply": false}}));
    put(dir.path(), "local.json", &json!({}));
    let boot = BootSnapshot::take(files(dir.path()));
    let t0 = Utc::now();
    put(dir.path(), "defaults.json", &json!({"fleet": {"autoApply": true}}));
    let a = compare(&boot, None, t0).unwrap();
    let again = compare(&boot, Some(&a), t0 + Duration::seconds(60)).unwrap();
    assert_eq!(again.fingerprint, a.fingerprint);
    put(dir.path(), "defaults.json", &json!({"fleet": {"autoApply": "other"}}));
    let b = compare(&boot, Some(&a), t0 + Duration::seconds(120)).unwrap();
    assert_ne!(b.fingerprint, a.fingerprint);
    assert_eq!(b.paths, a.paths);
    assert_eq!(b.first_seen, t0, "one demand across passes");
}

#[test]
fn demand_carries_paths_only_no_values() {
    let dir = tempfile::tempdir().unwrap();
    put(dir.path(), "defaults.json", &json!({"fleet": {"repo": "a/b"}}));
    put(dir.path(), "local.json", &json!({}));
    let boot = BootSnapshot::take(files(dir.path()));
    put(dir.path(), "defaults.json", &json!({"fleet": {"repo": "SECRET-VALUE"}}));
    let demand = compare(&boot, None, Utc::now()).unwrap();
    let shown = format!("{demand:?}");
    assert!(!shown.contains("SECRET-VALUE") && !shown.contains("a/b"), "{shown}");
    assert_eq!(demand.paths, vec!["fleet.repo".to_string()]);
}
