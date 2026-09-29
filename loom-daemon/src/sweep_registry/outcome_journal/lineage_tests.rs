//! End-to-end tests for the `sweep.outcome` record's attempt lineage and
//! rework events (Issue #9444).
//!
//! The two derivations are pure and unit-tested where they live
//! ([`super::lineage`], [`super::rework`], and the classification table in
//! [`crate::telemetry::lineage`]). What is tested HERE is the
//! record-construction path in
//! [`SweepRegistry::append_outcome_telemetry_journal`]: that successive real
//! journal lines for one issue form a walkable chain, that the trigger derived
//! from a real predecessor record reaches the wire, that a real PR label
//! timeline lands classified rework events, and that a journal/timeline that
//! could not be read omits the fields rather than fabricating them.
//!
//! The three acceptance criteria this file covers end-to-end:
//!
//! - **AC1** — a sweep whose PR conflicted with its moved base carries a
//!   `merge_conflict` rework event classified `environmental`, and the next
//!   attempt reports `trigger=merge_conflict`.
//! - **AC2** — a Judge `changes-requested` → Doctor loop is `substantive`.
//! - **AC3** — a retry after a spawn death reports
//!   `trigger=retry_after_env_failure` **with** `previous_sweep_id`.

use super::*;
use crate::sweep_registry::test_support::*;
use crate::telemetry::{ReworkClass, ReworkKind, SweepTrigger};
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

/// A registry whose `gh` is `script` and whose forge probes are NOT skipped.
/// Journals are confined to `ws`. Same shape as the sibling test files'
/// registries, duplicated rather than shared so each stays self-contained
/// (this crate's existing convention for these modules).
fn lineage_registry(ws: &Path, script: &str) -> SweepRegistry {
    let fake_gh = ws.join("fake-gh-lineage.sh");
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
/// `<rfc3339>\t<label>` shape `label_timeline`'s own `--jq` projects) and
/// whose `repo view` resolves the slug the lineage filter keys on.
fn gh_with_timeline(rows: &str) -> String {
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

/// A fake `gh` that resolves the repo slug but whose timeline read FAILS —
/// the rate-limited/unreadable-timeline arm.
fn gh_without_timeline() -> String {
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

/// A fake `gh` that cannot resolve the repo slug at all — the record then has
/// no `repo` (#9442's `repo_unresolved`) and therefore no lineage denominator.
fn gh_without_repo_slug() -> String {
    "#!/usr/bin/env bash\nexit 1\n".to_string()
}

/// Emit one terminal record for `issue`, optionally with a PR on its
/// checkpoint so the timeline is read. Returns the emitted sweep id.
///
/// `seq` distinguishes successive attempts at the SAME issue: the registry's
/// sweep ids are `sweep-issue-<n>-<seq>`, so two attempts sharing a `seq`
/// would share an id and the lineage would (correctly) refuse to make an
/// attempt its own predecessor.
fn emit(
    registry: &mut SweepRegistry,
    issue: u32,
    seq: u32,
    pr_number: Option<u32>,
    result: telemetry::SweepResult,
    failure_class: Option<&str>,
) -> String {
    let sweep_id = insert_dead_running_with_log(registry, issue, seq, "agent-1", "log\n");
    let started_at = Utc::now() - chrono::Duration::seconds(600);
    registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;
    if let Some(pr) = pr_number {
        let dir = registry.config().checkpoint_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("issue-{issue}.json")),
            format!(r#"{{"phase":"builder-done","issue":{issue},"pr_number":{pr}}}"#),
        )
        .unwrap();
        registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
    }
    registry.append_outcome_telemetry_journal(
        issue,
        &sweep_id,
        600,
        result,
        failure_class.map(str::to_string),
        None,
        crate::tap_usage::RegionAccounting::default(),
    );
    sweep_id
}

/// Every `sweep.outcome` record for `issue`, in journal order.
fn records_for(registry: &SweepRegistry, issue: u32) -> Vec<telemetry::SweepOutcomeRecord> {
    let path = registry.config().resolve_outcome_telemetry_path();
    sweep_outcomes::read_all_sweep_outcomes(&path)
        .into_iter()
        .filter(|r| r.issue == issue)
        .collect()
}

/// The raw JSON of the LAST `sweep.outcome` line for `issue` — the only way to
/// tell an **omitted** optional field from a `null` one.
fn raw_last(registry: &SweepRegistry, issue: u32) -> serde_json::Value {
    let path = registry.config().resolve_outcome_telemetry_path();
    let contents = std::fs::read_to_string(&path).expect("telemetry journal must exist");
    contents
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|value| value.get("record").cloned())
        .rfind(|record| {
            record.get("issue").and_then(serde_json::Value::as_u64) == Some(u64::from(issue))
        })
        .expect("a sweep.outcome record for this issue")
}

/// **AC3.** A first attempt dies of a spawn death; the retry that follows
/// reports `attempt_index: 2`, `trigger: retry_after_env_failure`, and names
/// its predecessor by `sweep_id`. Both records come off the real journal the
/// registry writes, so this is the whole derivation, not the pure classifier.
#[test]
#[serial]
fn a_retry_after_a_spawn_death_names_its_predecessor_and_reason() {
    let dir = tempdir().unwrap();
    let mut registry = lineage_registry(dir.path(), &gh_without_timeline());
    let issue = 94_441;

    let first = emit(
        &mut registry,
        issue,
        0,
        None,
        telemetry::SweepResult::Failure,
        Some("preflight-no-cli-start"),
    );
    let records = records_for(&registry, issue);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].attempt_index, Some(1));
    assert_eq!(records[0].previous_sweep_id, None);
    assert_eq!(records[0].trigger, Some(SweepTrigger::First));

    let _second = emit(
        &mut registry,
        issue,
        1,
        None,
        telemetry::SweepResult::Failure,
        Some("preflight-no-cli-start"),
    );
    let records = records_for(&registry, issue);
    assert_eq!(records.len(), 2);
    let retry = &records[1];
    assert_eq!(retry.attempt_index, Some(2));
    assert_eq!(retry.previous_sweep_id.as_deref(), Some(first.as_str()));
    assert_eq!(retry.trigger, Some(SweepTrigger::RetryAfterEnvFailure));
    assert_eq!(
        retry.trigger.and_then(SweepTrigger::classification),
        Some(ReworkClass::Environmental),
        "a retry after an environmental fault is environmental rework"
    );
}

