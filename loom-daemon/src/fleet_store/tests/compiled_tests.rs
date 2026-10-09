//! `fleet.json` (#10705): golden equality with the legacy files, the absent →
//! legacy fallback, and fail-closed on a present-but-invalid document.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::*;
use crate::fleet_store::render::{render, Target};
use crate::fleet_store::roster::{self, Roster};
use crate::fleet_store::state::{self, HostState};
use crate::fleet_store::test_support::{sample_files, snapshot_of};
use crate::fleet_store::{fetch::Snapshot, FLEET_JSON_PATH};

/// The compiled form of [`sample_files`]: the same roster, state and tiers in
/// the shape fleet-gitops' `scripts/render.py` writes.
fn sample_compiled() -> Value {
    json!({
        "_generated": {
            "generator": "scripts/render.py",
            "note": "Generated from fleet.yml by scripts/render.py; do not edit.",
            "schema": "schema/fleet.schema.json",
            "schema_version": 1,
            "source": "fleet.yml"
        },
        "config": {
            "defaults": {
                "autonomous": {
                    "autoUpdate": {"settleSecs": 3600},
                    "roleRunner": {"enabled": true, "roles": ["judge"]}
                },
                "forge": {"githubApp": {"appId": "1"}}
            },
            "hosts": {
                "build-1": {
                    "defaults": {
                        "autonomous": {
                            "roleRunner": {"roles": ["judge", "doctor"]},
                            "workFinder": {"maxConcurrent": 8}
                        },
                        "forge": {"githubApp": {"privateKeyPath": "/k.pem"}}
                    },
                    "local": {"observability": {"enabled": true}}
                },
                "build-2": {
                    "defaults": {"autonomous": {"workFinder": {"maxConcurrent": 2}}}
                }
            }
        },
        "dispatch": {},
        "hosts": [{"name": "build-1"}, {"name": "build-2"}],
        "repos": [
            {"dir": "app", "fleet": true, "fleet_priority": 5, "name": "app"}
        ],
        "root": "/srv/src",
        "state": {
            "fleet": {"state": "running"},
            "hosts": {"build-2": {"state": "paused"}}
        },
        "topology": {},
        "versions": {}
    })
}

fn legacy_only() -> Snapshot {
    snapshot_of(&sample_files())
}

fn compiled_only(doc: &Value) -> Snapshot {
    let mut f = BTreeMap::new();
    f.insert(FLEET_JSON_PATH.to_string(), serde_json::to_string_pretty(doc).unwrap());
    snapshot_of(&f)
}

/// Legacy files and a `fleet.json` (raw text) side by side, as a store in
/// transition publishes them.
fn both(fleet_json: &str) -> Snapshot {
    let mut f = sample_files();
    f.insert(FLEET_JSON_PATH.to_string(), fleet_json.to_string());
    snapshot_of(&f)
}

fn home() -> PathBuf {
    PathBuf::from("/home/x")
}

fn read_roster(s: &Snapshot) -> anyhow::Result<Option<Roster>> {
    roster::from_snapshot(s, &home())
}

fn read_state(s: &Snapshot, host: &str) -> anyhow::Result<Option<HostState>> {
    state::resolve_snapshot(s, host)
}

fn read_tiers(s: &Snapshot, host: &str) -> anyhow::Result<Vec<Target>> {
    render(s, host, Path::new("/m/defaults.json"), Path::new("/w/local.json"))
}

#[test]
fn golden_roster_state_and_tiers_equal_the_legacy_files() {
    let legacy = legacy_only();
    let compiled = compiled_only(&sample_compiled());

    let want = read_roster(&legacy).unwrap().expect("legacy roster");
    assert_eq!(read_roster(&compiled).unwrap().expect("compiled roster"), want);

    for host in ["build-1", "build-2", "build-3"] {
        assert_eq!(
            read_state(&compiled, host).unwrap(),
            read_state(&legacy, host).unwrap(),
            "state for {host}"
        );
    }
    for host in ["build-1", "build-2"] {
        assert_eq!(
            read_tiers(&compiled, host).unwrap(),
            read_tiers(&legacy, host).unwrap(),
            "tiers for {host}"
        );
    }
    // build-1 has both tiers, build-2 only the machine tier — in both forms.
    assert_eq!(read_tiers(&compiled, "build-1").unwrap().len(), 2);
    assert_eq!(read_tiers(&compiled, "build-2").unwrap().len(), 1);
}

#[test]
fn unknown_host_is_refused_from_fleet_json_as_from_the_legacy_files() {
    let err = read_tiers(&compiled_only(&sample_compiled()), "build-9")
        .unwrap_err()
        .to_string();
    assert!(err.contains("host `build-9` is not in the store"), "{err}");
    assert!(err.contains("config.hosts.build-9.defaults"), "{err}");
    assert!(read_tiers(&legacy_only(), "build-9").is_err());
}

#[test]
fn present_fleet_json_wins_and_the_legacy_files_are_not_read() {
    let mut doc = sample_compiled();
    doc["repos"][0]["fleet_priority"] = json!(7);
    doc["state"]["fleet"]["state"] = json!("stopped");
    doc["config"]["defaults"]["forge"]["githubApp"]["appId"] = json!("2");
    let s = both(&doc.to_string());

    let r = read_roster(&s).unwrap().unwrap();
    assert_eq!(r.records[0].fleet_priority, Some(7));
    assert_eq!(read_state(&s, "build-1").unwrap().unwrap().state, state::RunState::Stopped);
    let machine = &read_tiers(&s, "build-1").unwrap()[0];
    assert_eq!(machine.value["forge"]["githubApp"]["appId"], json!("2"));
}

