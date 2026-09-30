use serde_json::{json, Value};

use super::*;
use crate::fleet_store::render::{self, Tier};
use crate::fleet_store::test_support::{sample_files, snapshot_of};

struct Paths {
    _dir: tempfile::TempDir,
    machine: std::path::PathBuf,
    local: std::path::PathBuf,
}

fn paths() -> Paths {
    let dir = tempfile::tempdir().unwrap();
    let machine = dir.path().join("share/loom/config/defaults.json");
    let local = dir.path().join("ws/.loom-local/local.json");
    Paths {
        _dir: dir,
        machine,
        local,
    }
}

fn write_json(path: &std::path::Path, v: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec_pretty(v).unwrap()).unwrap();
}

#[test]
fn no_drift_at_all_plans_nothing() {
    let p = paths();
    let snapshot = snapshot_of(&sample_files());
    // build-2 has no store-side local.json, and no on-disk local file either.
    let rendered = render::render(&snapshot, "build-2", &p.machine, &p.local).unwrap();
    write_json(&p.machine, &rendered[0].value);
    let adopted = plan(&snapshot, "build-2", &p.machine, &p.local).unwrap();
    assert!(adopted.is_empty(), "{adopted:?}");
}

#[test]
fn a_changed_leaf_and_a_brand_new_key_patch_the_hosts_overlay_only() {
    let p = paths();
    let snapshot = snapshot_of(&sample_files());
    let rendered = render::render(&snapshot, "build-1", &p.machine, &p.local).unwrap();
    let machine = &rendered
        .iter()
        .find(|t| t.tier == Tier::Machine)
        .unwrap()
        .value;

    let mut on_disk = machine.clone();
    on_disk["autonomous"]["workFinder"]["maxConcurrent"] = json!(99);
    on_disk["custom"] = json!({"x": 1});
    write_json(&p.machine, &on_disk);
    // Keep the local tier in sync so only the machine tier drifts.
    let local = &rendered
        .iter()
        .find(|t| t.tier == Tier::Local)
        .unwrap()
        .value;
    write_json(&p.local, local);

    let adopted = plan(&snapshot, "build-1", &p.machine, &p.local).unwrap();
    assert_eq!(adopted.len(), 1);
    let f = &adopted[0];
    assert_eq!(f.path, "fleet/hosts/build-1/defaults.json");
    assert!(f.before.is_some());

    let overlay: Value = serde_json::from_str(&f.after).unwrap();
    assert_eq!(overlay["autonomous"]["workFinder"]["maxConcurrent"], json!(99));
    assert_eq!(overlay["custom"], json!({"x": 1}));
    // Untouched overlay keys survive the patch.
    assert_eq!(overlay["autonomous"]["roleRunner"]["roles"], json!(["judge", "doctor"]));
    assert_eq!(overlay["forge"]["githubApp"]["privateKeyPath"], json!("/k.pem"));
}

#[test]
fn drifted_local_tier_is_adopted_verbatim() {
    let p = paths();
    let snapshot = snapshot_of(&sample_files());
    let rendered = render::render(&snapshot, "build-1", &p.machine, &p.local).unwrap();
    write_json(
        &p.machine,
        &rendered
            .iter()
            .find(|t| t.tier == Tier::Machine)
            .unwrap()
            .value,
    );
    let on_disk_local = json!({"observability": {"enabled": false}, "extra": 1});
    write_json(&p.local, &on_disk_local);

    let adopted = plan(&snapshot, "build-1", &p.machine, &p.local).unwrap();
    assert_eq!(adopted.len(), 1);
    assert_eq!(adopted[0].path, "fleet/hosts/build-1/local.json");
    assert!(adopted[0].before.is_some(), "store already has a local.json for build-1");
    let after: Value = serde_json::from_str(&adopted[0].after).unwrap();
    assert_eq!(after, on_disk_local);
}

#[test]
fn a_host_local_override_the_store_has_never_had_is_proposed_as_a_new_file() {
    let p = paths();
    let snapshot = snapshot_of(&sample_files());
    // build-2 has no store-side local.json at all.
    let rendered = render::render(&snapshot, "build-2", &p.machine, &p.local).unwrap();
    write_json(&p.machine, &rendered[0].value);
    let on_disk_local = json!({"observability": {"enabled": true}});
    write_json(&p.local, &on_disk_local);

    let adopted = plan(&snapshot, "build-2", &p.machine, &p.local).unwrap();
    assert_eq!(adopted.len(), 1);
    assert_eq!(adopted[0].path, "fleet/hosts/build-2/local.json");
    assert_eq!(adopted[0].before, None, "the store has no local.json for build-2 yet");
    let after: Value = serde_json::from_str(&adopted[0].after).unwrap();
    assert_eq!(after, on_disk_local);
}

#[test]
fn a_missing_on_disk_local_file_is_never_proposed() {
    let p = paths();
    let snapshot = snapshot_of(&sample_files());
    let rendered = render::render(&snapshot, "build-1", &p.machine, &p.local).unwrap();
    write_json(
        &p.machine,
        &rendered
            .iter()
            .find(|t| t.tier == Tier::Machine)
            .unwrap()
            .value,
    );
    // No on-disk local file at all (p.local was never written).
    let adopted = plan(&snapshot, "build-1", &p.machine, &p.local).unwrap();
    assert!(adopted.is_empty(), "{adopted:?}");
}

#[test]
fn collect_and_apply_round_trip_a_removed_key_too() {
    let current = json!({"a": 1});
    let wanted = json!({"a": 1, "b": 2});
    let mut ops = Vec::new();
    collect(&mut Vec::new(), &current, &wanted, &mut ops);
    let mut overlay = json!({"b": 2, "c": 3});
    apply(&mut overlay, &ops);
    assert_eq!(overlay, json!({"c": 3}));
}