/// **AC1.** A PR that conflicted with its moved base carries a
/// `merge_conflict` rework event classified `environmental` on its own record
/// — and the NEXT attempt, whose predecessor landed rather than failed, is
/// triggered `merge_conflict`. Both halves of the acceptance criterion, on the
/// real construction path.
#[test]
#[serial]
fn a_conflicted_pr_records_environmental_rework_and_triggers_the_next_attempt() {
    let dir = tempdir().unwrap();
    let rows = "2026-09-18T12:00:00Z\tloom:review-requested\n\
                2026-09-18T12:30:00Z\tloom:merge-conflict\n\
                2026-09-18T13:00:00Z\tloom:review-requested\n";
    let mut registry = lineage_registry(dir.path(), &gh_with_timeline(rows));
    let issue = 94_442;

    emit(&mut registry, issue, 0, Some(9_450), telemetry::SweepResult::Success, None);
    let first = records_for(&registry, issue).remove(0);
    let events = first
        .rework_events
        .as_deref()
        .expect("a readable timeline must land rework_events");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].kind, ReworkKind::MergeConflict);
    assert_eq!(events[0].classification, ReworkClass::Environmental);
    assert_eq!(events[0].duration_sec, Some(1_800));
    // Its own trigger is `first` — there was no predecessor. In-sweep rework
    // never turns attempt 1 into a retry.
    assert_eq!(first.trigger, Some(SweepTrigger::First));
    assert_eq!(first.disposition, telemetry::SweepDisposition::Landed);

    emit(&mut registry, issue, 1, Some(9_450), telemetry::SweepResult::Success, None);
    let second = records_for(&registry, issue).remove(1);
    assert_eq!(second.attempt_index, Some(2));
    assert_eq!(second.trigger, Some(SweepTrigger::MergeConflict));
    assert_eq!(
        second.trigger.and_then(SweepTrigger::classification),
        Some(ReworkClass::Environmental)
    );
}

/// **AC2.** A Judge `changes-requested` closed by a Doctor hand-back is one
/// `substantive` rework event on the record — and it agrees, by contract, with
/// the `doctor_cycles` count beside it.
#[test]
#[serial]
fn a_judge_changes_requested_doctor_loop_is_substantive_on_the_record() {
    let dir = tempdir().unwrap();
    let rows = "2026-09-18T12:00:00Z\tloom:review-requested\n\
                2026-09-18T12:30:00Z\tloom:changes-requested\n\
                2026-09-18T13:00:00Z\tloom:review-requested\n\
                2026-09-18T13:30:00Z\tloom:pr\n";
    let mut registry = lineage_registry(dir.path(), &gh_with_timeline(rows));
    let issue = 94_443;

    emit(&mut registry, issue, 0, Some(9_451), telemetry::SweepResult::Success, None);
    let record = records_for(&registry, issue).remove(0);
    let events = record.rework_events.as_deref().expect("rework_events");
    let substantive: Vec<_> = events
        .iter()
        .filter(|e| e.classification == ReworkClass::Substantive)
        .collect();
    assert_eq!(substantive.len(), 1, "{events:?}");
    assert_eq!(substantive[0].kind, ReworkKind::Rejudge);
    assert_eq!(substantive[0].duration_sec, Some(1_800));
    assert_eq!(
        u32::try_from(
            events
                .iter()
                .filter(|e| e.kind == ReworkKind::Rejudge)
                .count()
        )
        .unwrap(),
        record.doctor_cycles.expect("doctor_cycles"),
        "the rejudge count and doctor_cycles must never disagree"
    );
}

