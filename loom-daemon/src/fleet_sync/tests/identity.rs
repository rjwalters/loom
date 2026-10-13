//! Issue #11740: a hostname-keyed host keeps its fleet config across #11530.

use std::cell::Cell;
use std::collections::BTreeMap;

use super::*;
use crate::fleet_state::{Enforcement, StatePass};
use crate::fleet_store::test_support::{sample_files, snapshot_of};
use crate::fleet_sync::{
    render_line, workspace_resync, ConfigPass, FleetSyncStatus, FloorPass, RosterPass,
};

const GENERATED: &str = "loom-host-0769b806b62a427e";

fn names(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

fn compiled(hosts: &[&str]) -> Snapshot {
    let hosts: serde_json::Map<String, serde_json::Value> = hosts
        .iter()
        .map(|h| ((*h).to_string(), serde_json::json!({"defaults": {}})))
        .collect();
    let doc = serde_json::json!({
        "_generated": {"generator": "scripts/render.py", "schema_version": 1, "source": "fleet.yml"},
        "config": {"defaults": {}, "hosts": hosts},
        "dispatch": {},
        "hosts": [],
        "repos": [],
        "root": "/srv/src",
        "state": {"fleet": {"state": "running"}},
        "topology": {},
        "versions": {}
    });
    let mut f = BTreeMap::new();
    f.insert(store::FLEET_JSON_PATH.to_string(), doc.to_string());
    snapshot_of(&f)
}

#[test]
fn store_hosts_reads_fleet_json_and_the_legacy_files() {
    let mut hosts = store_hosts(&compiled(&["robb-pro", "loom-worker-1"])).unwrap();
    hosts.sort();
    assert_eq!(hosts, names(&["loom-worker-1", "robb-pro"]));
    let mut legacy = store_hosts(&snapshot_of(&sample_files())).unwrap();
    legacy.sort();
    assert_eq!(legacy, names(&["build-1", "build-2"]));
}

#[test]
fn match_roster_needs_exactly_one_case_insensitive_hit() {
    let hosts = names(&["robb-pro", "robb-studio", "loom-worker-1"]);
    assert_eq!(match_roster(&hosts, &names(&["Robb-Pro"])), Some("robb-pro".to_string()));
    // A launchd-style name that names no store host matches nothing.
    assert_eq!(match_roster(&hosts, &names(&["MacBook-Air"])), None);
    assert_eq!(match_roster(&hosts, &[]), None);
    // Two store hosts named by this machine: ambiguous, so neither.
    assert_eq!(match_roster(&hosts, &names(&["robb-pro", "robb-studio"])), None);
}

#[test]
fn a_generated_id_not_in_the_store_adopts_the_one_matching_hostname() {
    let snap = compiled(&["robb-pro", "loom-worker-1"]);
    assert!(host_not_in_store(&snap, GENERATED));
    let asked = Cell::new(None);
    let got = decide(&snap, GENERATED, &names(&["robb-pro"]), |n| {
        asked.set(Some(n.to_string()));
        Ok(true)
    });
    assert_eq!(got.as_deref(), Some("robb-pro"));
    assert_eq!(asked.take().as_deref(), Some("robb-pro"));
    assert!(!host_not_in_store(&snap, "robb-pro"));
}

#[test]
fn nothing_is_adopted_when_the_id_is_in_the_store_or_no_hostname_matches() {
    let snap = compiled(&["robb-pro"]);
    let never = |_: &str| -> std::io::Result<bool> { panic!("must not adopt") };
    assert_eq!(decide(&snap, "robb-pro", &names(&["robb-pro"]), never), None);
    assert_eq!(decide(&snap, GENERATED, &names(&["MacBook-Air"]), never), None);
    assert!(host_not_in_store(&snap, GENERATED), "stays loud when nothing matched");
}

#[test]
fn a_refused_or_failed_adoption_keeps_the_id() {
    let snap = compiled(&["robb-pro"]);
    assert_eq!(decide(&snap, GENERATED, &names(&["robb-pro"]), |_| Ok(false)), None);
    assert_eq!(
        decide(&snap, GENERATED, &names(&["robb-pro"]), |_| Err(std::io::Error::other("ro"))),
        None
    );
}

#[test]
fn an_unreadable_store_is_not_reported_as_not_in_store() {
    let mut f = BTreeMap::new();
    f.insert(store::FLEET_JSON_PATH.to_string(), "{not json".to_string());
    let snap = snapshot_of(&f);
    assert!(!host_not_in_store(&snap, GENERATED));
    assert_eq!(decide(&snap, GENERATED, &names(&["robb-pro"]), |_| Ok(true)), None);
}

#[test]
fn status_text_flags_a_host_not_in_the_store() {
    let mut status = FleetSyncStatus {
        repo: "acme/fleet".to_string(),
        reference: "main".to_string(),
        host: GENERATED.to_string(),
        pass: "timer".to_string(),
        at: Utc::now(),
        interval_secs: 300,
        auto_apply: true,
        config: ConfigPass {
            error: Some(format!("host `{GENERATED}` is not in the store")),
            host_not_in_store: true,
            ..ConfigPass::default()
        },
        roster: RosterPass::default(),
        state: StatePass::default(),
        enforced: Enforcement::Proceed,
        floor: FloorPass::default(),
        workspaces: workspace_resync::WorkspacePass::default(),
        checkouts: Vec::new(),
        checkouts_note: None,
    };
    let text = render_line(Some(&status), status.at).unwrap();
    assert!(text.contains(&NOT_IN_STORE.to_uppercase()), "{text}");
    let json = serde_json::to_value(&status).unwrap();
    assert_eq!(json["config"]["hostNotInStore"], true);

    status.config.host_not_in_store = false;
    let text = render_line(Some(&status), status.at).unwrap();
    assert!(!text.contains(&NOT_IN_STORE.to_uppercase()), "{text}");
    let json = serde_json::to_value(&status).unwrap();
    assert!(json["config"].get("hostNotInStore").is_none(), "omitted when false");
}
