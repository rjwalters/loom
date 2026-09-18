//! Unit tests for [`super`] — extracted from the parent module's inline
//! `#[cfg(test)] mod tests` (Issue #8056) so the parent stays inside the
//! file-size ratchet (`scripts/check-file-size-budget.sh`). Content is
//! unchanged apart from dedenting and the new-module additions.

use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::time::SystemTime;
use tempfile::tempdir;

/// AC: the durable outcomes journal is written on BOTH terminal shapes —
/// a checkpoint-less `Exited` death (this test) and a checkpoint-present
/// `Crashed` death (the exit-78 test above, and the dedicated crashed-shape
/// test below) — at the same emit sites as the existing bus events.
#[test]
fn outcome_journal_records_exited_terminal_with_no_checkpoint() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());

    insert_dead_running_with_log(&mut registry, 4645, 0, "agent-2", "clean run, no markers\n");
    registry.reap_once();

    let path = registry.config().resolve_outcomes_journal_path();
    let records = sweep_outcomes::read_all(&path);
    let record = records
        .iter()
        .find(|r| r.issue == 4645)
        .expect("Exited terminal outcome must be journaled");
    assert_eq!(record.outcome, "exited");
    assert_eq!(record.repo, registry.config().workspace_root.display().to_string());
}

/// AC: the journal entry outlives `reap_once`'s in-memory GC of terminal
/// entries (the ~1h `TERMINAL_RETENTION_SECS` window) — it is the whole
/// point of a *durable* journal that it survives past the point the
/// in-memory registry has forgotten the sweep ever existed.
#[test]
fn outcome_journal_entry_survives_reap_once_gc() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);

    let sweep_id = insert_dead_running_with_log(
        &mut registry,
        4646,
        0,
        "agent-3",
        "==== loom-daemon dispatch: sweep-issue-4646-0 ====\nToken selection failed:\n",
    );
    registry.reap_once();

    // Force this entry's terminal timestamp far enough in the past that
    // the NEXT tick's GC pass drops it from the in-memory registry.
    if let Some(info) = registry.entries.get_mut(&sweep_id) {
        match &mut info.state {
            SweepState::Exited { at, .. } | SweepState::Crashed { at } => {
                *at = Utc::now() - chrono::Duration::seconds(TERMINAL_RETENTION_SECS + 60);
            }
            other => panic!("expected a terminal state, got {other:?}"),
        }
    }
    registry.reap_once();
    assert!(
        !registry.entries.contains_key(&sweep_id),
        "in-memory entry should be GC'd by the retention-window pass"
    );

    let path = registry.config().resolve_outcomes_journal_path();
    let records = sweep_outcomes::read_all(&path);
    assert!(
        records.iter().any(|r| r.issue == 4646),
        "the durable journal entry must survive reap_once's in-memory GC"
    );
}

// ------------------------------------------------------------------------
// `sweep.outcome` telemetry journal (Issue #4704, absorbs #4137)
// ------------------------------------------------------------------------

/// AC: every terminal transition that journals a #4644 [`OutcomeRecord`]
/// ALSO journals a paired `sweep.outcome` telemetry record carrying
/// model/config/result/refs the narrower #4644 record does not — the
/// crashed shape here, `Failure`.
#[test]
fn telemetry_outcome_records_crashed_terminal_as_failure() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);

    let sweep_id = insert_dead_running_with_log(
        &mut registry,
        4704,
        0,
        "agent-4",
        "==== loom-daemon dispatch: sweep-issue-4704-0 ====\nToken selection failed:\n",
    );
    if let Some(info) = registry.entries.get_mut(&sweep_id) {
        info.model = Some("opus".to_string());
        info.effort = Some("high".to_string());
        info.latest_phase = Some("builder".to_string());
    }
    registry.reap_once();

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records
        .iter()
        .find(|r| r.issue == 4704)
        .expect("sweep.outcome telemetry record must be journaled on a crash");
    assert_eq!(record.result, telemetry::SweepResult::Failure);
    assert_eq!(record.sweep_id, "sweep-issue-4704-0");
    assert_eq!(record.model.as_deref(), Some("opus"));
    assert_eq!(record.effort.as_deref(), Some("high"));
    assert_eq!(record.config.get("token_account").map(String::as_str), Some("agent-4"));
    // Issue #4809: an opus-family model is attributed to Arm A in the
    // free-form `config` map — additive, no schema-version bump.
    assert_eq!(record.config.get("arm").map(String::as_str), Some("A"));
    assert!(record.total_duration_sec >= 0);
    // skip_label_flip is set in the fixture registry, so the forge-probe
    // fields fall back to their private-safe / workspace-path defaults
    // rather than shelling out — never `Public`, never a stray PR number.
    assert_eq!(record.visibility, telemetry::RepoVisibility::Private);
    assert_eq!(record.pr_number, None);
}

