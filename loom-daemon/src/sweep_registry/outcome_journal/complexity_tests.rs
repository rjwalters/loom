//! End-to-end tests for the `sweep.outcome` record's issue-signal fields
//! (`story_points`, Issue #9432) and for the retired `complexity` key
//! (Issue #11665: the record no longer carries it).
//!
//! Mirrors [`super::timeline_tests`]'s shape: what is tested HERE is the
//! record-construction path in
//! [`SweepRegistry::append_outcome_telemetry_journal`].

use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use tempfile::tempdir;

/// A fake `gh` whose issue endpoint answers with the `{state, closed_at,
/// labels}` projection `complexity_signal` asks for — one call, both the
/// #9432 story points and the #9441 end-state signals. Every
/// other call (the PR timeline, `repo view`) is a silent success with empty
/// output, matching the real `gh` for a sweep whose PR has no notable
/// timeline.
fn gh_issue_ok(labels: &[&str]) -> String {
    let payload = serde_json::json!({
        "state": "open",
        "closed_at": serde_json::Value::Null,
        "labels": labels,
    })
    .to_string();
    format!(
        "#!/usr/bin/env bash\n\
         if [[ \"$1\" == \"api\" && \"$2\" == repos/*/issues/* && \"$2\" != */timeline ]]; then\n\
         printf '%s' '{payload}'\n\
         exit 0\n\
         fi\n\
         if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
         printf 'rjwalters/loom\\n'\n\
         exit 0\n\
         fi\n\
         exit 0\n",
        payload = payload.replace('\'', "'\\''"),
    )
}

/// A fake `gh` whose issue endpoint FAILS — the forge-failure/rate-limit
/// arm.
fn gh_issue_failing() -> String {
    "#!/usr/bin/env bash\n\
     if [[ \"$1\" == \"api\" && \"$2\" == repos/*/issues/* && \"$2\" != */timeline ]]; then\n\
     printf 'gh: API rate limit exceeded\\n' >&2\n\
     exit 1\n\
     fi\n\
     if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
     printf 'rjwalters/loom\\n'\n\
     exit 0\n\
     fi\n\
     exit 0\n"
        .to_string()
}

/// Read the raw JSON of the `sweep.outcome` telemetry line for `issue` — the
/// only way to tell an omitted optional field from a `null` one.
fn raw_record(registry: &SweepRegistry, issue: u32) -> serde_json::Value {
    let path = registry.config().resolve_outcome_telemetry_path();
    let contents = std::fs::read_to_string(&path).expect("telemetry journal must exist");
    contents
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|v| v["record"]["kind"] == "sweep.outcome" && v["record"]["issue"] == issue)
        .expect("a sweep.outcome line for this issue")["record"]
        .clone()
}

/// A sweep on an issue carrying a `points:*` label records `story_points`
/// (label-sourced, off the single issue read) and no `complexity` key.
#[test]
#[serial]
fn a_sized_issue_carries_story_points_and_no_complexity() {
    let dir = tempdir().unwrap();
    let mut registry =
        forge_probing_registry(dir.path(), &gh_issue_ok(&["loom:building", "points:5"]));
    let issue = 8542;
    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-1", "log\n");

    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        300,
        telemetry::SweepResult::Success,
        None,
        None,
        crate::tap_usage::RegionAccounting::default(),
    );

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records.iter().find(|r| r.issue == issue).unwrap();
    assert_eq!(record.story_points, Some(5));
    let raw = raw_record(&registry, issue);
    assert!(raw.get("complexity").is_none(), "the retired tier is never emitted: {raw}");
}

/// An unsized issue omits `story_points` entirely, never a fabricated value.
#[test]
#[serial]
fn an_unsized_issue_omits_story_points() {
    let dir = tempdir().unwrap();
    let mut registry = forge_probing_registry(dir.path(), &gh_issue_ok(&["loom:building"]));
    let issue = 8543;
    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-2", "log\n");

    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        60,
        telemetry::SweepResult::Failure,
        None,
        None,
        crate::tap_usage::RegionAccounting::default(),
    );

    let raw = raw_record(&registry, issue);
    assert!(raw.get("story_points").is_none(), "{raw}");
    assert!(raw.get("complexity").is_none(), "{raw}");
}

/// A forge failure / rate limit during the fetch omits `story_points` and still
/// writes the rest of the record.
#[test]
#[serial]
fn a_failing_fetch_omits_story_points_and_still_writes_the_record() {
    let dir = tempdir().unwrap();
    let mut registry = forge_probing_registry(dir.path(), &gh_issue_failing());
    let issue = 8545;
    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-4", "log\n");

    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        45,
        telemetry::SweepResult::Failure,
        Some("preflight-no-cli-start".to_string()),
        None,
        crate::tap_usage::RegionAccounting::default(),
    );

    let raw = raw_record(&registry, issue);
    assert!(raw.get("story_points").is_none(), "{raw}");
    assert!(raw.get("complexity").is_none(), "{raw}");
    assert_eq!(raw["result"], "failure");
    assert_eq!(raw["failure_class"], "preflight-no-cli-start");
}

/// `skip_label_flip` makes this path do no forge I/O at all, exactly as the
/// PR-timeline read behaves.
#[test]
#[serial]
fn skip_label_flip_makes_the_issue_read_a_no_op() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 8546;
    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-5", "log\n");

    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        30,
        telemetry::SweepResult::Success,
        None,
        None,
        crate::tap_usage::RegionAccounting::default(),
    );

    let raw = raw_record(&registry, issue);
    assert!(raw.get("complexity").is_none(), "{raw}");
}
