//! `fleet.json` (#10705): the compiled roster, state and tiers, no roster/state
//! fallback to the legacy files, and fail-closed on a present-but-invalid document.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::*;
use crate::fleet_store::render::{render, Target};
use crate::fleet_store::roster::{self, Roster};
use crate::fleet_store::state::{self, HostState};
use crate::fleet_store::test_support::{sample_compiled, sample_files, snapshot_of};
use crate::fleet_store::{fetch::Snapshot, FLEET_JSON_PATH};

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
fn compiled_roster_state_and_tiers_are_read() {
    let compiled = compiled_only(&sample_compiled());

    let roster = read_roster(&compiled).unwrap().expect("compiled roster");
    assert_eq!(roster.records[0].name, "app");
    assert_eq!(roster.records[0].weight, Some(5));

    assert_eq!(
        read_state(&compiled, "build-1").unwrap().unwrap().state,
        state::RunState::Running
    );
    assert_eq!(
        read_state(&compiled, "build-2").unwrap().unwrap().state,
        state::RunState::Paused
    );
    // The compiled tiers equal the legacy files' render of the same store.
    for host in ["build-1", "build-2"] {
        assert_eq!(
            read_tiers(&compiled, host).unwrap(),
            read_tiers(&legacy_only(), host).unwrap(),
            "tiers for {host}"
        );
    }
    // build-1 has both tiers, build-2 only the machine tier.
    assert_eq!(read_tiers(&compiled, "build-1").unwrap().len(), 2);
    assert_eq!(read_tiers(&compiled, "build-2").unwrap().len(), 1);
}

#[test]
fn unknown_host_is_refused_from_fleet_json_() {
    let err = read_tiers(&compiled_only(&sample_compiled()), "build-9")
        .unwrap_err()
        .to_string();
    assert!(err.contains("host `build-9` is not in the store"), "{err}");
    assert!(err.contains("config.hosts.build-9.defaults"), "{err}");
}

#[test]
fn present_fleet_json_wins_and_the_legacy_files_are_not_read() {
    let mut doc = sample_compiled();
    doc["repos"][0]["weight"] = json!(7);
    doc["state"]["fleet"]["state"] = json!("stopped");
    doc["config"]["defaults"]["forge"]["githubApp"]["appId"] = json!("2");
    let s = both(&doc.to_string());

    let r = read_roster(&s).unwrap().unwrap();
    assert_eq!(r.records[0].weight, Some(7));
    assert_eq!(read_state(&s, "build-1").unwrap().unwrap().state, state::RunState::Stopped);
    let machine = &read_tiers(&s, "build-1").unwrap()[0];
    assert_eq!(machine.value["forge"]["githubApp"]["appId"], json!("2"));
}

#[test]
fn a_store_with_only_legacy_roster_and_state_files_reads_as_absent() {
    let s = legacy_only();
    assert!(from_snapshot(&s).unwrap().is_none());
    assert!(read_roster(&s).unwrap().is_none(), "repos.yml is no longer read");
    assert!(
        read_state(&s, "build-2").unwrap().is_none(),
        "fleet/state.yml is no longer read"
    );
    let msg = roster::missing_message(&s);
    assert!(
        msg.contains("no fleet.json") && msg.contains("repos.yml is no longer read"),
        "{msg}"
    );
    // The config tiers are outside this change and still read their files.
    assert_eq!(read_tiers(&s, "build-1").unwrap().len(), 2);
}

#[test]
fn a_store_with_neither_form_reads_as_absent() {
    let s = snapshot_of(&BTreeMap::new());
    assert!(read_roster(&s).unwrap().is_none());
    assert!(read_state(&s, "build-1").unwrap().is_none());
    assert!(roster::missing_message(&s).contains("no fleet.json"));
    let msg = state::missing_message(&s);
    assert!(
        msg.contains("no fleet.json") && msg.contains("fleet/state.yml is no longer read"),
        "{msg}"
    );
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

// ===== per-repo fleet tier: `repos[].config` (#11738) =====

/// A compiled document whose roster exercises every per-repo-tier case.
fn compiled_with_repo_configs() -> Value {
    let mut doc = sample_compiled();
    doc["repos"] = json!([
        {"name": "app", "dir": "app", "fleet": true, "weight": 1, "config": {"autonomous": {"x": 1}, "guards": {}}},
        {"name": "lib", "dir": "lib-dir", "fleet": "maintain", "weight": 1},
        {"name": "vault", "dir": "vault", "fleet": false, "firewall": true, "config": {"a": 1}},
        {"name": "side", "dir": "side", "fleet": false, "config": {"a": 1}},
        {"name": "uncloned", "dir": "uncloned", "fleet": true, "weight": 1, "config": {"a": 1}}
    ]);
    doc
}

#[test]
fn repo_config_accessor_finds_a_record_config_by_name() {
    let doc = parse(&compiled_with_repo_configs().to_string()).unwrap();
    assert_eq!(doc.repo_config("app"), Some(&json!({"autonomous": {"x": 1}, "guards": {}})));
    assert_eq!(doc.repo_config("lib"), None, "a record without config");
    assert_eq!(doc.repo_config("nope"), None, "no such record");
}

#[test]
fn a_non_object_repo_config_fails_closed() {
    let mut doc = sample_compiled();
    doc["repos"][0]["config"] = json!(["not", "an", "object"]);
    assert_fails_closed(&doc.to_string(), "`repos[0].config` must be an object");
}

#[test]
fn repo_targets_cover_desired_cloned_repos_only() {
    use crate::fleet_store::render::{repo_targets, RepoTarget};
    let snap = compiled_only(&compiled_with_repo_configs());
    let cloned = |p: &Path| !p.ends_with("uncloned");
    let got = repo_targets(&snap, &home(), &cloned).unwrap();
    assert_eq!(
        got,
        vec![
            RepoTarget {
                name: "app".into(),
                path: PathBuf::from("/srv/src/app/.loom-local/fleet-repo.json"),
                value: Some(json!({"autonomous": {"x": 1}, "guards": {}})),
            },
            // Desired with no config: the file must not exist.
            RepoTarget {
                name: "lib".into(),
                path: PathBuf::from("/srv/src/lib-dir/.loom-local/fleet-repo.json"),
                value: None,
            },
        ],
        "firewall, unmanaged and uncloned records never get a target"
    );
}

#[test]
fn repo_targets_are_empty_for_a_legacy_store() {
    let got = crate::fleet_store::render::repo_targets(&legacy_only(), &home(), &|_| true).unwrap();
    assert!(got.is_empty(), "repos.yml has no per-repo config: {got:?}");
}

#[test]
fn repo_targets_refuse_an_invalid_roster() {
    let mut doc = compiled_with_repo_configs();
    doc["repos"][0]["firewall"] = json!(true);
    let err = crate::fleet_store::render::repo_targets(&compiled_only(&doc), &home(), &|_| true)
        .unwrap_err();
    assert!(format!("{err:#}").contains("fleet: true AND firewall: true"), "{err:#}");
}
