use super::*;

const STATE: &str = "# Desired run state
fleet:
  state: stopped
  since: 2026-01-01T01:30Z
  by: operator
  reason: >-
    Clearing the backlog
    before a restart.

hosts:
  build-1:
    state: running      # back early
  build-2:
    state: stopped
  build-3: {}
";

#[test]
fn a_host_entry_overrides_the_fleet_default() {
    let hs = resolve(STATE, "build-1").unwrap();
    assert_eq!(hs.state, RunState::Running);
    assert_eq!(hs.source, "host");
    assert_eq!(hs.reason, None, "fields come from the entry that set the state");
}

#[test]
fn a_host_without_an_entry_gets_the_fleet_default() {
    for host in ["build-3", "build-9"] {
        let hs = resolve(STATE, host).unwrap();
        assert_eq!(hs.state, RunState::Stopped);
        assert_eq!(hs.source, "fleet");
        assert_eq!(hs.by.as_deref(), Some("operator"));
        assert_eq!(hs.since.as_deref(), Some("2026-01-01T01:30Z"));
        assert_eq!(hs.reason.as_deref(), Some("Clearing the backlog before a restart."));
    }
}

#[test]
fn an_unknown_state_is_an_error() {
    let text = STATE.replace("state: running", "state: sleeping");
    let err = resolve(&text, "build-1").unwrap_err();
    assert!(err.to_string().contains("`sleeping`"), "{err}");
}

#[test]
fn no_state_anywhere_is_an_error() {
    assert!(resolve("hosts:\n  other:\n    state: paused\n", "build-1").is_err());
    assert_eq!(
        resolve("hosts:\n  build-1:\n    state: paused\n", "build-1")
            .unwrap()
            .state,
        RunState::Paused
    );
}