/// AC ("result"): a sweep observed completing the Merge phase is
/// classified `Success` — the schema's "merged" terminal state — even
/// though its exit status was never captured (the kill-probe reap path
/// yields no exit code) and its checkpoint was deleted on success.
#[test]
fn telemetry_outcome_records_merged_lifecycle_as_success() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 4705;

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-5", "clean run\n");
    let started_at = Utc::now() - Duration::from_secs(600);
    {
        let info = registry.entries.get_mut(&sweep_id).unwrap();
        info.model = Some("sonnet".to_string());
        info.started_at = started_at;
    }

    for phase in ["builder-done", "judge-done", "merge-done"] {
        write_checkpoint_with_mtime(&registry, issue, phase, SystemTime::now());
        registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
    }
    // The sweep skill deletes the checkpoint after a successful merge.
    std::fs::remove_file(
        registry
            .config
            .checkpoint_dir()
            .join(format!("issue-{issue}.json")),
    )
    .unwrap();

    registry.reap_once();

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records
        .iter()
        .find(|r| r.issue == issue)
        .expect("sweep.outcome telemetry record must be journaled on a merged lifecycle");
    assert_eq!(record.result, telemetry::SweepResult::Success);
    assert_eq!(record.model.as_deref(), Some("sonnet"));
    // Issue #4809: sonnet-family attributes to Arm B.
    assert_eq!(record.config.get("arm").map(String::as_str), Some("B"));
    assert_eq!(
        record
            .phase_durations
            .iter()
            .map(|p| p.phase.as_str())
            .collect::<Vec<_>>(),
        vec!["builder", "judge", "merge"]
    );
}

/// Issue #4809: a model that is neither arm (or no model at all) carries
/// NO `arm` key in the outcome record's `config` map — never a fabricated
/// attribution.
#[test]
fn telemetry_outcome_records_no_arm_for_an_unattributable_model() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());

    let sweep_id = insert_dead_running_with_log(&mut registry, 4809, 0, "agent-15", "clean run\n");
    if let Some(info) = registry.entries.get_mut(&sweep_id) {
        info.model = Some("claude-haiku-5".to_string());
    }
    registry.reap_once();

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records.iter().find(|r| r.issue == 4809).unwrap();
    assert_eq!(record.config.get("arm"), None);
}

/// A checkpoint-less death whose exit status was never observed is NOT
/// success: an unobservable exit is not evidence that anything landed.
#[test]
fn telemetry_outcome_records_unverified_exit_as_failure() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());

    insert_dead_running_with_log(&mut registry, 4714, 0, "agent-14", "no markers\n");
    registry.reap_once();

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records.iter().find(|r| r.issue == 4714).unwrap();
    assert_eq!(record.result, telemetry::SweepResult::Failure);
}

/// AC: an operator/watchdog-initiated cancel is telemetry-classified
/// `Cancelled` — distinct from both `Success` and `Failure`.
#[test]
fn telemetry_outcome_records_cancel_as_cancelled() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());

    let kind = SweepKind::Issue(4706);
    let started_at = Utc::now();
    let sweep_id = "sweep-issue-4706-test".to_string();
    registry.entries.insert(
        sweep_id.clone(),
        SweepInfo {
            pgid: None,
            sweep_id: sweep_id.clone(),
            kind: kind.clone(),
            pid: 2_147_483_640,
            token_name: "agent-6".into(),
            runtime: "claude".into(),
            runtime_source: None,
            log_path: registry.compute_log_path(4706),
            idempotency_key: None,
            started_at,
            state: SweepState::Running,
            latest_phase: None,
            pr_number: None,
            model: Some("opus".to_string()),
            effort: None,
            depends_on: None,
            repo: None,
        },
    );

    registry.finish_cancel(&sweep_id, 2_147_483_640, &kind, started_at, true);

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records
        .iter()
        .find(|r| r.issue == 4706)
        .expect("sweep.outcome telemetry record must be journaled on cancel");
    assert_eq!(record.result, telemetry::SweepResult::Cancelled);
    assert_eq!(record.model.as_deref(), Some("opus"));
}

/// AC: the telemetry journal entry survives `reap_once`'s in-memory GC of
/// terminal entries, mirroring [`outcome_journal_entry_survives_reap_once_gc`]
/// for the #4644 journal.
#[test]
fn telemetry_outcome_entry_survives_reap_once_gc() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);

    let sweep_id = insert_dead_running_with_log(
        &mut registry,
        4707,
        0,
        "agent-7",
        "==== loom-daemon dispatch: sweep-issue-4707-0 ====\nToken selection failed:\n",
    );
    registry.reap_once();

    if let Some(info) = registry.entries.get_mut(&sweep_id) {
        match &mut info.state {
            SweepState::Exited { at, .. } | SweepState::Crashed { at } => {
                *at = Utc::now() - chrono::Duration::seconds(TERMINAL_RETENTION_SECS + 60);
            }
            other => panic!("expected a terminal state, got {other:?}"),
        }
    }
    registry.reap_once();
    assert!(!registry.entries.contains_key(&sweep_id));

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    assert!(
        records.iter().any(|r| r.issue == 4707),
        "the durable telemetry journal entry must survive reap_once's in-memory GC"
    );
}

/// AC: with neither a sampled transition history nor a known
/// `latest_phase`, `phase_durations` is empty rather than fabricating a
/// phase name.
#[test]
fn telemetry_outcome_phase_durations_empty_without_a_known_phase() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());

    insert_dead_running_with_log(&mut registry, 4708, 0, "agent-8", "clean run\n");
    registry.reap_once();

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records.iter().find(|r| r.issue == 4708).unwrap();
    assert!(record.phase_durations.is_empty());
}

