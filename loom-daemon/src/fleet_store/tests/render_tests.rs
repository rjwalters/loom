use serde_json::json;

use super::*;
use crate::fleet_store::test_support::{sample_files, snapshot_of};

struct Paths {
    _dir: tempfile::TempDir,
    machine: PathBuf,
    local: PathBuf,
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

#[test]
fn machine_tier_is_deep_merge_of_fleet_defaults_and_host_overlay() {
    let p = paths();
    let targets = render(&snapshot_of(&sample_files()), "build-1", &p.machine, &p.local).unwrap();
    assert_eq!(targets.len(), 2);
    let machine = &targets[0];
    assert_eq!(machine.tier, Tier::Machine);
    assert_eq!(
        machine.value,
        json!({
            "autonomous": {
                // Objects merge key by key…
                "roleRunner": {"enabled": true, "roles": ["judge", "doctor"]},
                "autoUpdate": {"settleSecs": 3600},
                "workFinder": {"maxConcurrent": 8}
            },
            "forge": {"githubApp": {"appId": "1", "privateKeyPath": "/k.pem"}}
        }),
        // …and arrays replace, exactly as config_resolver::deep_merge does.
    );
    let files = sample_files();
    let base: Value = serde_json::from_str(&files["fleet/defaults.json"]).unwrap();
    let overlay: Value = serde_json::from_str(&files["fleet/hosts/build-1/defaults.json"]).unwrap();
    assert_eq!(machine.value, crate::config_resolver::deep_merge(&base, &overlay));
    assert_eq!(targets[1].tier, Tier::Local);
    assert_eq!(targets[1].value, json!({"observability": {"enabled": true}}));
}

#[test]
fn render_write_then_check_roundtrips_to_in_sync() {
    let p = paths();
    let targets = render(&snapshot_of(&sample_files()), "build-1", &p.machine, &p.local).unwrap();
    let drifts: Vec<Drift> = targets.iter().map(drift).collect();
    assert_eq!(drifts, vec![Drift::Missing, Drift::Missing]);
    assert_eq!(check_exit_code(&drifts), 1);
    for t in &targets {
        assert_eq!(write(t, "20260101T000000Z").unwrap(), (true, None));
    }
    // What was written reads back through the resolver's own tier reader.
    let back = crate::config_resolver::soft_read_json_object(&p.machine);
    assert_eq!(back, targets[0].value);
    let drifts: Vec<Drift> = targets.iter().map(drift).collect();
    assert_eq!(drifts, vec![Drift::InSync, Drift::InSync]);
    assert_eq!(check_exit_code(&drifts), 0);
    // A second render is a no-op: nothing written, no backup made.
    assert_eq!(write(&targets[0], "20260101T000001Z").unwrap(), (false, None));
}

#[test]
fn drift_is_semantic_not_textual() {
    let p = paths();
    let targets = render(&snapshot_of(&sample_files()), "build-1", &p.machine, &p.local).unwrap();
    let compact = serde_json::to_string(&targets[1].value).unwrap();
    std::fs::create_dir_all(p.local.parent().unwrap()).unwrap();
    std::fs::write(&p.local, compact).unwrap();
    assert_eq!(drift(&targets[1]), Drift::InSync);
}

#[test]
fn check_reports_each_differing_path() {
    let p = paths();
    let targets = render(&snapshot_of(&sample_files()), "build-1", &p.machine, &p.local).unwrap();
    let mut on_disk = targets[0].value.clone();
    on_disk["autonomous"]["workFinder"]["maxConcurrent"] = json!(12);
    on_disk["autonomous"]["autoUpdate"] = json!(null);
    on_disk["handEdited"] = json!(true);
    on_disk["forge"]["githubApp"]
        .as_object_mut()
        .unwrap()
        .remove("appId");
    std::fs::create_dir_all(p.machine.parent().unwrap()).unwrap();
    std::fs::write(&p.machine, serde_json::to_vec(&on_disk).unwrap()).unwrap();
    let Drift::Differs(lines) = drift(&targets[0]) else {
        panic!("expected a diff");
    };
    assert!(
        lines.contains(&"~ autonomous.workFinder.maxConcurrent: 12 -> 8".to_string()),
        "{lines:?}"
    );
    assert!(
        lines.contains(&"~ autonomous.autoUpdate: null -> {\"settleSecs\":3600}".to_string()),
        "{lines:?}"
    );
    assert!(lines.contains(&"- handEdited: true".to_string()), "{lines:?}");
    assert!(lines.contains(&"+ forge.githubApp.appId: \"1\"".to_string()), "{lines:?}");
    assert_eq!(check_exit_code(&[Drift::InSync, Drift::Differs(lines)]), 1);
}

#[test]
fn unparseable_target_is_drift() {
    let p = paths();
    let targets = render(&snapshot_of(&sample_files()), "build-1", &p.machine, &p.local).unwrap();
    std::fs::create_dir_all(p.machine.parent().unwrap()).unwrap();
    std::fs::write(&p.machine, "{ <<<<<<< HEAD").unwrap();
    assert!(matches!(drift(&targets[0]), Drift::Unparseable(_)));
}

#[test]
fn write_keeps_exactly_one_backup_and_leaves_other_backups_alone() {
    let p = paths();
    let targets = render(&snapshot_of(&sample_files()), "build-1", &p.machine, &p.local).unwrap();
    let dir = p.machine.parent().unwrap();
    std::fs::create_dir_all(dir).unwrap();
    let hand = dir.join("defaults.json.bak-20260101-pre-change");
    std::fs::write(&hand, "{}").unwrap();

    std::fs::write(&p.machine, r#"{"v":1}"#).unwrap();
    let (wrote, b1) = write(&targets[0], "20260101T000000Z").unwrap();
    assert!(wrote);
    let b1 = b1.unwrap();
    assert_eq!(std::fs::read_to_string(&b1).unwrap(), r#"{"v":1}"#);

    std::fs::write(&p.machine, r#"{"v":2}"#).unwrap();
    let (_, b2) = write(&targets[0], "20260101T000100Z").unwrap();
    let b2 = b2.unwrap();
    assert!(!b1.exists(), "the older fleet-store backup is pruned");
    assert_eq!(std::fs::read_to_string(&b2).unwrap(), r#"{"v":2}"#);
    assert!(hand.exists(), "an operator's own backup is never touched");
    assert_eq!(drift(&targets[0]), Drift::InSync);
}

#[test]
fn a_host_without_local_json_renders_only_the_machine_tier() {
    let p = paths();
    let targets = render(&snapshot_of(&sample_files()), "build-2", &p.machine, &p.local).unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].value["autonomous"]["workFinder"]["maxConcurrent"], json!(2));
}

#[test]
fn an_unknown_host_is_an_error_naming_the_fix() {
    let p = paths();
    let err = render(&snapshot_of(&sample_files()), "nope", &p.machine, &p.local).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("host `nope` is not in the store") && msg.contains("LOOM_HOST_ID"),
        "{msg}"
    );
}

#[test]
fn a_malformed_store_file_is_an_error_not_an_empty_tier() {
    let p = paths();
    let mut files = sample_files();
    files.insert("fleet/hosts/build-1/defaults.json".to_string(), "[1,2]".to_string());
    let err = render(&snapshot_of(&files), "build-1", &p.machine, &p.local).unwrap_err();
    assert!(err.to_string().contains("not a JSON object"), "{err}");
    files.insert("fleet/hosts/build-1/defaults.json".to_string(), "{oops".to_string());
    assert!(render(&snapshot_of(&files), "build-1", &p.machine, &p.local).is_err());
}

#[test]
fn a_path_traversing_host_id_is_refused() {
    let p = paths();
    assert!(render(&snapshot_of(&sample_files()), "../x", &p.machine, &p.local).is_err());
}
