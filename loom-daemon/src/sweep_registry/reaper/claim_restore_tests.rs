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
use serial_test::serial;
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

// ========================================================================
// Issue #8381: the #8355 seed's interaction with THIS branch's own probe
// ========================================================================

/// Install a fake `gh` for the #8381 fixtures that records every invocation
/// (space-joined argv, one line each) and answers the two forge probes the
/// reaper's checkpoint-less clean-exit branch makes after the seed:
///
/// * the #4123 open-linked-PR probe — `api graphql` plus its #5911 REST
///   timeline fallback, which either answer `graphql_prs` (whitespace-
///   separated open PR numbers, empty for "none open") or, when
///   `transports_down`, both exit non-zero, exactly the double-transport
///   failure that opens the #6058/#6788 fail-open window;
/// * the #4504 issue-state probe (`api repos/<o>/<r>/issues/<n>`), always
///   answering `OPEN`.
///
/// Ordering is load-bearing: the timeline arm must precede the generic
/// `repos/*` arm, whose glob also matches the timeline endpoint's path. The
/// timeline arm is keyed on `cross-referenced` (the open-linked-PR query's own
/// `--jq`, see `worktree_ops::gh::open_linked_pr_timeline_args`) rather than
/// the bare endpoint, so the branch's OTHER, unrelated `issues/<n>/timeline`
/// probes — the claim-age read and the label-history sample, both keyed on
/// `labeled` events — fall through untouched and are not miscounted as
/// open-PR round trips by [`open_pr_probe_calls`].
fn memo_seed_fake_gh(ws: &Path, log: &Path, graphql_prs: &str, transports_down: bool) -> PathBuf {
    let transports = if transports_down {
        "if [[ \"$1\" == \"api\" && \"$2\" == \"graphql\" ]]; then\n\
         printf 'gh: rate limit exceeded\\n' >&2\n\
         exit 1\n\
         fi\n\
         if [[ \"$1\" == \"api\" && \"$*\" == *cross-referenced* ]]; then\n\
         printf 'gh: rate limit exceeded\\n' >&2\n\
         exit 1\n\
         fi\n"
            .to_string()
    } else {
        format!(
            "if [[ \"$1\" == \"api\" && \"$*\" == *cross-referenced* ]]; then\n\
             printf '%s\\n' \"{graphql_prs}\"\n\
             exit 0\n\
             fi\n\
             {gql}",
            gql = fake_gh_graphql_arm(graphql_prs, 0),
        )
    };
    let fake_gh = ws.join("fake-gh-memo-seed.sh");
    let script = format!(
        "#!/usr/bin/env bash\n\
         printf '%s\\n' \"$*\" >> \"{log}\"\n\
         {transports}\
         if [[ \"$1\" == \"api\" && \"$2\" == repos/* ]]; then\n\
         printf '%s\\n' '{state}'\n\
         exit 0\n\
         fi\n\
         if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
         printf 'rjwalters/loom\\n'\n\
         exit 0\n\
         fi\n\
         exit 0\n",
        log = log.display(),
        state = state_probe_json("OPEN", false),
    );
    std::fs::write(&fake_gh, &script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }
    fake_gh
}

/// A registry over `ws` whose `gh` is [`memo_seed_fake_gh`], with the real
/// label/probe path enabled (`skip_label_flip = false` — the `exit_code ==
/// Some(0) && !skip_label_flip` gate on the open-PR probe is exactly what
/// these tests exercise).
fn memo_seed_registry(
    ws: &Path,
    graphql_prs: &str,
    transports_down: bool,
) -> (SweepRegistry, PathBuf) {
    let log = ws.join("gh-invocations.log");
    let fake_gh = memo_seed_fake_gh(ws, &log, graphql_prs, transports_down);
    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false;
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    (SweepRegistry::new(config), log)
}

/// Count the open-linked-PR probe's forge round trips in `log` — `gh api
/// graphql` (the closes-graph transport) plus its #5911 REST timeline
/// fallback, identified by the `cross-referenced` filter unique to that
/// query. Zero means the probe was served from the memo without touching the
/// forge.
fn open_pr_probe_calls(log: &Path) -> usize {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.starts_with("api graphql") || line.contains("cross-referenced"))
        .count()
}

