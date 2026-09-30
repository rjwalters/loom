use super::*;
use crate::fleet_store::state::RunState;

const SAMPLE: &str = "\
# the fleet's desired run state
fleet:
  state: running
  since: \"2026-01-01T00:00Z\"
  by: \"operator\"
hosts:
  build-2:
    state: paused
";

#[test]
fn editing_the_fleet_default_touches_only_its_own_block() {
    let out =
        edit(SAMPLE, None, RunState::Paused, "", "loom-worker-1", "2026-09-30T08:00Z").unwrap();
    let expected = "\
# the fleet's desired run state
fleet:
  state: paused
  since: \"2026-09-30T08:00Z\"
  by: \"loom-worker-1\"
hosts:
  build-2:
    state: paused
";
    assert_eq!(out, expected);
}

#[test]
fn an_empty_reason_leaves_the_block_without_one() {
    let out = edit(SAMPLE, None, RunState::Stopped, "", "op", "t").unwrap();
    assert!(!out.contains("reason"));
}

#[test]
fn a_non_empty_reason_is_recorded_and_quoted() {
    let out = edit(SAMPLE, None, RunState::Stopped, "quarterly maintenance", "op", "t").unwrap();
    assert!(out.contains("  reason: \"quarterly maintenance\""));
}

#[test]
fn editing_an_existing_hosts_entry_leaves_other_hosts_alone() {
    let text = "fleet:\n  state: running\nhosts:\n  build-1:\n    state: running\n  build-2:\n    \
                state: paused\n";
    let out = edit(text, Some("build-2"), RunState::Running, "back up", "op", "t").unwrap();
    assert!(out.contains("  build-1:\n    state: running\n"));
    assert!(out.contains(
        "  build-2:\n    state: running\n    since: \"t\"\n    by: \"op\"\n    \
                           reason: \"back up\"\n"
    ));
}

#[test]
fn creates_a_new_host_entry_when_hosts_exists_but_the_host_does_not() {
    let text = "fleet:\n  state: running\nhosts:\n  build-1:\n    state: running\n";
    let out = edit(text, Some("build-9"), RunState::Paused, "", "op", "t").unwrap();
    assert_eq!(
        out,
        "fleet:\n  state: running\nhosts:\n  build-1:\n    state: running\n  build-9:\n    \
         state: paused\n    since: \"t\"\n    by: \"op\"\n"
    );
}

#[test]
fn creates_the_hosts_mapping_from_scratch_when_absent() {
    let text = "fleet:\n  state: running\n";
    let out = edit(text, Some("build-9"), RunState::Stopped, "", "op", "t").unwrap();
    assert_eq!(
        out,
        "fleet:\n  state: running\nhosts:\n  build-9:\n    state: stopped\n    since: \"t\"\n    \
         by: \"op\"\n"
    );
}

#[test]
fn a_missing_fleet_default_is_a_clear_error_not_a_silent_create() {
    let err = edit(
        "hosts:\n  build-1:\n    state: running\n",
        None,
        RunState::Paused,
        "",
        "op",
        "t",
    )
    .unwrap_err();
    assert!(err.to_string().contains("no top-level `fleet:`"), "{err}");
}

#[test]
fn preserves_a_file_with_no_trailing_newline() {
    let text = "fleet:\n  state: running";
    let out = edit(text, None, RunState::Paused, "", "op", "t").unwrap();
    assert!(!out.ends_with('\n'), "{out:?}");
}
