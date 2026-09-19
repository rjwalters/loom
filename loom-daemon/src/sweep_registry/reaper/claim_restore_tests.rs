//! Reaper tests for the daemon-owned **claim-restore / PR-produced** family:
//! whether a sweep that dies or is cancelled without a checkpoint restores its
//! pre-dispatch `loom:building` claim to `loom:issue`, and (issue #8355)
//! whether a reap whose sweep DID produce a PR seeds the #4123 dispatch
//! guard's open-PR memo.
//!
//! Moved verbatim out of the sibling `tests.rs` — that file is over the
//! file-size ratchet's 1000-line threshold and therefore frozen at its current
//! size (`.loom/docs/file-size-policy.md`), so new tests in this family land
//! here instead of growing it.

use super::*;
use crate::sweep_registry::test_support::*;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

/// Issue #3823b: orphaned-claim recovery. A daemon-owned sweep that exits
/// cleanly with NO checkpoint (the self-skip / no-work case) must have its
/// pre-dispatch loom:building claim restored to loom:issue by the reaper —
/// otherwise the claim is orphaned and needs manual reclamation (the exact
/// dogfood symptom). Point `gh_bin` at a fake recorder with the real label
/// path enabled (`skip_label_flip = false`) and assert the restore fired.
#[test]
fn reap_restores_label_for_orphaned_clean_exit_without_pr() {
    let dir = tempdir().unwrap();
    let gh_log = dir.path().join("gh-invocations.log");
    // Fake gh: record the space-joined argv and exit 0.
    let fake_gh = dir.path().join("fake-gh.sh");
    let script = format!(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\nexit 0\n",
        gh_log.display()
    );
    std::fs::write(&fake_gh, &script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }

    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false; // exercise the real restore path
    let mut registry = SweepRegistry::new(config);

    let sweep_id = "sweep-issue-77-test".to_string();
    registry.entries.insert(
        sweep_id.clone(),
        SweepInfo {
            pgid: None,
            sweep_id: sweep_id.clone(),
            kind: SweepKind::Issue(77),
            pid: 2_147_483_640, // ~i32::MAX, almost certainly dead
            token_name: "unknown".into(),
            runtime: "unknown".into(),
            runtime_source: None,
            log_path: registry.compute_log_path(77),
            idempotency_key: None,
            started_at: Utc::now(),
            state: SweepState::Running,
            latest_phase: None,
            pr_number: None, // no PR produced -> recoverable claim
            model: None,
            effort: None,
            depends_on: None,
            repo: None,
        },
    );

    // No checkpoint file exists -> Exited branch -> orphaned-claim recovery.
    let changed = registry.reap_once();
    assert!(changed >= 1);

    let info = registry.get(&sweep_id).unwrap();
    assert!(matches!(info.state, SweepState::Exited { .. }));

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        gh_calls.contains("issue edit 77 --remove-label loom:building --add-label loom:issue"),
        "expected reaper to restore loom:building -> loom:issue for an orphaned \
             clean exit without a PR; got gh invocations: {gh_calls:?}"
    );
}