/// AC (#4704 "per-phase durations"): the persisted record carries a REAL
/// per-phase breakdown built from the transitions the registry sampled
/// while the sweep was alive — one entry per observed phase completion, in
/// lifecycle order, with the checkpoint markers normalized to lifecycle
/// names and each duration measured from the previous observation.
#[test]
fn telemetry_outcome_phase_durations_come_from_sampled_transitions() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 4709;

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-9", "log\n");
    // Backdate the dispatch so the first phase's measured duration is
    // unambiguously non-zero (it runs from `started_at` to the first
    // observation).
    let started_at = Utc::now() - Duration::from_secs(300);
    registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;

    // Two phase completions observed by (simulated) reaper ticks while the
    // sweep was still alive — exactly what `reap_once` does per tick.
    write_checkpoint_with_mtime(&registry, issue, "curator-done", SystemTime::now());
    registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
    write_checkpoint_with_mtime(&registry, issue, "judge-rejected", SystemTime::now());
    registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
    // A repeat tick with an unchanged checkpoint must NOT add an entry.
    registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);

    registry.reap_once();

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records
        .iter()
        .find(|r| r.issue == issue)
        .expect("terminal transition must journal a sweep.outcome record");
    assert_eq!(
        record
            .phase_durations
            .iter()
            .map(|p| p.phase.as_str())
            .collect::<Vec<_>>(),
        vec!["curator", "judge"],
        "markers normalize to lifecycle names, in lifecycle order, deduped per tick"
    );
    assert!(
        record.phase_durations[0].duration_sec >= 299,
        "first phase spans dispatch -> first observation, got {}",
        record.phase_durations[0].duration_sec
    );
    let sum: i64 = record.phase_durations.iter().map(|p| p.duration_sec).sum();
    assert!(
        sum <= record.total_duration_sec,
        "the unattributed trailing in-flight segment means phases sum to at most the total \
         ({sum} > {})",
        record.total_duration_sec
    );
}

/// AC ("…and refs"): the record's `pr_number` comes from the checkpoint
/// value sampled while the sweep ran — no forge round trip — and survives
/// the checkpoint being deleted, which is what a *successful* sweep does
/// before its record is written.
#[test]
fn telemetry_outcome_pr_number_comes_from_the_sampled_checkpoint() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 4713;

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-13", "log\n");
    let started_at = Utc::now() - Duration::from_secs(120);
    registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;

    let cp_dir = registry.config.checkpoint_dir();
    std::fs::create_dir_all(&cp_dir).unwrap();
    let cp_path = cp_dir.join(format!("issue-{issue}.json"));
    std::fs::write(
        &cp_path,
        format!(r#"{{"phase":"builder-done","issue":{issue},"pr_number":8123}}"#),
    )
    .unwrap();
    registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
    // The sweep skill deletes the checkpoint on success — the sampled
    // observation must survive that.
    std::fs::remove_file(&cp_path).unwrap();

    registry.reap_once();

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records.iter().find(|r| r.issue == issue).unwrap();
    assert_eq!(record.pr_number, Some(8123));
}

// --- Work-output capture: tokens_in/tokens_out, lines_added/lines_deleted (#5357) ---

/// `git init` a worktree at `registry`'s `issue-<N>` path with `main` at
/// one commit and a checked-out `feature` branch carrying one more
/// commit: `new.txt` (+2 lines) and one appended `README.md` line — a
/// deterministic `(3, 0)` diffstat against `main`'s merge base.
fn seed_worktree_with_diff(registry: &SweepRegistry, issue: u32) -> PathBuf {
    let wt = registry.worktree_path(issue);
    std::fs::create_dir_all(&wt).unwrap();
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .args(args)
            .current_dir(&wt)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?} failed in {}", wt.display());
    };
    git(&["init", "-q", "--initial-branch=main"]);
    git(&["config", "user.email", "loom@example.com"]);
    git(&["config", "user.name", "Loom Test"]);
    std::fs::write(wt.join("README.md"), "line1\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "seed"]);
    git(&["checkout", "-q", "-b", "feature"]);
    std::fs::write(wt.join("README.md"), "line1\nline2\n").unwrap();
    std::fs::write(wt.join("new.txt"), "a\nb\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "work"]);
    wt
}

/// AC: LOC survives a `--merge`-mode sweep's own synchronous worktree
/// cleanup, because it was opportunistically sampled (and cached) while
/// the worktree was still live — the whole reason `sampled_loc` exists
/// rather than only ever live-probing at outcome-write time.
#[test]
fn telemetry_outcome_lines_survive_worktree_removal_after_sampling() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 5357;

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-53", "log\n");
    let started_at = Utc::now() - Duration::from_secs(60);
    registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;

    let wt = seed_worktree_with_diff(&registry, issue);
    write_checkpoint_with_mtime(&registry, issue, "builder-done", SystemTime::now());
    registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
    assert_eq!(
        registry.sampled_loc(&sweep_id),
        Some((3, 0)),
        "the tick must have sampled the diff"
    );

    // Simulate a self-merging sweep's own `merge-pr.sh` cleanup racing
    // ahead of this reaper's terminal-transition write.
    std::fs::remove_dir_all(&wt).unwrap();

    registry.reap_once();

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records.iter().find(|r| r.issue == issue).unwrap();
    assert_eq!(record.lines_added, Some(3));
    assert_eq!(record.lines_deleted, Some(0));
}

