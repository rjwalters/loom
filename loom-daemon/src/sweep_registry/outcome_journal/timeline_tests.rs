//! End-to-end tests for the `sweep.outcome` record's label-timeline-sourced
//! `judge_verdicts` / `doctor_cycles` (Issue #8222).
//!
//! The pure reconstruction (`parse_label_events` / `signals_from_events`) is
//! unit-tested inside [`super::label_timeline`]; what is tested HERE is the
//! record-construction path in
//! [`SweepRegistry::append_outcome_telemetry_journal`]: that a readable
//! timeline lands both fields on the record, and — the acceptance criterion
//! this file exists for — that a **failing fetcher** omits both while the rest
//! of the record is still written.

use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

/// A registry whose `gh` is `script` and whose forge probes are NOT skipped
/// (the fixture registries set `skip_label_flip`, which suppresses every real
/// forge read on this path by design). Journals are confined to `ws`.
fn timeline_registry(ws: &Path, script: &str) -> SweepRegistry {
    let fake_gh = ws.join("fake-gh-timeline.sh");
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

/// A fake `gh` whose timeline endpoint answers with `rows` (already in the
/// `<rfc3339>\t<label>` shape the module's own `--jq` projects). `repo view`
/// answers so the record's repo slug resolves; everything else is a silent
/// success, as the real `gh` would be for the probes this path makes.
fn gh_timeline_ok(rows: &str) -> String {
    format!(
        "#!/usr/bin/env bash\n\
         if [[ \"$1\" == \"api\" && \"$2\" == */timeline ]]; then\n\
         printf '%s' '{rows}'\n\
         exit 0\n\
         fi\n\
         if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
         printf 'rjwalters/loom\\n'\n\
         exit 0\n\
         fi\n\
         exit 0\n",
        rows = rows.replace('\'', "'\\''"),
    )
}

/// A fake `gh` whose timeline endpoint FAILS — the forge-failure/rate-limit
/// arm. Everything else still succeeds, so the test proves the omission is
/// scoped to the timeline read and not a broadly broken fixture.
fn gh_timeline_failing() -> String {
    "#!/usr/bin/env bash\n\
     if [[ \"$1\" == \"api\" && \"$2\" == */timeline ]]; then\n\
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

/// A fake `gh` that behaves like the REAL `gh api` does about repo selection:
/// it REJECTS a `--repo` flag (`gh api` has none — only `gh issue`/`gh pr` do)
/// and resolves the repo from the `GH_REPO` env var instead. Answers `rows`
/// only when `GH_REPO` matches `want_repo`, so a caller that passed the repo
/// the wrong way produces an unreadable timeline rather than a silent pass.
fn gh_timeline_requiring_gh_repo_env(rows: &str, want_repo: &str) -> String {
    format!(
        "#!/usr/bin/env bash\n\
         for a in \"$@\"; do\n\
         if [[ \"$a\" == \"--repo\" ]]; then\n\
         printf 'unknown flag: --repo\\n' >&2\n\
         exit 1\n\
         fi\n\
         done\n\
         if [[ \"$1\" == \"api\" && \"$2\" == */timeline ]]; then\n\
         if [[ \"${{GH_REPO:-}}\" != '{want_repo}' ]]; then\n\
         printf 'gh: could not determine the repository\\n' >&2\n\
         exit 1\n\
         fi\n\
         printf '%s' '{rows}'\n\
         exit 0\n\
         fi\n\
         exit 0\n",
        rows = rows.replace('\'', "'\\''"),
    )
}

/// Insert a dead `Running` entry for `issue` that has already recorded
/// `pr_number` on its checkpoint, so the terminal record resolves a PR to read
/// the timeline of. Returns the sweep id.
fn insert_sweep_with_pr(registry: &mut SweepRegistry, issue: u32, pr_number: u32) -> String {
    let sweep_id = insert_dead_running_with_log(registry, issue, 0, "agent-1", "log\n");
    let started_at = Utc::now() - chrono::Duration::seconds(600);
    registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;

    let dir = registry.config().checkpoint_dir();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("issue-{issue}.json")),
        format!(r#"{{"phase":"builder-done","issue":{issue},"pr_number":{pr_number}}}"#),
    )
    .unwrap();
    registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
    sweep_id
}

/// Read the raw JSON of the `sweep.outcome` telemetry line for `issue` — the
/// only way to tell an **omitted** optional field from a `null` one, which the
/// "unknown != zero" contract turns on.
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

/// AC: a sweep whose PR was rejected once, fixed, and then approved carries
/// both verdicts with 1-based per-PR `attempt` numbers, and one completed
/// Doctor cycle — all from the forge label timeline, none of it from the
/// sampled phase history (which here only ever saw `builder-done`).
#[test]
#[serial]
fn records_judge_verdicts_and_doctor_cycles_from_the_label_timeline() {
    let dir = tempdir().unwrap();
    let rows = "2026-09-18T12:00:00Z\tloom:review-requested\n\
                2026-09-18T12:30:00Z\tloom:changes-requested\n\
                2026-09-18T13:00:00Z\tloom:review-requested\n\
                2026-09-18T13:30:00Z\tloom:pr\n";
    let mut registry = timeline_registry(dir.path(), &gh_timeline_ok(rows));
    let issue = 8222;
    let sweep_id = insert_sweep_with_pr(&mut registry, issue, 8299);

    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        600,
        telemetry::SweepResult::Success,
        None,
        None,
    );

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records.iter().find(|r| r.issue == issue).unwrap();
    assert_eq!(record.pr_number, Some(8299));
    assert_eq!(
        record.judge_verdicts,
        Some(vec![
            telemetry::JudgeVerdict {
                attempt: 1,
                verdict: "fail".to_string(),
            },
            telemetry::JudgeVerdict {
                attempt: 2,
                verdict: "pass".to_string(),
            },
        ])
    );
    assert_eq!(record.doctor_cycles, Some(1));
}

