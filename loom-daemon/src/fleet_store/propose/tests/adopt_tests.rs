use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::*;
use crate::fleet_store::render::{self, Tier};
use crate::fleet_store::test_support::snapshot_of;

/// A store's `fleet.yml`: comments in its config, which must survive.
const FLEET_YML: &str = "\
# the fleet model
root: /srv
repos: []
state:
  fleet:
    state: running
config:
  defaults:
    autonomous:
      roleRunner:
        enabled: true
        roles:
          - judge
      autoUpdate:
        settleSecs: 3600
    forge:
      githubApp:
        appId: \"1\"
  hosts:
    build-1:
      defaults:
        # build-1 runs more
        autonomous:
          workFinder:
            maxConcurrent: 8
          roleRunner:
            roles: [judge, doctor]
        forge:
          githubApp:
            privateKeyPath: /k.pem
      local:
        observability:
          enabled: true
    build-2:
      defaults:
        autonomous:
          workFinder:
            maxConcurrent: 2
";

/// `text` as the store's compiled `fleet.json` would carry it.
fn compiled(text: &str) -> Value {
    let mut doc = crate::fleet_store::yaml::parse(text).unwrap();
    doc["_generated"] = json!({"schema_version": 1});
    doc
}

/// A snapshot of a store whose `fleet.json` is `FLEET_YML` compiled.
fn store() -> crate::fleet_store::fetch::Snapshot {
    let mut files = BTreeMap::new();
    files.insert(crate::fleet_store::FLEET_JSON_PATH.to_string(), compiled(FLEET_YML).to_string());
    snapshot_of(&files)
}

/// `host`'s `tier` as `text` (an edited `fleet.yml`) says it.
fn tier(text: &str, host: &str, tier: &str) -> Value {
    compiled(text)["config"]["hosts"][host][tier].clone()
}

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

/// The store's render for `host`: (machine tier, local tier if any).
fn rendered(p: &Paths, host: &str) -> (Value, Option<Value>) {
    let targets = render::render(&store(), host, &p.machine, &p.local).unwrap();
    let pick = |t: Tier| {
        targets
            .iter()
            .find(|x| x.tier == t)
            .map(|x| x.value.clone())
    };
    (pick(Tier::Machine).unwrap(), pick(Tier::Local))
}

#[test]
fn no_drift_at_all_plans_nothing() {
    let p = paths();
    let (machine, _) = rendered(&p, "build-2");
    write_json(&p.machine, &machine);
    let planned = plan(&store(), FLEET_YML, "build-2", &p.machine, &p.local).unwrap();
    assert_eq!(planned, None);
}

#[test]
fn a_changed_leaf_and_a_brand_new_key_patch_the_hosts_overlay_only() {
    let p = paths();
    let (machine, local) = rendered(&p, "build-1");
    let mut on_disk = machine.clone();
    on_disk["autonomous"]["workFinder"]["maxConcurrent"] = json!(99);
    on_disk["custom"] = json!({"x": 1});
    write_json(&p.machine, &on_disk);
    write_json(&p.local, &local.unwrap());

    let after = plan(&store(), FLEET_YML, "build-1", &p.machine, &p.local)
        .unwrap()
        .expect("drift to adopt");
    let overlay = tier(&after, "build-1", "defaults");
    assert_eq!(overlay["autonomous"]["workFinder"]["maxConcurrent"], json!(99));
    assert_eq!(overlay["custom"], json!({"x": 1}));
    // Untouched overlay keys, the fleet defaults and the comments survive.
    assert_eq!(overlay["autonomous"]["roleRunner"]["roles"], json!(["judge", "doctor"]));
    assert_eq!(overlay["forge"]["githubApp"]["privateKeyPath"], json!("/k.pem"));
    assert_eq!(
        compiled(&after)["config"]["defaults"],
        compiled(FLEET_YML)["config"]["defaults"]
    );
    assert!(after.contains("        # build-1 runs more\n"));
    assert!(after.contains("            maxConcurrent: 99\n"));
    // Only those lines changed.
    let changed = after.lines().count() - FLEET_YML.lines().count();
    assert_eq!(changed, 2, "{after}");
}

#[test]
fn a_key_dropped_on_disk_is_removed_from_the_overlay() {
    let p = paths();
    let (machine, local) = rendered(&p, "build-1");
    let mut on_disk = machine.clone();
    on_disk["forge"]["githubApp"]
        .as_object_mut()
        .unwrap()
        .remove("privateKeyPath");
    write_json(&p.machine, &on_disk);
    write_json(&p.local, &local.unwrap());
    let after = plan(&store(), FLEET_YML, "build-1", &p.machine, &p.local)
        .unwrap()
        .unwrap();
    assert!(!after.contains("/k.pem"));
    assert_eq!(tier(&after, "build-1", "defaults")["forge"]["githubApp"], json!({}));
}

#[test]
fn drifted_local_tier_is_made_equal_to_the_file() {
    let p = paths();
    let (machine, _) = rendered(&p, "build-1");
    write_json(&p.machine, &machine);
    let on_disk_local = json!({"observability": {"enabled": false}, "extra": 1});
    write_json(&p.local, &on_disk_local);

    let after = plan(&store(), FLEET_YML, "build-1", &p.machine, &p.local)
        .unwrap()
        .unwrap();
    assert_eq!(tier(&after, "build-1", "local"), on_disk_local);
    assert_eq!(tier(&after, "build-1", "defaults"), tier(FLEET_YML, "build-1", "defaults"));
}

#[test]
fn a_host_local_override_the_store_has_never_had_is_added() {
    let p = paths();
    let (machine, local) = rendered(&p, "build-2");
    assert_eq!(local, None, "build-2 has no local tier in the store");
    write_json(&p.machine, &machine);
    let on_disk_local = json!({"observability": {"enabled": true}});
    write_json(&p.local, &on_disk_local);

    let after = plan(&store(), FLEET_YML, "build-2", &p.machine, &p.local)
        .unwrap()
        .unwrap();
    assert_eq!(tier(&after, "build-2", "local"), on_disk_local);
}

#[test]
fn a_missing_on_disk_local_file_is_never_proposed() {
    let p = paths();
    let (machine, _) = rendered(&p, "build-1");
    write_json(&p.machine, &machine);
    let planned = plan(&store(), FLEET_YML, "build-1", &p.machine, &p.local).unwrap();
    assert_eq!(planned, None);
}

#[test]
fn a_store_without_fleet_json_is_refused() {
    let p = paths();
    let snapshot = snapshot_of(&crate::fleet_store::test_support::sample_files());
    let err = plan(&snapshot, FLEET_YML, "build-1", &p.machine, &p.local).unwrap_err();
    assert!(err.to_string().contains("has no fleet.json"), "{err}");
}