/// AC: a sweep that died before any reap tick sampled it (no live process
/// handle, reconstructed after a daemon restart, etc.) still gets its LOC
/// via the outcome-write-time fallback probe, as long as the worktree
/// still exists then — the common Builder-only-dispatch case the curator
/// pass confirmed.
#[test]
fn telemetry_outcome_lines_fall_back_to_a_live_probe_without_prior_sampling() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 5358;

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-54", "log\n");
    registry.entries.get_mut(&sweep_id).unwrap().started_at = Utc::now() - Duration::from_secs(60);
    seed_worktree_with_diff(&registry, issue);
    assert_eq!(
        registry.sampled_loc(&sweep_id),
        None,
        "never sampled — no checkpoint was written"
    );

    registry.reap_once();

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records.iter().find(|r| r.issue == issue).unwrap();
    assert_eq!(record.lines_added, Some(3));
    assert_eq!(record.lines_deleted, Some(0));
}

/// AC: no worktree, never sampled ⇒ the LOC fields are omitted entirely,
/// never a fabricated `(0, 0)`.
#[test]
fn telemetry_outcome_omits_lines_when_the_worktree_never_existed() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 5359;

    insert_dead_running_with_log(&mut registry, issue, 0, "agent-55", "log\n");
    registry.reap_once();

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records.iter().find(|r| r.issue == issue).unwrap();
    assert_eq!(record.lines_added, None);
    assert_eq!(record.lines_deleted, None);
}

/// AC: `tokens_in`/`tokens_out` are aggregated from the sweep's own
/// matched Claude Code transcripts, split by axis rather than combined —
/// input includes both cache counters, output is `output_tokens` alone.
#[test]
#[serial]
fn telemetry_outcome_tokens_come_from_matched_transcripts() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 5360;

    let config_dir = dir.path().join("claude-config");
    std::fs::create_dir_all(config_dir.join("projects")).unwrap();
    std::env::set_var("CLAUDE_CONFIG_DIR", &config_dir);

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-56", "log\n");
    registry.entries.get_mut(&sweep_id).unwrap().started_at = Utc::now() - Duration::from_secs(60);

    let project = config_dir
        .join("projects")
        .join(crate::transcript_tokens::project_slug(&registry.config.workspace_root));
    std::fs::create_dir_all(project.join("uuid-1/subagents")).unwrap();
    let head = format!(
        "{{\"type\":\"user\",\"message\":{{\"content\":\
         \"<command-name>/loom:sweep</command-name>\\n\
         <command-args>{issue} --claim-owned {issue}</command-args>\"}}}}\n"
    );
    let usage = |input: u32, output: u32, read: u32, create: u32| {
        format!(
            "{{\"message\":{{\"usage\":{{\"input_tokens\":{input},\
             \"output_tokens\":{output},\"cache_read_input_tokens\":{read},\
             \"cache_creation_input_tokens\":{create}}}}}}}\n"
        )
    };
    std::fs::write(project.join("uuid-1.jsonl"), format!("{head}{}", usage(10, 20, 300, 40)))
        .unwrap();
    std::fs::write(project.join("uuid-1/subagents/agent-bld.jsonl"), usage(1, 2, 30, 4)).unwrap();

    registry.reap_once();

    std::env::remove_var("CLAUDE_CONFIG_DIR");

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records.iter().find(|r| r.issue == issue).unwrap();
    // input = (10+300+40) + (1+30+4) = 350 + 35; output = 20 + 2.
    assert_eq!(record.tokens_in, Some(385));
    assert_eq!(record.tokens_out, Some(22));

    // Issue #6384: the same real transcripts also populate the per-model
    // breakdown (grouped under the "no model in the fixture usage
    // blocks" bucket, since these fixtures never stamp `model`).
    let by_model = record
        .tokens_by_model
        .as_ref()
        .expect("tokens_by_model must be populated from the same matched transcripts");
    let total_input: i64 = by_model
        .iter()
        .map(|row| row.input + row.cache_read + row.cache_write_5m + row.cache_write_1h)
        .sum();
    let total_output: i64 = by_model.iter().map(|row| row.output).sum();
    assert_eq!(total_input, 385);
    assert_eq!(total_output, 22);
}

/// AC: a sweep with no attributable transcript (pruned logs, or none
/// ever written) omits both token fields — never a fabricated `0`.
#[test]
#[serial]
fn telemetry_outcome_omits_tokens_when_no_transcript_is_attributable() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 5361;

    let config_dir = dir.path().join("claude-config");
    std::fs::create_dir_all(config_dir.join("projects")).unwrap();
    std::env::set_var("CLAUDE_CONFIG_DIR", &config_dir);

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-57", "log\n");
    registry.entries.get_mut(&sweep_id).unwrap().started_at = Utc::now() - Duration::from_secs(60);

    registry.reap_once();

    std::env::remove_var("CLAUDE_CONFIG_DIR");

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records.iter().find(|r| r.issue == issue).unwrap();
    assert_eq!(record.tokens_in, None);
    assert_eq!(record.tokens_out, None);
    // Issue #6384: same "unknown != zero" contract for the per-model
    // breakdown — never a fabricated empty vec.
    assert_eq!(record.tokens_by_model, None);
}