/// Issue #8355: when the reaper's orphaned-claim branch discovers a
/// checkpoint-less clean exit whose sweep DID produce a PR, it must
/// immediately seed the #4123 guard's open-PR memo (#6788) with that
/// answer — at zero extra forge cost — rather than leaving the memo cold
/// until the next unrelated probe. This is the fix for the incident behind
/// this issue: issue #8170's Builder sweep produced PR #8329 and exited
/// cleanly (checkpoint deleted on success, so the reap lands in exactly
/// this branch), but nothing recorded that PR into the guard's memo, so the
/// FIRST re-probe of issue #8170 — potentially hours later — was cold and
/// had no fallback when GraphQL and REST both failed under a correlated
/// rate-limit exhaustion, letting the dispatch guard fall open onto an
/// issue with a still-open, Judge-approved PR.
///
/// **Reachability is the point of this test's shape.** The PR number is NOT
/// hand-planted on the `SweepInfo` (`pr_number` there is reserved for a
/// future phase and every production construction site sets it to `None`,
/// so asserting on it would test a branch a real daemon can never enter).
/// Instead the test drives the production path end to end: a first reap
/// tick observes a live `builder-done` checkpoint carrying `pr_number`
/// exactly as `sample_phase_transition` does on a real daemon, then the
/// checkpoint is deleted (mirroring the sweep skill's success-path
/// deletion) and the process dies, so the second tick lands in the
/// checkpoint-less clean-exit branch with nothing but the sampled history
/// to seed from.
#[test]
fn reap_seeds_open_pr_memo_when_pr_produced_without_checkpoint() {
    let dir = tempdir().unwrap();
    let gh_log = dir.path().join("gh-invocations.log");
    let fake_gh = dir.path().join("fake-gh.sh");
    let script = format!(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\nexit 0\n",
        gh_log.display()
    );
    std::fs::write(&fake_gh, &script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }

    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false;
    let mut registry = SweepRegistry::new(config);

    let sweep_id = "sweep-issue-8170-test".to_string();
    // `started_at` in the past so the checkpoint written below has an mtime
    // inside this run's window (the #4009 freshness guard in
    // `checkpoint_written_by_run`), and the sweep is still ALIVE on the
    // first tick — this process's own pid is the cheapest guaranteed-live
    // one. Production never populates `SweepInfo::pr_number`, so this
    // fixture leaves it `None` too.
    registry.entries.insert(
        sweep_id.clone(),
        SweepInfo {
            pgid: None,
            sweep_id: sweep_id.clone(),
            kind: SweepKind::Issue(8170),
            pid: std::process::id(),
            token_name: "unknown".into(),
            runtime: "unknown".into(),
            runtime_source: None,
            log_path: registry.compute_log_path(8170),
            idempotency_key: None,
            started_at: Utc::now() - chrono::Duration::seconds(600),
            state: SweepState::Running,
            latest_phase: None,
            pr_number: None,
            model: None,
            effort: None,
            depends_on: None,
            repo: None,
        },
    );

    // Sanity: before any reap, the memo has nothing for this issue.
    assert!(registry.fresh_open_pr_memo(8170, Utc::now()).is_none());

    // Tick 1 — the sweep is still running and its checkpoint records the PR
    // it just opened, exactly as `sweep-checkpoint.sh` writes it from
    // `builder-done` onward. The reaper samples it at the top of the tick.
    let checkpoint_dir = registry.config().checkpoint_dir();
    std::fs::create_dir_all(&checkpoint_dir).unwrap();
    let checkpoint = checkpoint_dir.join("issue-8170.json");
    std::fs::write(&checkpoint, r#"{"phase":"builder-done","issue":8170,"pr_number":8329}"#)
        .unwrap();
    assert_eq!(registry.reap_once(), 0, "a live sweep must not be reaped on the first tick");
    assert_eq!(
        registry.sampled_pr_number(&sweep_id),
        Some(8329),
        "the reap tick's sample_phase_transition must have captured the \
             checkpoint's pr_number (#4704) — this is the production source \
             the memo seed reads"
    );
    assert!(
        registry.fresh_open_pr_memo(8170, Utc::now()).is_none(),
        "sampling alone must not warm the memo — only the reap does"
    );

    // The sweep now finishes successfully: the skill DELETES the checkpoint,
    // and the process exits. Nothing on disk or on the entry names the PR
    // anymore; only the sampled phase history does.
    std::fs::remove_file(&checkpoint).unwrap();
    registry.entries.get_mut(&sweep_id).unwrap().pid = 2_147_483_640; // ~i32::MAX, dead

    // Tick 2 -> Exited branch -> checkpoint-less orphaned-claim recovery,
    // the branch this fix's memo seed lives in.
    let changed = registry.reap_once();
    assert!(changed >= 1);

    let info = registry.get(&sweep_id).unwrap();
    assert!(matches!(info.state, SweepState::Exited { .. }));

    // The memo MUST now be warm with the sweep's PR, at zero extra forge
    // calls: the fake `gh` only ever saw label-flip-shaped commands.
    let memo = registry.fresh_open_pr_memo(8170, Utc::now()).expect(
        "expected reap_once to seed the open-PR memo for issue #8170 from the sampled pr_number",
    );
    assert_eq!(memo.pr, 8329);
    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        !gh_calls.contains("pr list") && !gh_calls.contains("api graphql"),
        "the memo seed must cost no forge round trip; got gh invocations: {gh_calls:?}"
    );
}

