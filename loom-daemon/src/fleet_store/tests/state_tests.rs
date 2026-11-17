use super::*;

use serde_json::json;

/// The run-state document, as `fleet.json`'s `state` section.
fn state_doc() -> Value {
    json!({
        "fleet": {
            "state": "stopped",
            "since": "2026-01-01T01:30Z",
            "by": "operator",
            "reason": "Clearing the backlog before a restart."
        },
        "hosts": {
            "build-1": {"state": "running"},
            "build-2": {"state": "stopped"},
            "build-3": {}
        }
    })
}

fn resolve(doc: &Value, host: &str) -> Result<HostState> {
    resolve_value(doc, host, "fleet.json `state`")
}

#[test]
fn a_host_entry_overrides_the_fleet_default() {
    let hs = resolve(&state_doc(), "build-1").unwrap();
    assert_eq!(hs.state, RunState::Running);
    assert_eq!(hs.source, "host");
    assert_eq!(hs.reason, None, "fields come from the entry that set the state");
}

#[test]
fn a_host_without_an_entry_gets_the_fleet_default() {
    for host in ["build-3", "build-9"] {
        let hs = resolve(&state_doc(), host).unwrap();
        assert_eq!(hs.state, RunState::Stopped);
        assert_eq!(hs.source, "fleet");
        assert_eq!(hs.by.as_deref(), Some("operator"));
        assert_eq!(hs.since.as_deref(), Some("2026-01-01T01:30Z"));
        assert_eq!(hs.reason.as_deref(), Some("Clearing the backlog before a restart."));
    }
}

#[test]
fn an_unknown_state_is_an_error() {
    let mut doc = state_doc();
    doc["hosts"]["build-1"]["state"] = json!("sleeping");
    let err = resolve(&doc, "build-1").unwrap_err();
    assert!(err.to_string().contains("`sleeping`"), "{err}");
}

#[test]
fn no_state_anywhere_is_an_error() {
    assert!(resolve(&json!({"hosts": {"other": {"state": "paused"}}}), "build-1").is_err());
    assert_eq!(
        resolve(&json!({"hosts": {"build-1": {"state": "paused"}}}), "build-1")
            .unwrap()
            .state,
        RunState::Paused
    );
}

#[test]
fn a_store_without_fleet_json_has_no_state_even_with_a_legacy_file() {
    use crate::fleet_store::test_support::snapshot_of;
    let files = std::collections::BTreeMap::from([(
        "fleet/state.yml".to_string(),
        "fleet:\n  state: stopped\n".to_string(),
    )]);
    let s = snapshot_of(&files);
    assert!(resolve_snapshot(&s, "build-1").unwrap().is_none());
    assert!(missing_message(&s).contains("no fleet.json"));
    assert!(missing_message(&s).contains("no longer read"));
}