/// AC: first-pass judge approval rate is computable from the journal alone —
/// `judge_verdicts[0].verdict == "pass"` over the sweeps that have the field.
#[test]
#[serial]
fn a_first_pass_approval_is_readable_off_the_record_alone() {
    let dir = tempdir().unwrap();
    let rows = "2026-09-18T12:00:00Z\tloom:review-requested\n\
                2026-09-18T12:30:00Z\tloom:pr\n";
    let mut registry = timeline_registry(dir.path(), &gh_timeline_ok(rows));
    let issue = 8223;
    let sweep_id = insert_sweep_with_pr(&mut registry, issue, 8300);

    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        300,
        telemetry::SweepResult::Success,
        None,
        None,
    );

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records.iter().find(|r| r.issue == issue).unwrap();
    let verdicts = record.judge_verdicts.as_ref().unwrap();
    assert_eq!(verdicts[0].attempt, 1);
    assert_eq!(verdicts[0].verdict, "pass");
    assert_eq!(record.doctor_cycles, Some(0));
}

/// AC (the one this file exists for): a forge failure / rate limit during the
/// fetch omits BOTH fields — absent keys, never `[]`/`0` — and the rest of the
/// record is still written.
#[test]
#[serial]
fn a_failing_timeline_fetch_omits_both_fields_and_still_writes_the_record() {
    let dir = tempdir().unwrap();
    let mut registry = timeline_registry(dir.path(), &gh_timeline_failing());
    let issue = 8224;
    let sweep_id = insert_sweep_with_pr(&mut registry, issue, 8301);

    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        420,
        telemetry::SweepResult::Failure,
        Some("preflight-no-cli-start".to_string()),
        None,
    );

    let raw = raw_record(&registry, issue);
    assert!(
        raw.get("judge_verdicts").is_none(),
        "an unreadable timeline must omit judge_verdicts, never emit []: {raw}"
    );
    assert!(
        raw.get("doctor_cycles").is_none(),
        "an unreadable timeline must omit doctor_cycles, never report 0: {raw}"
    );
    // The rest of the record is untouched by the failed read.
    assert_eq!(raw["result"], "failure");
    assert_eq!(raw["pr_number"], 8301);
    assert_eq!(raw["total_duration_sec"], 420);
    assert_eq!(raw["failure_class"], "preflight-no-cli-start");
    assert_eq!(raw["sweep_id"], sweep_id);
}

