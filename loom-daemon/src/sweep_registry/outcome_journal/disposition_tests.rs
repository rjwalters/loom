//! End-to-end tests for the `sweep.outcome` record's `disposition` (Issue
//! #9441).
//!
//! The classifier itself is pure and exhaustively contract-tested where it
//! lives ([`crate::telemetry::disposition`]). What is tested HERE is the
//! record-construction path in
//! [`SweepRegistry::append_outcome_telemetry_journal`]: that a real journal
//! line carries the field, that the `landed` ⇔ PR biconditional survives the
//! whole assembly (not just the classifier call), that the issue end state
//! read off the forge actually reaches the record, and that the
//! `failure_class`-is-mandatory invariant holds on emitted records.

use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use tempfile::tempdir;

/// A registry whose `gh` is `script` and whose forge probes are NOT skipped.
/// Journals are confined to `ws`. Same shape as `complexity_tests`'
/// `complexity_registry`, duplicated here rather than shared so each sibling
/// test file stays self-contained (this crate's existing convention).
fn disposition_registry(ws: &Path, script: &str) -> SweepRegistry {
    let fake_gh = ws.join("fake-gh-disposition.sh");
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

/// A fake `gh` answering the `{body, state, closed_at, labels}` projection
/// `complexity_signal` requests. Everything else (the PR timeline, `repo
/// view`) is a silent success, as the real `gh` would be here.
fn gh_issue_state(state: &str, closed_at: Option<&str>, labels: &[&str]) -> String {
    let payload = serde_json::json!({
        "body": "an issue body with no markers",
        "state": state,
        "closed_at": closed_at,
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

/// The single `sweep.outcome` record this journal holds for `issue`.
fn record_for(registry: &SweepRegistry, issue: u32) -> telemetry::SweepOutcomeRecord {
    let path = registry.config().resolve_outcome_telemetry_path();
    sweep_outcomes::read_all_sweep_outcomes(&path)
        .into_iter()
        .find(|r| r.issue == issue)
        .expect("a sweep.outcome record for this issue")
}

/// Emit one record for `issue` with the given terminal signals, using the
/// journal's real construction path.
fn emit(
    registry: &mut SweepRegistry,
    issue: u32,
    duration_sec: i64,
    result: telemetry::SweepResult,
    failure_class: Option<&str>,
) -> telemetry::SweepOutcomeRecord {
    let sweep_id = insert_dead_running_with_log(registry, issue, 0, "agent-1", "log\n");
    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        duration_sec,
        result,
        failure_class.map(str::to_string),
        None,
        crate::tap_usage::RegionAccounting::default(),
    );
    record_for(registry, issue)
}

/// AC2 (the contract): a record whose sweep produced a PR reports `landed`,
/// and — on the very same construction path — one that produced none never
/// does, whatever its `result` says.
#[test]
#[serial]
fn landed_holds_exactly_when_the_record_names_a_pr() {
    let dir = tempdir().unwrap();
    let mut registry = disposition_registry(dir.path(), &gh_issue_state("open", None, &[]));

    // --- With a PR: the checkpoint the sweep sampled names one. ---
    let issue = 94_411;
    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-1", "log\n");
    let started_at = Utc::now() - Duration::from_secs(600);
    registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;
    let cp_dir = registry.config.checkpoint_dir();
    std::fs::create_dir_all(&cp_dir).unwrap();
    let cp_path = cp_dir.join(format!("issue-{issue}.json"));
    std::fs::write(
        &cp_path,
        format!(r#"{{"phase":"builder-done","issue":{issue},"pr_number":9450}}"#),
    )
    .unwrap();
    registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        600,
        telemetry::SweepResult::Success,
        None,
        None,
        crate::tap_usage::RegionAccounting::default(),
    );
    let landed = record_for(&registry, issue);
    assert_eq!(landed.pr_number, Some(9450));
    assert_eq!(landed.disposition, telemetry::SweepDisposition::Landed);
    assert!(landed.disposition_invariants_hold());

    // --- Without a PR: never `landed`, on any result. ---
    for (offset, result) in [
        (1_u32, telemetry::SweepResult::Success),
        (2, telemetry::SweepResult::Failure),
        (3, telemetry::SweepResult::Cancelled),
        (4, telemetry::SweepResult::Blocked),
    ] {
        let issue = 94_420 + offset;
        let record = emit(&mut registry, issue, 900, result, None);
        assert_eq!(record.pr_number, None);
        assert_ne!(
            record.disposition,
            telemetry::SweepDisposition::Landed,
            "no PR must never read as landed ({result:?})"
        );
        assert!(record.disposition_invariants_hold(), "{record:?}");
    }
}

/// AC3: the no-op re-dispatch shape — a clean, short run with no PR and no
/// phases — is reported as `noop_already_done`, distinguishable from a
/// landing, while `result` still says `success` for every pre-#9441 consumer.
#[test]
#[serial]
fn a_short_clean_redispatch_is_reported_as_a_noop_not_a_success_landing() {
    let dir = tempdir().unwrap();
    let mut registry = disposition_registry(dir.path(), &gh_issue_state("open", None, &[]));
    let record = emit(&mut registry, 94_430, 41, telemetry::SweepResult::Success, None);

    assert_eq!(record.disposition, telemetry::SweepDisposition::NoopAlreadyDone);
    assert_eq!(
        record.result,
        telemetry::SweepResult::Success,
        "#9441 is additive: `result` is unchanged beside the new field"
    );
    assert_eq!(record.pr_number, None);
    assert!(record.disposition_invariants_hold());
}

/// An issue that was already closed before this sweep was dispatched had
/// nothing to do — the 1,526-sweeps-on-one-issue shape. The end state comes
/// off the forge read, so it holds even for a long run.
#[test]
#[serial]
fn an_issue_closed_before_dispatch_is_a_noop_even_after_a_long_run() {
    let dir = tempdir().unwrap();
    let closed_at = (Utc::now() - Duration::from_secs(86_400)).to_rfc3339();
    let mut registry =
        disposition_registry(dir.path(), &gh_issue_state("closed", Some(&closed_at), &[]));
    let issue = 94_440;
    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-1", "log\n");
    registry.entries.get_mut(&sweep_id).unwrap().started_at = Utc::now() - Duration::from_secs(60);
    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        3_600,
        telemetry::SweepResult::Failure,
        None,
        None,
        crate::tap_usage::RegionAccounting::default(),
    );

    let record = record_for(&registry, issue);
    assert_eq!(record.disposition, telemetry::SweepDisposition::NoopAlreadyDone);
    assert!(record.disposition_invariants_hold());
}

/// The Curator's "Issues Are Suggestions" paths reach the record: an issue
/// closed during the sweep is `curator_closed`, one handed back to
/// `loom:curated` is `curator_rescoped`. Neither is a failure.
#[test]
#[serial]
fn curator_close_and_rescope_reach_the_record() {
    let dir = tempdir().unwrap();
    let closed_at = Utc::now().to_rfc3339();
    let mut closed_registry =
        disposition_registry(dir.path(), &gh_issue_state("closed", Some(&closed_at), &[]));
    let issue = 94_450;
    let sweep_id = insert_dead_running_with_log(&mut closed_registry, issue, 0, "agent-1", "log\n");
    closed_registry
        .entries
        .get_mut(&sweep_id)
        .unwrap()
        .started_at = Utc::now() - Duration::from_secs(600);
    closed_registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        600,
        telemetry::SweepResult::Success,
        None,
        None,
        crate::tap_usage::RegionAccounting::default(),
    );
    assert_eq!(
        record_for(&closed_registry, issue).disposition,
        telemetry::SweepDisposition::CuratorClosed
    );

    let rescope_dir = tempdir().unwrap();
    let mut rescope_registry =
        disposition_registry(rescope_dir.path(), &gh_issue_state("open", None, &["loom:curated"]));
    let issue = 94_451;
    let record = emit(&mut rescope_registry, issue, 600, telemetry::SweepResult::Failure, None);
    assert_eq!(record.disposition, telemetry::SweepDisposition::CuratorRescoped);
    assert!(record.disposition_invariants_hold());
}

/// The label the reaper's own orphaned-claim recovery restores
/// (`loom:building` → `loom:issue`) is NOT a rescope signal — reading it as
/// one would mislabel every failed sweep as a Curator hand-back.
#[test]
#[serial]
fn the_restored_ready_label_is_not_mistaken_for_a_rescope() {
    let dir = tempdir().unwrap();
    let mut registry =
        disposition_registry(dir.path(), &gh_issue_state("open", None, &["loom:issue"]));
    let record =
        emit(&mut registry, 94_460, 1_800, telemetry::SweepResult::Failure, Some("exit-1"));
    assert_ne!(record.disposition, telemetry::SweepDisposition::CuratorRescoped);
    assert!(record.disposition_invariants_hold());
}

/// An environmental classifier label reaches the record as `env_failure`
/// with the classifier's own (never overwritten) `failure_class`.
#[test]
#[serial]
fn an_exhausted_account_is_an_env_failure_carrying_its_own_class() {
    let dir = tempdir().unwrap();
    let mut registry = disposition_registry(dir.path(), &gh_issue_state("open", None, &[]));
    let record = emit(
        &mut registry,
        94_470,
        12,
        telemetry::SweepResult::Failure,
        Some("account-exhausted:model-credits-exhausted"),
    );
    assert_eq!(record.disposition, telemetry::SweepDisposition::EnvFailure);
    assert_eq!(
        record.failure_class.as_deref(),
        Some("account-exhausted:model-credits-exhausted")
    );
    assert!(record.disposition_invariants_hold());
}

/// Invariant 2 on emitted records, over every terminal shape this path can
/// produce: nothing that asserts a fault (or admits it cannot tell) is ever
/// written without saying why.
#[test]
#[serial]
fn every_emitted_record_satisfies_both_invariants() {
    let dir = tempdir().unwrap();
    let mut registry = disposition_registry(dir.path(), &gh_issue_state("open", None, &[]));
    let mut issue = 94_500;
    let mut fault_records = 0_usize;
    for result in [
        telemetry::SweepResult::Success,
        telemetry::SweepResult::Failure,
        telemetry::SweepResult::Cancelled,
        telemetry::SweepResult::Blocked,
    ] {
        for class in [None, Some("preflight-no-cli-start"), Some("exit-1")] {
            for duration in [0_i64, 45, 400, 2_730] {
                issue += 1;
                let record = emit(&mut registry, issue, duration, result, class);
                assert!(record.disposition_invariants_hold(), "invariant violated: {record:?}");
                if record.disposition.requires_failure_class() {
                    fault_records += 1;
                    assert!(!record.failure_class.unwrap_or_default().is_empty());
                }
            }
        }
    }
    assert!(fault_records > 0, "the sweep must exercise the fault arms");
}

/// A pre-#9441 journal line carries no `disposition` key. It must still
/// decode — the readers drop, rather than error on, an unparseable line, so
/// without the serde default all historical records would vanish.
#[test]
fn a_pre_9441_journal_line_still_decodes_as_unknown() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("sweep-outcome-telemetry.jsonl");
    // Deliberately hand-written, mirroring the pre-#9441 wire shape (no
    // `disposition` key at all).
    std::fs::write(
        &path,
        r#"{"schema_version":2,"emitted_at":"2026-08-01T00:00:00Z","host_id":"h","record":{"kind":"sweep.outcome","repo":"rjwalters/loom","visibility":"public","issue":8542,"sweep_id":"sweep-issue-8542-0","total_duration_sec":120,"result":"success"}}"#,
    )
    .unwrap();

    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    assert_eq!(records.len(), 1, "a pre-#9441 line must not be dropped");
    assert_eq!(records[0].disposition, telemetry::SweepDisposition::Unknown);
}

/// The field is always on the wire — never skipped, never `null` — so a
/// consumer can read it unconditionally instead of branching on presence.
#[test]
#[serial]
fn disposition_is_always_serialized() {
    let dir = tempdir().unwrap();
    let mut registry = disposition_registry(dir.path(), &gh_issue_state("open", None, &[]));
    emit(&mut registry, 94_600, 30, telemetry::SweepResult::Success, None);

    let path = registry.config().resolve_outcome_telemetry_path();
    let contents = std::fs::read_to_string(&path).unwrap();
    let line = contents
        .lines()
        .find(|l| l.contains("\"issue\":94600"))
        .expect("the record line");
    assert!(
        line.contains("\"disposition\":\"noop_already_done\""),
        "disposition must be present on every line: {line}"
    );
}