#[test]
fn absent_fleet_json_falls_back_to_the_legacy_files() {
    let s = legacy_only();
    assert!(from_snapshot(&s).unwrap().is_none());
    assert_eq!(read_roster(&s).unwrap().unwrap().records[0].name, "app");
    assert_eq!(read_state(&s, "build-2").unwrap().unwrap().state, state::RunState::Paused);
    assert_eq!(read_tiers(&s, "build-1").unwrap().len(), 2);
}

#[test]
fn a_store_with_neither_form_reads_as_absent() {
    let s = snapshot_of(&BTreeMap::new());
    assert!(read_roster(&s).unwrap().is_none());
    assert!(read_state(&s, "build-1").unwrap().is_none());
    assert!(roster::missing_message(&s).contains("neither fleet.json nor repos.yml"));
    assert!(state::missing_message(&s).contains("neither fleet.json nor fleet/state.yml"));
}

/// Every reader refuses `text` as `fleet.json`, even though valid legacy
/// files sit beside it, and the error names `fleet.json`.
fn assert_fails_closed(text: &str, want: &str) {
    let s = both(text);
    let errs = [
        read_roster(&s).map(|_| ()).unwrap_err(),
        read_state(&s, "build-1").map(|_| ()).unwrap_err(),
        read_tiers(&s, "build-1").map(|_| ()).unwrap_err(),
    ];
    for e in errs {
        let e = format!("{e:#}");
        assert!(e.contains("fleet.json"), "{e}");
        assert!(e.contains(want), "want `{want}` in: {e}");
    }
}

#[test]
fn malformed_fleet_json_fails_closed_with_no_fallback() {
    assert_fails_closed("{\"root\": ", "not valid JSON");
    assert_fails_closed("[]", "top level must be a JSON object");
}

#[test]
fn unknown_schema_version_fails_closed() {
    let mut doc = sample_compiled();
    doc["_generated"]["schema_version"] = json!(2);
    assert_fails_closed(&doc.to_string(), "reads only version 1");
    doc["_generated"]["schema_version"] = json!("1");
    assert_fails_closed(&doc.to_string(), "reads only version 1");
    doc["_generated"]
        .as_object_mut()
        .unwrap()
        .remove("schema_version");
    assert_fails_closed(&doc.to_string(), "schema_version` is missing");
    doc.as_object_mut().unwrap().remove("_generated");
    assert_fails_closed(&doc.to_string(), "no `_generated` header");
}

#[test]
fn a_misshapen_section_fails_closed_for_every_reader() {
    for (path, bad, want) in [
        ("/root", json!(3), "`root` must be a string"),
        ("/repos", json!({}), "`repos` must be a list"),
        ("/state", json!("running"), "`state` must be an object"),
        ("/config", json!([]), "`config` must be an object"),
        ("/config/defaults", json!(null), "`config.defaults` must be an object"),
        ("/config/hosts", json!([]), "`config.hosts` must be an object"),
        ("/config/hosts/build-2", json!(1), "`config.hosts.build-2` must be an object"),
        (
            "/config/hosts/build-1/local",
            json!("x"),
            "`config.hosts.build-1.local` must be an object",
        ),
    ] {
        let mut doc = sample_compiled();
        *doc.pointer_mut(path).unwrap_or_else(|| panic!("{path}")) = bad;
        assert_fails_closed(&doc.to_string(), want);
    }
}

#[test]
fn roster_validation_applies_to_fleet_json_records() {
    let mut doc = sample_compiled();
    doc["repos"][0]["firewall"] = json!(true);
    let err = format!("{:#}", read_roster(&compiled_only(&doc)).unwrap_err());
    assert!(err.contains("fleet.json is not a valid roster"), "{err}");
    assert!(err.contains("fleet: true AND firewall: true"), "{err}");
}

#[test]
fn state_errors_name_fleet_json() {
    let mut doc = sample_compiled();
    doc["state"]["hosts"]["build-2"]["state"] = json!("asleep");
    let err = format!("{:#}", read_state(&compiled_only(&doc), "build-2").unwrap_err());
    assert!(err.contains("fleet.json `state`"), "{err}");
    assert!(err.contains("`asleep` is not running, paused or stopped"), "{err}");
}

#[test]
fn optional_host_tiers_and_hosts_section() {
    let mut doc = sample_compiled();
    doc["config"].as_object_mut().unwrap().remove("hosts");
    let c = parse(&doc.to_string()).unwrap();
    assert!(c.host_defaults("build-1").is_none());
    assert!(c.host_local("build-1").is_none());
    assert_eq!(c.fleet_defaults()["forge"]["githubApp"]["appId"], json!("1"));
}

#[test]
fn fleet_json_is_a_fetched_contract_path() {
    assert!(crate::fleet_store::is_contract_path(FLEET_JSON_PATH));
    assert!(!crate::fleet_store::is_contract_path("fleet.yml"));
    // It survives into a snapshot built from a store listing.
    assert!(both("{}").files.contains_key(FLEET_JSON_PATH));
}
