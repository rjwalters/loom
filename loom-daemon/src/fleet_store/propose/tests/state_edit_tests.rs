use super::*;
use crate::fleet_store::state::RunState;

const SAMPLE: &str = "\
repos: []

state:
  # the fleet's desired run state
  fleet:
    state: running
    since: \"2026-01-01T00:00Z\"
    by: operator
    reason: >-
      Restarted after
      the pause.
  hosts:
    build-2:
      state: paused

config: {}
";

#[test]
fn editing_the_fleet_default_touches_only_its_own_block() {
    let out =
        edit(SAMPLE, None, RunState::Paused, "", "loom-worker-1", "2026-09-30T08:00Z").unwrap();
    let expected = SAMPLE
        .replace("    state: running", "    state: paused")
        .replace("2026-01-01T00:00Z", "2026-09-30T08:00Z")
        .replace("by: operator", "by: loom-worker-1");
    assert_eq!(out, expected);
}

#[test]
fn an_empty_reason_leaves_any_existing_reason_alone() {
    let out = edit(SAMPLE, Some("build-2"), RunState::Stopped, "", "op", "t").unwrap();
    assert!(out.contains("    build-2:\n      state: stopped\n      since: t\n      by: op\n\n"));
    assert!(out.contains("Restarted after"));
}

#[test]
fn a_non_empty_reason_replaces_a_folded_one_and_is_quoted() {
    let out = edit(SAMPLE, None, RunState::Stopped, "quarterly: maintenance", "op", "t").unwrap();
    assert!(
        out.contains("    by: op\n    reason: \"quarterly: maintenance\"\n  hosts:"),
        "{out}"
    );
    assert!(!out.contains("the pause"));
}

#[test]
fn creates_a_new_host_entry_when_hosts_exists_but_the_host_does_not() {
    let out = edit(SAMPLE, Some("build-9"), RunState::Paused, "", "op", "t").unwrap();
    assert!(
        out.contains(
            "    build-2:\n      state: paused\n    build-9:\n      state: paused\n      \
             since: t\n      by: op\n\nconfig:"
        ),
        "{out}"
    );
}

#[test]
fn creates_the_hosts_mapping_from_scratch_when_absent() {
    let text = "state:\n  fleet:\n    state: running\n";
    let out = edit(text, Some("build-9"), RunState::Stopped, "", "op", "t").unwrap();
    assert_eq!(
        out,
        "state:\n  fleet:\n    state: running\n  hosts:\n    build-9:\n      state: stopped\n      \
         since: t\n      by: op\n"
    );
}

#[test]
fn a_missing_fleet_default_is_a_clear_error_not_a_silent_create() {
    let err = edit(
        "state:\n  hosts:\n    build-1:\n      state: running\n",
        None,
        RunState::Paused,
        "",
        "op",
        "t",
    )
    .unwrap_err();
    assert!(err.to_string().contains("no `state.fleet` default"), "{err}");
}

#[test]
fn preserves_a_file_with_no_trailing_newline() {
    let text = "state:\n  fleet:\n    state: running";
    let out = edit(text, None, RunState::Paused, "", "op", "t").unwrap();
    assert!(!out.ends_with('\n'), "{out:?}");
}
