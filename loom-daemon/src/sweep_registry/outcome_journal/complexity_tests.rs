//! End-to-end tests for the `sweep.outcome` record's `complexity` field
//! (Issue #8542).
//!
//! Mirrors [`super::timeline_tests`]'s shape: what is tested HERE is the
//! record-construction path in
//! [`SweepRegistry::append_outcome_telemetry_journal`], not the pure
//! marker-extraction logic (already unit-tested by
//! [`crate::script_helpers::sweep_experiment`]'s own tests).

use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

/// A registry whose `gh` is `script` and whose forge probes are NOT skipped.
/// Journals are confined to `ws`. Identical shape to `timeline_tests`'
/// `timeline_registry`, duplicated here rather than shared so each sibling
/// test file stays self-contained (matches this crate's existing convention
/// of `timeline_tests` vs. `credential_tests`).
fn complexity_registry(ws: &Path, script: &str) -> SweepRegistry {
    let fake_gh = ws.join("fake-gh-complexity.sh");
    std::fs::write(&fake_gh, script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }

    let scripts_dir = ws.join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let spawn = scripts_dir.join("spawn-claude.sh");
    std::fs::write(&spawn, "#!/usr/bin/env bash\nexit 0\n").unwrap();
    let mut sperms = std::fs::metadata(&spawn).unwrap().permissions();
    sperms.set_mode(0o755);
    std::fs::set_permissions(&spawn, sperms).unwrap();

    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.spawn_bin = Some(spawn);
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false;
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    config.outcomes_journal_path = Some(ws.join("test-sweep-outcomes.jsonl"));
    config.outcome_telemetry_path = Some(ws.join("test-sweep-outcome-telemetry.jsonl"));
    SweepRegistry::new(config)
}

/// A fake `gh` whose issue-body endpoint answers with `body`. Every other
/// call (the PR timeline, `repo view`) is a silent success with empty output,
/// matching the real `gh` for a sweep whose PR has no notable timeline.
fn gh_issue_body_ok(body: &str) -> String {
    format!(
        "#!/usr/bin/env bash\n\
         if [[ \"$1\" == \"api\" && \"$2\" == repos/*/issues/* && \"$2\" != */timeline ]]; then\n\
         printf '%s' '{body}'\n\
         exit 0\n\
         fi\n\
         if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
         printf 'rjwalters/loom\\n'\n\
         exit 0\n\
         fi\n\
         exit 0\n",
        body = body.replace('\'', "'\\''"),
    )
}

/// A fake `gh` whose issue-body endpoint FAILS — the forge-failure/rate-limit
/// arm.
fn gh_issue_body_failing() -> String {
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

/// AC: a sweep on an issue carrying a recognized complexity marker produces a
/// journal record with `complexity` set.
#[test]
#[serial]
fn a_marked_issue_carries_complexity_on_the_record() {
    let dir = tempdir().unwrap();
    let body = "Some issue body.\n\n<!-- loom:complexity=complex -->\n";
    let mut registry = complexity_registry(dir.path(), &gh_issue_body_ok(body));
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
    assert_eq!(record.complexity, Some("complex".to_string()));
}

/// AC: a sweep on an unmarked issue omits the `complexity` key entirely —
/// never a fabricated `"routine"`.
#[test]
#[serial]
fn an_unmarked_issue_omits_complexity() {
    let dir = tempdir().unwrap();
    let mut registry = complexity_registry(dir.path(), &gh_issue_body_ok("no marker here"));
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
    assert!(
        raw.get("complexity").is_none(),
        "an unmarked issue must omit complexity, never emit \"routine\": {raw}"
    );
}

/// An issue body carrying an out-of-vocabulary tier (a Curator paraphrase of
/// the closed enum) also omits `complexity` — unlike `resolve-tier-model.sh`'s
/// own dispatch-time fold, this journal field never substitutes a default for
/// an unobserved/invalid value.
#[test]
#[serial]
fn an_invalid_tier_marker_omits_complexity() {
    let dir = tempdir().unwrap();
    let body = "<!-- loom:complexity=trivial -->";
    let mut registry = complexity_registry(dir.path(), &gh_issue_body_ok(body));
    let issue = 8544;
    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-3", "log\n");

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
    assert!(raw.get("complexity").is_none(), "{raw}");
}

/// A forge failure / rate limit during the fetch omits `complexity` and still
/// writes the rest of the record.
#[test]
#[serial]
fn a_failing_fetch_omits_complexity_and_still_writes_the_record() {
    let dir = tempdir().unwrap();
    let mut registry = complexity_registry(dir.path(), &gh_issue_body_failing());
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
    assert!(raw.get("complexity").is_none(), "{raw}");
    assert_eq!(raw["result"], "failure");
    assert_eq!(raw["failure_class"], "preflight-no-cli-start");
}

/// `skip_label_flip` makes this path do no forge I/O at all, exactly as the
/// PR-timeline read behaves.
#[test]
#[serial]
fn skip_label_flip_makes_the_complexity_fetch_a_no_op() {
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