/// Drive the production two-tick shape a #8355 memo seed needs, with a REAL
/// retained `Child` handle so `poll_liveness` yields `exit_code == Some(0)`
/// rather than the no-handle fallback's `None`.
///
/// The existing #8355 regression test (
/// [`reap_seeds_open_pr_memo_when_pr_produced_without_checkpoint`]) kills its
/// sweep by swapping in a dead pid, so its reap has no handle and therefore
/// `exit_code == None` — which never satisfies the `!skip_label_flip &&
/// exit_code == Some(0)` gate the open-PR probe sits behind. That is why it
/// cannot reach the interaction these two tests pin.
///
/// The child here blocks on `read` until its stdin is closed, so the test —
/// not a timing window — decides when it exits, and it exits **0**:
///
/// 1. tick 1 observes it alive with a `builder-done` checkpoint carrying
///    `pr_number`, which `sample_phase_transition` records (#4704);
/// 2. the checkpoint is deleted (the sweep skill's success-path deletion) and
///    stdin is closed, so the child exits 0;
/// 3. ticks run until the exit is observed, landing in the checkpoint-less
///    clean-exit branch with the seed and the probe both in play.
fn reap_clean_exit_that_produced_a_pr(registry: &mut SweepRegistry, issue: u32, pr: u32) {
    let mut child = Command::new("sh")
        .arg("-c")
        // `read` itself returns non-zero on EOF, so exit 0 explicitly: this
        // fixture is specifically a CLEAN exit.
        .arg("read line; exit 0")
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn stdin-gated fixture child");
    let pid = child.id();
    let stdin = child.stdin.take().expect("piped stdin");

    let sweep_id = format!("sweep-issue-{issue}-clean-exit");
    registry.entries.insert(
        sweep_id.clone(),
        SweepInfo {
            pgid: None,
            sweep_id: sweep_id.clone(),
            kind: SweepKind::Issue(issue),
            pid,
            token_name: "unknown".into(),
            runtime: "unknown".into(),
            runtime_source: None,
            log_path: registry.compute_log_path(issue),
            idempotency_key: None,
            // In the past so the checkpoint written below has an mtime inside
            // this run's window (#4009's `checkpoint_written_by_run` guard).
            started_at: Utc::now() - chrono::Duration::seconds(600),
            state: SweepState::Running,
            latest_phase: None,
            pr_number: None, // production never populates this — see #8355
            model: None,
            effort: None,
            depends_on: None,
            repo: None,
        },
    );
    // Retain the handle (mirrors `dispatch()`'s `self.children.insert`) so
    // `poll_liveness` reads the real exit status via `try_wait`.
    registry.children.insert(sweep_id.clone(), child);

    let checkpoint_dir = registry.config().checkpoint_dir();
    std::fs::create_dir_all(&checkpoint_dir).unwrap();
    let checkpoint = checkpoint_dir.join(format!("issue-{issue}.json"));
    std::fs::write(
        &checkpoint,
        format!(r#"{{"phase":"builder-done","issue":{issue},"pr_number":{pr}}}"#),
    )
    .unwrap();
    assert_eq!(registry.reap_once(), 0, "a live sweep must not be reaped on the first tick");
    assert_eq!(
        registry.sampled_pr_number(&sweep_id),
        Some(pr),
        "the reap tick must have sampled the checkpoint's pr_number (#4704) — \
         it is the production source the memo seed reads"
    );

    std::fs::remove_file(&checkpoint).unwrap();
    drop(stdin); // EOF -> the child returns from `read` and exits 0

    let mut ticks = 0;
    while registry.reap_once() == 0 {
        ticks += 1;
        assert!(
            ticks < 500,
            "the stdin-gated fixture child never exited after its stdin was closed"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let info = registry.get(&sweep_id).unwrap();
    assert!(
        matches!(info.state, SweepState::Exited { code: Some(0), .. }),
        "the retained Child handle must yield a real exit_code == Some(0) — the \
         gate the open-PR probe sits behind; got {:?}",
        info.state
    );
}

/// Issue #8381 (AC 1): the #8355 seed lands BEFORE this same branch's own
/// `probe_open_linked_pr` call, and a fresh memo is a full short circuit
/// (zero `gh` calls), so that probe is now served from the entry the seed
/// just wrote — it does not merely happen to agree with the forge, it never
/// asks the forge at all.
///
/// The fixture makes that observable rather than merely plausible: BOTH
/// probe transports are down (the #6058/#6788 double-failure window). A live
/// probe under this fixture can only reach `ProbeFailed`, which yields
/// `yielded_open_pr == false` and therefore `clear_dispatch_backoff`. The
/// seeded reap instead reaches `Open(pr)` and arms the #4485 ladder, with no
/// `api graphql` / timeline invocation recorded — and the control below (an
/// identical clean exit whose sweep produced no PR, so the memo is cold)
/// shows the same fixture DOES pay both round trips without a seed. That
/// contrast is the round trip the seed saves.
#[test]
#[serial]
fn reap_seeded_memo_serves_this_branchs_own_open_pr_probe() {
    let dir = tempdir().unwrap();
    let (mut registry, gh_log) = memo_seed_registry(dir.path(), "", true);

    reap_clean_exit_that_produced_a_pr(&mut registry, 8170, 8329);

    let memo = registry
        .fresh_open_pr_memo(8170, Utc::now())
        .expect("the reap must have seeded the open-PR memo from the sampled pr_number");
    assert_eq!(memo.pr, 8329);
    assert_eq!(
        open_pr_probe_calls(&gh_log),
        0,
        "this branch's own open-PR probe must be served from the memo the same \
         tick seeded — zero forge round trips; gh invocations: {:?}",
        std::fs::read_to_string(&gh_log).unwrap_or_default()
    );
    assert_eq!(
        registry.dispatch_failure_count(8170),
        1,
        "the memo-served probe must yield Open(pr) — the #6350 self-skip shape that \
         arms the per-issue dispatch backoff. A forge-served probe could not: both \
         transports are down in this fixture, so it would have reached ProbeFailed \
         and cleared the backoff instead"
    );
    assert!(
        registry
            .dispatch_backoff_remaining(8170, Utc::now())
            .is_some(),
        "the armed backoff must be in effect immediately after the reap"
    );

    // Control: the SAME fixture, an identical clean exit 0 — but its sweep
    // produced no PR, so nothing was sampled, nothing was seeded, and the
    // memo is cold. The probe therefore goes to the forge and pays both
    // transports (which then fail, this being an outage fixture).
    insert_clean_exit_running(&mut registry, 8171, 0);
    registry.reap_once();
    assert!(
        open_pr_probe_calls(&gh_log) > 0,
        "without a seeded memo the very same probe DOES hit the forge — this is \
         the round trip the seed saves; gh invocations: {:?}",
        std::fs::read_to_string(&gh_log).unwrap_or_default()
    );
    assert_eq!(
        registry.dispatch_failure_count(8171),
        0,
        "a cold-memo ProbeFailed is not a self-skip: it clears the backoff, \
         confirming the seeded issue's armed ladder came from the memo"
    );
}

/// Issue #8381 (AC 2): the divergent shape. When the sweep's PR is already
/// merged or closed by the time the reap runs — the ordinary full-lifecycle
/// Builder → Judge → Merge sweep, which merges its own PR before exiting —
/// the seed makes this branch's probe answer `Open(pr)` where a live probe
/// would have answered `NoneOpen`.
///
/// That flip is bounded and conservative, and this test pins both halves of
/// why:
///
/// * `yielded_open_pr` becomes `true`, so `record_dispatch_failure` arms the
///   #4485 backoff ladder where `clear_dispatch_backoff` used to run — a
///   deferred redispatch, capped by the memo's `OPEN_PR_MEMO_FRESH` window;
/// * `counted_failure = insta_crash || no_progress` deliberately EXCLUDES
///   `yielded_open_pr`, so the quarantine tally is untouched. The control
///   below shows the exemption is doing real work: the same reap without a
///   seed counts a `no_progress` failure against the issue.
#[test]
#[serial]
fn reap_seeded_memo_for_a_closed_pr_arms_backoff_without_quarantine() {
    let dir = tempdir().unwrap();
    // Transports are UP and answer "no open linked PR" — the PR merged (or
    // closed) between the sweep's `builder-done` checkpoint and this reap.
    let (mut registry, gh_log) = memo_seed_registry(dir.path(), "", false);
    assert_eq!(registry.quarantine_config().threshold, 3);

    reap_clean_exit_that_produced_a_pr(&mut registry, 8360, 8374);

    assert_eq!(
        registry
            .fresh_open_pr_memo(8360, Utc::now())
            .map(|memo| memo.pr),
        Some(8374),
        "the seed records an INFERRED-open entry even though the PR is no longer \
         open on the forge — the interaction this test pins (#8381)"
    );
    assert_eq!(
        open_pr_probe_calls(&gh_log),
        0,
        "the fresh seed short-circuits the probe, so the forge is never asked and \
         never gets to say NoneOpen; gh invocations: {:?}",
        std::fs::read_to_string(&gh_log).unwrap_or_default()
    );
    assert_eq!(
        registry.dispatch_failure_count(8360),
        1,
        "the inferred-open verdict arms the #4485 dispatch backoff ladder"
    );
    assert!(registry
        .dispatch_backoff_remaining(8360, Utc::now())
        .is_some());
    assert_eq!(
        registry.insta_crash_count(8360),
        0,
        "`counted_failure = insta_crash || no_progress` excludes `yielded_open_pr` — \
         an inferred-open self-skip must never charge the quarantine tally (#8381 \
         protects this exemption against regression)"
    );
    assert!(
        !registry.is_quarantined(8360),
        "arming the backoff must not also quarantine the issue"
    );

    // Control: same fixture, same clean exit 0, same forge answers — but no
    // sampled PR, so no seed. The probe reaches the forge's real NoneOpen and
    // the issue-state probe confirms the issue is open, so this IS a #4366
    // no-progress failure and DOES charge the quarantine tally. The only
    // difference between the two is the seed.
    insert_clean_exit_running(&mut registry, 8361, 0);
    registry.reap_once();
    assert!(
        open_pr_probe_calls(&gh_log) > 0,
        "the cold-memo control must actually reach the forge"
    );
    assert_eq!(
        registry.insta_crash_count(8361),
        1,
        "without the seed the identical reap counts a no-progress failure — which is \
         exactly what the seeded issue above is exempt from"
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