/// Issue #3827: a cancelled daemon-owned Issue sweep that never opened a
/// PR must have its pre-dispatch loom:building claim restored to loom:issue
/// by `finish_cancel` — mirroring the reaper's clean-exit recovery (#3823b).
/// Otherwise cancelling a daemon-owned sweep strands the issue in
/// loom:building forever (the live repro: #3780/#3785).
#[test]
fn cancel_restores_label_when_no_pr_produced() {
    let dir = tempdir().unwrap();
    let gh_log = dir.path().join("gh-invocations.log");
    let fake_gh = dir.path().join("fake-gh.sh");
    let script = format!(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\nexit 0\n",
        gh_log.display()
    );
    std::fs::write(&fake_gh, &script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }

    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false; // exercise the real restore path
    let mut registry = SweepRegistry::new(config);

    let kind = SweepKind::Issue(88);
    let started_at = Utc::now();
    let sweep_id = "sweep-issue-88-test".to_string();
    registry.entries.insert(
        sweep_id.clone(),
        SweepInfo {
            pgid: None,
            sweep_id: sweep_id.clone(),
            kind: kind.clone(),
            pid: 2_147_483_640, // ~i32::MAX, almost certainly dead
            token_name: "unknown".into(),
            runtime: "unknown".into(),
            runtime_source: None,
            log_path: registry.compute_log_path(88),
            idempotency_key: None,
            started_at,
            state: SweepState::Running,
            latest_phase: None,
            pr_number: None, // no PR produced -> recoverable claim
            model: None,
            effort: None,
            depends_on: None,
            repo: None,
        },
    );

    // exited_within_grace = true: no SIGKILL, straight to terminal path.
    let outcome = registry.finish_cancel(&sweep_id, 2_147_483_640, &kind, started_at, true);
    assert!(outcome.was_running);

    let info = registry.get(&sweep_id).unwrap();
    assert!(matches!(info.state, SweepState::Exited { .. }));

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        gh_calls.contains("issue edit 88 --remove-label loom:building --add-label loom:issue"),
        "expected finish_cancel to restore loom:building -> loom:issue for a \
             cancelled sweep without a PR; got gh invocations: {gh_calls:?}"
    );
}

/// Issue #3827: a cancelled sweep that DID open a PR (`pr_number` set) must
/// NOT have its label reset — that would yank loom:building out from under
/// an in-flight PR's issue and undo real progress.
#[test]
fn cancel_does_not_restore_label_when_pr_produced() {
    let dir = tempdir().unwrap();
    let gh_log = dir.path().join("gh-invocations.log");
    let fake_gh = dir.path().join("fake-gh.sh");
    let script = format!(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\nexit 0\n",
        gh_log.display()
    );
    std::fs::write(&fake_gh, &script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }

    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false; // real restore path enabled but must not fire
    let mut registry = SweepRegistry::new(config);

    let kind = SweepKind::Issue(99);
    let started_at = Utc::now();
    let sweep_id = "sweep-issue-99-test".to_string();
    registry.entries.insert(
        sweep_id.clone(),
        SweepInfo {
            pgid: None,
            sweep_id: sweep_id.clone(),
            kind: kind.clone(),
            pid: 2_147_483_640,
            token_name: "unknown".into(),
            runtime: "unknown".into(),
            runtime_source: None,
            log_path: registry.compute_log_path(99),
            idempotency_key: None,
            started_at,
            state: SweepState::Running,
            latest_phase: None,
            pr_number: Some(456), // PR opened -> must NOT reset the label
            model: None,
            effort: None,
            depends_on: None,
            repo: None,
        },
    );

    let outcome = registry.finish_cancel(&sweep_id, 2_147_483_640, &kind, started_at, true);
    assert!(outcome.was_running);

    let info = registry.get(&sweep_id).unwrap();
    assert!(matches!(info.state, SweepState::Exited { .. }));

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        !gh_calls.contains("--remove-label loom:building"),
        "expected finish_cancel to NOT restore the label when a PR was \
             produced; got gh invocations: {gh_calls:?}"
    );
}

/// Issue #3827: `SweepKind::PrSet` cancels must be unaffected — the
/// `if let SweepKind::Issue` scoping already excludes them, so no
/// `restore_label_to_ready` call is ever attempted.
#[test]
fn cancel_prset_does_not_restore_label() {
    let dir = tempdir().unwrap();
    let gh_log = dir.path().join("gh-invocations.log");
    let fake_gh = dir.path().join("fake-gh.sh");
    let script = format!(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\nexit 0\n",
        gh_log.display()
    );
    std::fs::write(&fake_gh, &script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }

    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false;
    let mut registry = SweepRegistry::new(config);

    let kind = SweepKind::PrSet(vec![101, 102]);
    let started_at = Utc::now();
    let sweep_id = "sweep-prset-test".to_string();
    registry.entries.insert(
        sweep_id.clone(),
        SweepInfo {
            pgid: None,
            sweep_id: sweep_id.clone(),
            kind: kind.clone(),
            pid: 2_147_483_640,
            token_name: "unknown".into(),
            runtime: "unknown".into(),
            runtime_source: None,
            log_path: registry.compute_log_path(0),
            idempotency_key: None,
            started_at,
            state: SweepState::Running,
            latest_phase: None,
            pr_number: None,
            model: None,
            effort: None,
            depends_on: None,
            repo: None,
        },
    );

    let outcome = registry.finish_cancel(&sweep_id, 2_147_483_640, &kind, started_at, true);
    assert!(outcome.was_running);

    let info = registry.get(&sweep_id).unwrap();
    assert!(matches!(info.state, SweepState::Exited { .. }));

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        !gh_calls.contains("--remove-label loom:building"),
        "expected finish_cancel to NOT touch labels for a PrSet cancel; \
             got gh invocations: {gh_calls:?}"
    );
}