/// A checkpoint left behind by an EARLIER dispatch of the same issue must
/// never be sampled as this run's progress — the #4009 freshness guard
/// (`checkpoint_written_by_run`) applies to phase sampling exactly as it
/// does to every other checkpoint read in this file.
#[test]
fn phase_sampling_ignores_a_checkpoint_from_an_earlier_dispatch() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 4710;

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-10", "log\n");
    let started_at = Utc::now();
    registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;

    // Checkpoint written an hour BEFORE this run started.
    write_checkpoint_with_mtime(
        &registry,
        issue,
        "builder-done",
        SystemTime::now() - Duration::from_secs(3600),
    );
    registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);

    assert!(
        !registry.phase_history.contains_key(&sweep_id),
        "a stale checkpoint must not seed this run's phase history"
    );
}

/// Retained phase observations are capped so a pathological Judge<->Doctor
/// loop cannot grow the per-sweep history without bound.
#[test]
fn phase_history_is_capped() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 4711;

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-11", "log\n");
    let started_at = Utc::now() - Duration::from_secs(10);
    registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;

    for i in 0..(MAX_PHASE_OBSERVATIONS * 2) {
        // Alternate so every tick looks like a genuine transition.
        let phase = if i.is_multiple_of(2) {
            "judge-rejected"
        } else {
            "doctor-done"
        };
        write_checkpoint_with_mtime(&registry, issue, phase, SystemTime::now());
        registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
    }

    assert_eq!(
        registry.phase_history.get(&sweep_id).map(Vec::len),
        Some(MAX_PHASE_OBSERVATIONS)
    );
}

/// The per-SweepId phase history is pruned alongside entry GC, so it
/// cannot accumulate across many dispatches.
#[test]
fn phase_history_is_pruned_with_terminal_entry_gc() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 4712;

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-12", "log\n");
    let started_at = Utc::now() - Duration::from_secs(30);
    registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;
    write_checkpoint_with_mtime(&registry, issue, "curator-done", SystemTime::now());
    registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
    assert!(registry.phase_history.contains_key(&sweep_id));

    registry.reap_once();
    if let Some(info) = registry.entries.get_mut(&sweep_id) {
        match &mut info.state {
            SweepState::Exited { at, .. } | SweepState::Crashed { at } => {
                *at = Utc::now() - chrono::Duration::seconds(TERMINAL_RETENTION_SECS + 60);
            }
            other => panic!("expected a terminal state, got {other:?}"),
        }
    }
    registry.reap_once();

    assert!(!registry.entries.contains_key(&sweep_id));
    assert!(
        !registry.phase_history.contains_key(&sweep_id),
        "phase history must be pruned with the GC'd entry"
    );
}

/// Checkpoint markers normalize to the lifecycle phase names the telemetry
/// schema documents; an unknown marker passes through unchanged rather
/// than being truncated at an arbitrary separator.
#[test]
fn phase_label_normalizes_checkpoint_markers() {
    assert_eq!(phase_label("curator-done"), "curator");
    assert_eq!(phase_label("builder-done"), "builder");
    assert_eq!(phase_label("judge-done"), "judge");
    assert_eq!(phase_label("judge-rejected"), "judge");
    assert_eq!(phase_label("doctor-done"), "doctor");
    assert_eq!(phase_label("merge-done"), "merge");
    // No known suffix ⇒ passed through verbatim.
    assert_eq!(phase_label("some-future-phase"), "some-future-phase");
    assert_eq!(phase_label("curator"), "curator");
}

// ------------------------------------------------------------------------
// `Event::SweepPhase` bus emission (Issue #4863)
// ------------------------------------------------------------------------