/// A sweep that never opened a PR has no timeline to read, so both fields are
/// absent rather than a fabricated "observed, zero" — and no `gh` call is made
/// at all.
#[test]
#[serial]
fn a_sweep_with_no_pr_omits_both_fields() {
    let dir = tempdir().unwrap();
    let mut registry = timeline_registry(dir.path(), &gh_timeline_ok(""));
    let issue = 8225;
    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-2", "log\n");

    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        30,
        telemetry::SweepResult::Failure,
        None,
        None,
    );

    let raw = raw_record(&registry, issue);
    assert!(raw.get("pr_number").is_none(), "precondition: no PR: {raw}");
    assert!(raw.get("judge_verdicts").is_none(), "{raw}");
    assert!(raw.get("doctor_cycles").is_none(), "{raw}");
}

/// A PR whose timeline was read but carries no verdict yet (the sweep died
/// before Judge) reports `[]`/`0` — a real observation, and the case a
/// consumer must be able to tell apart from the omitted one above.
#[test]
#[serial]
fn an_observed_timeline_with_no_verdict_reports_empty_not_absent() {
    let dir = tempdir().unwrap();
    let rows = "2026-09-18T12:00:00Z\tloom:review-requested\n";
    let mut registry = timeline_registry(dir.path(), &gh_timeline_ok(rows));
    let issue = 8226;
    let sweep_id = insert_sweep_with_pr(&mut registry, issue, 8302);

    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        90,
        telemetry::SweepResult::Failure,
        None,
        None,
    );

    let raw = raw_record(&registry, issue);
    assert_eq!(
        raw.get("judge_verdicts").and_then(|v| v.as_array()),
        Some(&vec![]),
        "an observed-but-unjudged PR reports an empty list, not an absent key: {raw}"
    );
    assert_eq!(
        raw.get("doctor_cycles").and_then(serde_json::Value::as_u64),
        Some(0),
        "an observed timeline with no completed Doctor cycle reports 0: {raw}"
    );
}

/// A machine-global `LOOM_REPO` override reaches `gh api` as the `GH_REPO`
/// ENV VAR, never as a `--repo` flag: `gh api` has no such flag and exits
/// `unknown flag: --repo` before issuing a request, which would silently omit
/// both fields on every `LOOM_REPO`-configured host. The fake `gh` here
/// enforces both halves of that contract.
#[test]
#[serial]
fn a_loom_repo_override_is_passed_as_the_gh_repo_env_var() {
    let dir = tempdir().unwrap();
    let rows = "2026-09-18T12:00:00Z\tloom:review-requested\n\
                2026-09-18T12:30:00Z\tloom:pr\n";
    let mut registry =
        timeline_registry(dir.path(), &gh_timeline_requiring_gh_repo_env(rows, "rjwalters/loom"));
    let issue = 8228;
    let sweep_id = insert_sweep_with_pr(&mut registry, issue, 8304);

    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        120,
        telemetry::SweepResult::Success,
        None,
        None,
    );
    std::env::remove_var("LOOM_REPO");

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records.iter().find(|r| r.issue == issue).unwrap();
    assert_eq!(
        record.judge_verdicts,
        Some(vec![telemetry::JudgeVerdict {
            attempt: 1,
            verdict: "pass".to_string(),
        }]),
        "a LOOM_REPO-configured host must still read the timeline — a `--repo` \
         flag would have made `gh api` exit before the request"
    );
}

/// `skip_label_flip` (the daemon's "do not touch the forge" mode, and every
/// unit-test fixture) makes this path do no forge I/O at all — so both fields
/// are absent, exactly as every other real-forge probe in this file behaves.
#[test]
#[serial]
fn skip_label_flip_makes_the_fetch_a_no_op() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 8227;
    let sweep_id = insert_sweep_with_pr(&mut registry, issue, 8303);

    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        60,
        telemetry::SweepResult::Success,
        None,
        None,
    );

    let raw = raw_record(&registry, issue);
    assert_eq!(raw["pr_number"], 8303, "precondition: the PR is known: {raw}");
    assert!(raw.get("judge_verdicts").is_none(), "{raw}");
    assert!(raw.get("doctor_cycles").is_none(), "{raw}");
}