/// A landed PR whose timeline shows no rework reports `rework_events: []` —
/// an observation, not a default. The clean-landing shape a per-issue effort
/// query charges entirely to `clean`.
#[test]
#[serial]
fn a_clean_landing_reports_an_empty_rework_list_not_an_absent_key() {
    let dir = tempdir().unwrap();
    let rows = "2026-09-18T12:00:00Z\tloom:review-requested\n\
                2026-09-18T12:30:00Z\tloom:pr\n";
    let mut registry = lineage_registry(dir.path(), &gh_with_timeline(rows));
    let issue = 94_444;

    emit(&mut registry, issue, 0, Some(9_452), telemetry::SweepResult::Success, None);
    let raw = raw_last(&registry, issue);
    assert_eq!(
        raw.get("rework_events")
            .and_then(serde_json::Value::as_array),
        Some(&vec![]),
        "an observed-but-rework-free PR is an empty list on the wire: {raw}"
    );
    assert_eq!(raw["trigger"], "first");
    assert_eq!(raw["attempt_index"], 1);
}

/// An unreadable timeline omits `rework_events` entirely while the lineage
/// fields — which come from the local journal, not the forge — are still
/// derived. The two sources fail independently, and the record says which.
#[test]
#[serial]
fn an_unreadable_timeline_omits_rework_but_keeps_the_journal_derived_lineage() {
    let dir = tempdir().unwrap();
    let mut registry = lineage_registry(dir.path(), &gh_without_timeline());
    let issue = 94_445;

    emit(&mut registry, issue, 0, Some(9_453), telemetry::SweepResult::Success, None);
    let raw = raw_last(&registry, issue);
    assert!(
        raw.get("rework_events").is_none(),
        "an unread timeline omits the key rather than claiming no rework: {raw}"
    );
    assert_eq!(raw["attempt_index"], 1);
    assert_eq!(raw["trigger"], "first");
}

/// No resolvable repo slug means no lineage denominator: all three
/// journal-derived fields are omitted rather than fabricated as "attempt 1".
#[test]
#[serial]
fn an_unresolved_repo_slug_omits_the_lineage_entirely() {
    let dir = tempdir().unwrap();
    let mut registry = lineage_registry(dir.path(), &gh_without_repo_slug());
    let issue = 94_446;

    emit(
        &mut registry,
        issue,
        0,
        None,
        telemetry::SweepResult::Failure,
        Some("preflight-no-cli-start"),
    );
    let raw = raw_last(&registry, issue);
    assert_eq!(
        raw.get("repo_unresolved")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "fixture precondition: the slug must not resolve: {raw}"
    );
    for field in ["attempt_index", "previous_sweep_id", "trigger"] {
        assert!(
            raw.get(field).is_none(),
            "{field} must be omitted when there is no repo to count attempts for: {raw}"
        );
    }
}

/// Another issue's attempts never enter this issue's chain — the count is
/// per `repo` + `issue`, which is what makes `attempt_index` mean anything.
#[test]
#[serial]
fn another_issues_attempts_do_not_inflate_this_ones_index() {
    let dir = tempdir().unwrap();
    let mut registry = lineage_registry(dir.path(), &gh_without_timeline());

    for seq in 0..3 {
        emit(
            &mut registry,
            94_447,
            seq,
            None,
            telemetry::SweepResult::Failure,
            Some("preflight-no-cli-start"),
        );
    }
    emit(
        &mut registry,
        94_448,
        0,
        None,
        telemetry::SweepResult::Failure,
        Some("preflight-no-cli-start"),
    );

    let mine = records_for(&registry, 94_448);
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].attempt_index, Some(1));
    assert_eq!(mine[0].previous_sweep_id, None);
    assert_eq!(mine[0].trigger, Some(SweepTrigger::First));

    let theirs = records_for(&registry, 94_447);
    assert_eq!(
        theirs.iter().map(|r| r.attempt_index).collect::<Vec<_>>(),
        vec![Some(1), Some(2), Some(3)]
    );
}