/// Drive `n` reaper ticks and drain every `Event::SweepPhase` the registry
/// published, returning the phase strings in emission order.
async fn drain_phase_events(sub: &mut crate::event_bus::Subscription) -> Vec<String> {
    let mut phases = Vec::new();
    while let Ok(recv) = tokio::time::timeout(Duration::from_millis(250), sub.recv()).await {
        match recv {
            Ok(Event::SweepPhase { phase, .. }) => phases.push(phase),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    phases
}

/// AC (#4863): a live sweep advancing through phases publishes exactly ONE
/// `sweep.issue.{N}.phase` event per transition — and republishes nothing
/// on the ticks where it sits in the same phase. The dedupe matters as much
/// as the emission: `reap_once` re-reads the checkpoint every tick (every
/// 30s, plus every read-path reap), so an unguarded emit would republish
/// the same phase for the sweep's entire residence in it.
#[tokio::test]
async fn phase_transitions_publish_one_sweep_phase_event_each() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let bus = Arc::new(crate::event_bus::EventBus::new());
    registry.set_event_bus(bus.clone());
    let mut sub = bus.subscribe::<[&str; 0], &str>([]);

    let issue = 4863;
    let started_at = Utc::now() - Duration::from_secs(60);
    // A live-looking pid, so `reap_once` samples the phase without reaping
    // the entry — the real shape of a mid-flight sweep.
    let _sweep_id = insert_running_at(&mut registry, issue, 0, started_at);

    write_checkpoint_with_mtime(&registry, issue, "curator-done", SystemTime::now());
    registry.reap_once();
    // Two more ticks with the checkpoint unchanged: the sweep is still in
    // the same phase, so these must publish nothing.
    registry.reap_once();
    registry.reap_once();
    write_checkpoint_with_mtime(&registry, issue, "builder-done", SystemTime::now());
    registry.reap_once();
    registry.reap_once();

    assert_eq!(
        drain_phase_events(&mut sub).await,
        vec!["curator".to_string(), "builder".to_string()],
        "one event per transition, none while the sweep sits in a phase"
    );
}

/// AC (#4863): the emitted `phase` agrees with the `sweep.phase` schema's
/// vocabulary (`curator|builder|judge|doctor|merge`) rather than carrying
/// the raw checkpoint marker (`"judge-rejected"`), which is what the
/// dashboard's `SweepPhaseName` renders.
#[tokio::test]
async fn emitted_phase_uses_the_schema_lifecycle_vocabulary() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let bus = Arc::new(crate::event_bus::EventBus::new());
    registry.set_event_bus(bus.clone());
    let mut sub = bus.subscribe::<[&str; 0], &str>([]);

    let issue = 4864;
    let started_at = Utc::now() - Duration::from_secs(60);
    insert_running_at(&mut registry, issue, 0, started_at);

    for marker in ["judge-rejected", "doctor-done", "merge-done"] {
        write_checkpoint_with_mtime(&registry, issue, marker, SystemTime::now());
        registry.reap_once();
    }

    assert_eq!(
        drain_phase_events(&mut sub).await,
        vec![
            "judge".to_string(),
            "doctor".to_string(),
            "merge".to_string()
        ]
    );
}

/// AC (#4863), end-to-end: a REAL phase transition — not a hand-built
/// `Event::SweepPhase` fixture — produces a `sweep.phase` record on the
/// durable telemetry queue via the observability collector's own
/// event→record mapping. This is the link that was missing: every piece of
/// the pipeline below was already correct and unit-tested, but nothing
/// upstream ever published the event that feeds it.
#[tokio::test]
async fn a_real_phase_transition_reaches_the_telemetry_queue() {
    use crate::observability::queue::DurableQueue;
    use crate::telemetry::{RepoVisibility, TelemetryEnvelope, TelemetryRecord};

    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let bus = Arc::new(crate::event_bus::EventBus::new());
    registry.set_event_bus(bus.clone());
    let mut sub = bus.subscribe::<[&str; 0], &str>([]);

    let issue = 4865;
    let started_at = Utc::now() - Duration::from_secs(60);
    let sweep_id = insert_running_at(&mut registry, issue, 0, started_at);

    write_checkpoint_with_mtime(&registry, issue, "builder-done", SystemTime::now());
    registry.reap_once();

    let event = loop {
        let recv = tokio::time::timeout(Duration::from_secs(5), sub.recv())
            .await
            .expect("a phase transition must publish an event");
        match recv.unwrap() {
            ev @ Event::SweepPhase { .. } => break ev,
            _ => continue,
        }
    };
    // The registry stamps its owning workspace root (#3929) on the way out.
    match &event {
        Event::SweepPhase { repo, .. } => assert_eq!(
            repo.as_deref(),
            Some(
                registry
                    .config()
                    .workspace_root
                    .display()
                    .to_string()
                    .as_str()
            )
        ),
        other => panic!("expected SweepPhase, got {other:?}"),
    }

    // Same mapping the collector task applies to every bus event, with the
    // dispatch state a live daemon would already hold for this sweep.
    let mut dispatches = HashMap::new();
    crate::observability::collector::map_event_to_records(
        &Event::SweepGlobalDispatch {
            sweep_id: sweep_id.clone(),
            kind: SweepKind::Issue(issue),
            runtime: None,
            runtime_source: None,
            repo: None,
        },
        issue,
        "rjwalters/loom",
        RepoVisibility::Public,
        &mut dispatches,
    );
    let records = crate::observability::collector::map_event_to_records(
        &event,
        issue,
        "rjwalters/loom",
        RepoVisibility::Public,
        &mut dispatches,
    );

    let queue = DurableQueue::open(dir.path().join("telemetry-queue.jsonl"), 64);
    for record in records {
        queue.push(TelemetryEnvelope::new("host-test", record));
    }

    assert_eq!(queue.len(), 1, "exactly one sweep.phase record queued");
    match &queue.peek_batch(1)[0].record {
        TelemetryRecord::SweepPhase(r) => {
            assert_eq!(r.issue, issue);
            assert_eq!(r.phase, "builder");
            assert_eq!(r.sweep_id, sweep_id);
            assert_eq!(r.repo, "rjwalters/loom");
        }
        other => panic!("expected a SweepPhase record on the queue, got {other:?}"),
    }
}

// ------------------------------------------------------------------------
// Outcome-journal completeness (Issue #8056): failure_class, models_used,
// doctor_cycles, and token-account recovery.
// ------------------------------------------------------------------------

/// Read the raw JSON of the `sweep.outcome` telemetry line for `issue` —
/// the only way to tell an **omitted** optional field from a `null` one,
/// which the "unknown != zero" contract turns on.
fn raw_outcome_record(registry: &SweepRegistry, issue: u32) -> serde_json::Value {
    let path = registry.config().resolve_outcome_telemetry_path();
    let contents = std::fs::read_to_string(&path).expect("telemetry journal must exist");
    let line = contents
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|v| v["record"]["kind"] == "sweep.outcome" && v["record"]["issue"] == issue)
        .expect("a sweep.outcome line for this issue");
    line["record"].clone()
}

/// AC: `failure_class` on a spawn-death record equals the sibling
/// `sweep-outcomes.jsonl` record's classification for the same `sweep_id`
/// — readable off the telemetry record alone, with no external join.
#[test]
fn telemetry_outcome_failure_class_copies_the_preflight_death_class() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);
    let issue = 8056;

    insert_dead_running_with_log(
        &mut registry,
        issue,
        0,
        "agent-1",
        "==== loom-daemon dispatch: sweep-issue-8056-0 ====\nToken selection failed:\n",
    );
    registry.reap_once();

    let siblings = sweep_outcomes::read_all(&registry.config().resolve_outcomes_journal_path());
    let sibling = siblings.iter().find(|r| r.issue == issue).unwrap();
    assert_eq!(
        sibling.death_class.as_deref(),
        Some("preflight-token-selection-failed"),
        "precondition: the sibling journal classifies this as a pre-flight death"
    );

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records.iter().find(|r| r.issue == issue).unwrap();
    assert_eq!(
        record.failure_class.as_deref(),
        Some("preflight-token-selection-failed"),
        "the telemetry record must carry the same class, no sweep_id join required"
    );
}

/// A credit-exhaustion death carries no `death_class` at all (the
/// pre-flight classifier excludes account exhaustion by design), so
/// `failure_class` falls through to the sibling's `crash_classification` —
/// which is how an `account-exhausted:*` record stays distinguishable from
/// a pre-flight one. The sibling journal still carries both fields.
#[test]
fn telemetry_outcome_failure_class_prefers_the_account_exhaustion_class() {
    let dir = tempdir().unwrap();
    let mut registry = backoff_registry(dir.path(), 60, 900);
    let issue = 8058;

    insert_dead_running_with_log(
        &mut registry,
        issue,
        0,
        "agent-1",
        "==== loom-daemon dispatch: sweep-issue-8058-0 ====\n\
         # CLAUDE_CLI_START\n\
         Claude: You're out of usage credits for this model.\n",
    );
    registry.reap_once();

    let siblings = sweep_outcomes::read_all(&registry.config().resolve_outcomes_journal_path());
    let sibling = siblings.iter().find(|r| r.issue == issue).unwrap();
    assert_eq!(
        sibling.crash_classification.as_deref(),
        Some("account-exhausted:model-credits-exhausted"),
    );

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records.iter().find(|r| r.issue == issue).unwrap();
    assert_eq!(
        record.failure_class.as_deref(),
        Some("account-exhausted:model-credits-exhausted"),
    );
}

/// A clean run carries NO `failure_class` key at all — not `null`, not
/// `"unknown"`, not `""`. There is nothing to classify about a success.
#[test]
fn telemetry_outcome_omits_failure_class_when_there_was_no_classification() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 8059;

    // A log that reached the CLI and shows no exhaustion signature: no
    // pre-flight class, no crash classification, nothing to copy.
    insert_dead_running_with_log(
        &mut registry,
        issue,
        0,
        "agent-2",
        "==== loom-daemon dispatch: sweep-issue-8059-0 ====\n\
         # CLAUDE_CLI_START\n\
         clean run\n",
    );
    registry.reap_once();

    let raw = raw_outcome_record(&registry, issue);
    assert!(
        raw.get("failure_class").is_none(),
        "an unclassified terminal transition must omit failure_class entirely: {raw}"
    );
}

/// AC: a sweep whose lifecycle included two Doctor phases reports
/// `doctor_cycles: 2` — the "sonnet passed" vs. "sonnet failed, the Doctor
/// fixed it" discriminator, counted per observed phase, not collapsed.
#[test]
fn telemetry_outcome_counts_doctor_cycles_from_sampled_phases() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 8060;

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-3", "log\n");
    let started_at = Utc::now() - Duration::from_secs(300);
    registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;

    for phase in [
        "builder-done",
        "judge-rejected",
        "doctor-done",
        "judge-rejected",
        "doctor-done",
    ] {
        write_checkpoint_with_mtime(&registry, issue, phase, SystemTime::now());
        registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
    }
    registry.reap_once();

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records.iter().find(|r| r.issue == issue).unwrap();
    assert_eq!(record.doctor_cycles, Some(2));
}

/// "Observed, no Doctor phase" is `0` — a real, load-bearing value, and
/// distinct from the omitted case below.
#[test]
fn telemetry_outcome_reports_zero_doctor_cycles_for_an_observed_clean_lifecycle() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 8061;

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, "agent-4", "log\n");
    let started_at = Utc::now() - Duration::from_secs(120);
    registry.entries.get_mut(&sweep_id).unwrap().started_at = started_at;
    write_checkpoint_with_mtime(&registry, issue, "builder-done", SystemTime::now());
    registry.sample_phase_transition(&sweep_id, &SweepKind::Issue(issue), started_at);
    registry.reap_once();

    let record = raw_outcome_record(&registry, issue);
    assert_eq!(
        record
            .get("doctor_cycles")
            .and_then(serde_json::Value::as_u64),
        Some(0),
        "an observed lifecycle with no Doctor phase reports 0, not an absent key: {record}"
    );
}

/// "Not observed" is an ABSENT key — a sweep that died before the first
/// reaper tick sampled anything has no lifecycle to count, and reporting
/// `0` there would fabricate a "no Doctor phase happened" claim the daemon
/// cannot make.
#[test]
fn telemetry_outcome_omits_doctor_cycles_without_a_sampled_history() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 8062;

    insert_dead_running_with_log(&mut registry, issue, 0, "agent-5", "clean run\n");
    registry.reap_once();

    let raw = raw_outcome_record(&registry, issue);
    assert!(
        raw.get("doctor_cycles").is_none(),
        "an unobserved lifecycle must omit doctor_cycles, never report 0: {raw}"
    );
}

/// AC: `config.token_account` carries the account the sweep actually ran
/// on even when the dispatch-time capture recorded `unknown` — the log's
/// own selection line is durable and is re-read at the terminal
/// transition.
#[test]
fn telemetry_outcome_token_account_recovers_from_the_log_when_the_entry_says_unknown() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 8063;

    let sweep_id = insert_dead_running_with_log(&mut registry, issue, 0, UNKNOWN_TOKEN_NAME, "");
    // The account-selection line `spawn-claude.sh` logs a moment after the
    // 5s dispatch-time capture window gave up, anchored to this dispatch.
    let log_path = registry.entries.get(&sweep_id).unwrap().log_path.clone();
    std::fs::write(
        &log_path,
        format!(
            "==== loom-daemon dispatch: sweep_id={sweep_id} issue={issue} ====\n\
             spawn-claude: using OAuth account 'agent7-2amlogic' (mode=random)\n"
        ),
    )
    .unwrap();
    registry.reap_once();

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records.iter().find(|r| r.issue == issue).unwrap();
    assert_eq!(record.config.get("token_account").map(String::as_str), Some("agent7-2amlogic"),);
    // The sibling #4644 journal, written from the same resolution, agrees.
    let siblings = sweep_outcomes::read_all(&registry.config().resolve_outcomes_journal_path());
    let sibling = siblings.iter().find(|r| r.issue == issue).unwrap();
    assert_eq!(sibling.token_name, "agent7-2amlogic");
}

/// The `unknown` fallback survives for the genuinely-unknowable case: no
/// entry attribution AND no selection line in the log. Never invented.
#[test]
fn telemetry_outcome_token_account_stays_unknown_when_nothing_recorded_it() {
    let dir = tempdir().unwrap();
    let (mut registry, _rec) = fixture_registry(dir.path());
    let issue = 8064;

    insert_dead_running_with_log(
        &mut registry,
        issue,
        0,
        UNKNOWN_TOKEN_NAME,
        "no account selection was ever logged\n",
    );
    registry.reap_once();

    let path = registry.config().resolve_outcome_telemetry_path();
    let records = sweep_outcomes::read_all_sweep_outcomes(&path);
    let record = records.iter().find(|r| r.issue == issue).unwrap();
    assert_eq!(record.config.get("token_account").map(String::as_str), Some(UNKNOWN_TOKEN_NAME),);
}

/// `models_used` lifts the DISTINCT model ids out of the per-model token
/// rows, sorted and deduped — so an escalated sweep is visible at the top
/// level even though its `model` field names only the dispatched model.
#[test]
fn models_used_lifts_distinct_models_from_the_token_rows() {
    let row =
        |model: &str, speed: &str| crate::script_helpers::sweep_experiment::ModelUsageTotals {
            model: model.to_string(),
            speed: speed.to_string(),
            service_tier: "standard".to_string(),
            input: 1,
            cache_read: 0,
            cache_write_5m: 0,
            cache_write_1h: 0,
            output: 1,
        };
    // Same model twice under different speeds must collapse to one id.
    let rows = vec![
        row("claude-sonnet-5", "standard"),
        row("claude-opus-5", "standard"),
        row("claude-sonnet-5", "fast"),
    ];
    assert_eq!(
        models_used_from(Some(&rows)),
        Some(vec!["claude-opus-5".to_string(), "claude-sonnet-5".to_string()]),
    );
}

/// "Unknown != zero": no attributable transcript ⇒ no `models_used` at
/// all, never an empty vec that would read as "ran no models".
#[test]
fn models_used_is_absent_when_no_token_rows_were_found() {
    assert_eq!(models_used_from(None), None);
    assert_eq!(models_used_from(Some(&[])), None);
}
