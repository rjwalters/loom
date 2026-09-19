//! Tests for the startup (#3887), mid-build-death (#3895) and review-stall
//! (#3910) watchdogs.
//!
//! # If several `midbuild_*` tests fail together, read this first (#8170)
//!
//! This module is **more exposed than most** to the shared-process hazard
//! `loom-daemon/src/lib.rs` documents under "Test isolation convention"
//! (#4385): nearly every test here `Command::spawn`s real children — `git`
//! (repo fixture, dirty probe, `reset --hard`, `clean -fd`), `lsof`, a fake
//! `spawn-claude.sh`, a fake `gh`, a stand-in sweep process — while hundreds
//! of unrelated tests in the same binary call `env::set_var`/`remove_var`.
//! `spawn` snapshots the environ non-atomically, so a concurrent mutation can
//! hand one of those children a torn environment.
//!
//! The mid-build watchdog amplifies a single torn `git status` into a
//! module-wide failure, because `SweepRegistry::worktree_dirty` fails closed:
//! a `git` invocation that does not exit 0 reads as "not dirty", which makes
//! `midbuild_decision` return `Healthy`, which means **nothing** is recorded —
//! no recovery, no `midbuild_inuse` refusal, no `midbuild_gaveup`, no
//! `midbuild_lease_superseded`. So one flaked subprocess takes down every
//! recovery-path test *and* the positive-control leg of every
//! refuse-to-destroy test at once, while the pure-refusal tests pass. That
//! pattern — 6-8 `midbuild_*` failures under `cargo test --lib`, 90/90 green
//! when the module is re-run alone — is this, not a regression in the guards.
//!
//! Issue #8170 measured it directly: the module was run in its **own process**
//! three times while the rest of the suite ran concurrently in a second
//! process, saturating the same host (same `lsof`, same `git`, same disk, same
//! CPU). 90/90 passed on all three runs. Host-level interference — including
//! the operator's concurrent `git worktree` activity suspected in the report —
//! is therefore ruled out; the interference is intra-process.
//!
//! `#[serial]` cannot fix it (its lock is advisory and only binds *marked*
//! tests, while the mutating tests are unmarked), and neither can anything
//! inside this module: the shared resource is the process environment itself.
//! The structural fix is the one the repo already ships — **run the suite
//! under `cargo nextest run`, one process per test** (`.config/nextest.toml`,
//! which is what CI uses). What #8170 *did* change here is the part that is
//! fixable locally: [`make_dirty_git_worktree`] is now hermetic with respect
//! to the host's git config and asserts its own dirty post-condition, so a
//! torn or host-perturbed `git` surfaces at the fixture with an explanation
//! instead of as an unrelated-looking assertion five layers up.
//!
//! Do not "fix" a failure here by loosening an assertion. These are
//! refuse-to-destroy guards (#4449/#4556/#4564/#7612); their strictness is the
//! whole point.

use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::time::SystemTime;
use tempfile::tempdir;

/// The mid-build-death watchdog (#3895) recovery must still fire with a
/// backoff window armed — the recovery is latched to one attempt per issue,
/// so a backoff refusal would silently consume it and strand the sweep.
#[test]
fn midbuild_recovery_is_not_blocked_by_an_armed_backoff() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let mut reg = backoff_registry(ws, 60, 900);

    make_dirty_git_worktree(ws, 6055);
    insert_terminal_issue(&mut reg, "sweep-issue-6055-dead", 6055, None);
    // An earlier fast failure armed a live window for this very issue.
    reg.record_dispatch_failure(6055);
    assert!(reg.dispatch_backoff_remaining(6055, Utc::now()).is_some());

    let recovered = reg.midbuild_watchdog_once();
    assert_eq!(recovered, 1, "the bounded one-shot recovery still re-dispatches");
    assert!(reg.issue_has_active_sweep(6055));
    assert_eq!(
        reg.dispatch_failure_count(6055),
        0,
        "the watchdog released the window before dispatching"
    );
}

/// The mid-build-death watchdog (#3895) must not recover an issue whose
/// sweep is still alive — the 03:08:52Z re-dispatch in the #4275 timeline.
///
/// The dispatch-time guard alone would be **too late** here: this path
/// `git reset --hard`s the shared worktree and burns the single recovery
/// retry *before* it calls `dispatch`. So the refusal must happen up front,
/// and this test asserts exactly that — the uncommitted mid-build work
/// survives and the retry is not consumed.
///
/// Nothing holds the worktree (no `.loom-in-use`, no `index.lock`, no live
/// lock owner), so the #4449 live-use veto does *not* fire: the only thing
/// standing between the live sweep and a `git reset --hard` is #4556's
/// live-claim probe.
#[test]
fn midbuild_watchdog_does_not_recover_an_issue_with_a_live_claim() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let (mut reg, _record_log) = fixture_registry(ws);

    make_dirty_git_worktree(ws, 4562);
    insert_terminal_issue(&mut reg, "sweep-issue-4562-dead", 4562, None);
    // The live sweep's claim survives in the machine-level journal even
    // though this daemon believes sweep-…-dead is terminal — the exact
    // state a false-dead verdict leaves behind once it has released the
    // lock and reverted the label.
    let sweep = FakeSweep::spawn(4562);
    write_journal_entry(&reg, &ws.display().to_string(), 4562, sweep.pid());
    assert!(
        reg.worktree_in_use(4562).is_empty(),
        "precondition: the #4449 live-use veto must NOT be what refuses here"
    );

    assert_eq!(
        reg.midbuild_watchdog_once(),
        0,
        "no recovery while a live sweep claim exists for the issue"
    );
    assert!(
        ws.join(".loom/worktrees/issue-4562/dirty.txt").exists(),
        "the live sweep's uncommitted mid-build work MUST survive (#4556)"
    );
    assert!(
        !reg.midbuild_retried.contains(&4562),
        "a live-claim refusal must NOT consume the single recovery retry"
    );
    assert!(
        reg.midbuild_liveclaim.contains(&4562),
        "the refusal is recorded and logged once"
    );
    assert!(reg.entries.values().all(|i| i.state.is_terminal()), "no new sweep was created");
}

/// The inverse: once the live claim is gone, the same mid-build recovery
/// proceeds normally. Without this the guard could wedge the recovery path
/// permanently on a stale record.
#[test]
fn midbuild_watchdog_recovers_once_the_live_claim_is_gone() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let (mut reg, _record_log) = fixture_registry(ws);

    make_dirty_git_worktree(ws, 4566);
    insert_terminal_issue(&mut reg, "sweep-issue-4566-dead", 4566, None);
    {
        let sweep = FakeSweep::spawn(4566);
        write_journal_entry(&reg, &ws.display().to_string(), 4566, sweep.pid());
        assert_eq!(reg.midbuild_watchdog_once(), 0, "refused while the claim is live");
        assert!(reg.midbuild_liveclaim.contains(&4566));
    } // the stand-in sweep exits here
    assert!(
        reg.live_claim_evidence(4566).is_none(),
        "the journal record's pid is dead once the stand-in sweep exits"
    );

    assert_eq!(reg.midbuild_watchdog_once(), 1, "recovery resumes once the claim dies");
    assert!(!reg.midbuild_liveclaim.contains(&4566), "the log-once latch is cleared");
    assert!(
        reg.midbuild_retried.contains(&4566),
        "the retry is consumed by the real recovery"
    );
}

/// Ordering regression pin for the #4556 × #4602/#4564 interaction: the
/// live-claim probe MUST run **before** `claim_lock_for_midbuild`.
///
/// #4602 replaced the watchdog's read-only `lock_owned_by_other` probe with
/// a *mutating* takeover that rewrites `.loom/locks/issue-<N>/owner.json` to
/// this daemon's pid and a `midbuild-watchdog-…` sweep id. A "keep both
/// hunks" rebase that leaves the #4556 probe after that call compiles, and
/// every other #4556 test still passes — but it is wrong twice over:
///
/// 1. the probe's strongest leg (the live lock owner) is destroyed by the
///    very call it is meant to gate, since the daemon's argv is
///    `loom-daemon`, not `/loom:sweep <N>`; and
/// 2. the refusal path `continue`s without releasing, so the live sweep's
///    owner record stays clobbered — its own `release_lock_owned` then reads
///    `Superseded` and skips its label restore.
///
/// The fixture makes the takeover genuinely **eligible** (a leftover lock
/// naming the dead sweep, with a dead owner pid) so that neither the #4449
/// live-use veto nor the #4463 peer-owner refusal is what stops the
/// recovery, and leaves the live claim visible **only** through the journal
/// leg. Byte-comparing `owner.json` across the refused tick is what fails
/// under the inverted order.
#[test]
fn midbuild_live_claim_probe_runs_before_the_lock_takeover() {
    // Above every plausible `pid_max`, so `is_pid_alive` reports dead.
    const DEAD_OWNER_PID: u32 = 2_147_483_640;
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let (mut reg, _record_log) = fixture_registry(ws);

    make_dirty_git_worktree(ws, 4602);
    insert_terminal_issue(&mut reg, "sweep-issue-4602-dead", 4602, None);
    let lock = write_lock_owner(&reg, 4602, "sweep-issue-4602-dead", DEAD_OWNER_PID);
    let owner_path = lock.join("owner.json");
    let owner_before = std::fs::read_to_string(&owner_path).unwrap();

    let sweep = FakeSweep::spawn(4602);
    write_journal_entry(&reg, &ws.display().to_string(), 4602, sweep.pid());

    assert!(
        reg.worktree_in_use(4602).is_empty(),
        "precondition: the #4449 live-use veto must NOT be what refuses here \
             (the lock's owner pid is dead, so it is not live-use evidence)"
    );
    assert!(
        !reg.lock_owned_by_other(4602, "sweep-issue-4602-dead"),
        "precondition: the #4463 peer-owner probe must NOT be what refuses here — \
             the takeover is eligible, which is exactly what makes an ordering \
             regression observable"
    );

    assert_eq!(
        reg.midbuild_watchdog_once(),
        0,
        "the journal-visible live claim must refuse the recovery"
    );

    assert_eq!(
        std::fs::read_to_string(&owner_path).unwrap(),
        owner_before,
        "ORDERING REGRESSION: `claim_lock_for_midbuild` (#4602) rewrote the live \
             sweep's owner.json, which means the #4556 live-claim probe ran AFTER it. \
             The probe must come first — see the ORDERING IS LOAD-BEARING comment in \
             `midbuild_watchdog_once`'s Recover arm."
    );
    assert!(
        reg.midbuild_liveclaim.contains(&4602),
        "the refusal must be the live-claim one, logged once"
    );
    assert!(
        !reg.midbuild_retried.contains(&4602),
        "a live-claim refusal must NOT consume the single recovery retry"
    );
    assert!(
        ws.join(".loom/worktrees/issue-4602/dirty.txt").exists(),
        "the live sweep's uncommitted mid-build work MUST survive"
    );
}

/// The review-stall watchdog (#3910) must not re-dispatch either — the
/// 03:54:25Z re-dispatch in the #4275 timeline.
///
/// Unlike the mid-build path this one is *safe* to guard at dispatch time:
/// it cancels its own child (SIGTERM → grace → SIGKILL) first and takes no
/// destructive action on the worktree, so a refusal after the cancel costs
/// nothing. The guard therefore lives in the shared `dispatch` entry point,
/// where it also covers a claim held by a sweep this daemon never spawned.
#[test]
fn review_stall_watchdog_does_not_redispatch_an_issue_with_a_live_claim() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let (mut reg, _record_log) = fixture_registry(ws);
    let sweep = FakeSweep::spawn(4563);
    write_journal_entry(&reg, &ws.display().to_string(), 4563, sweep.pid());

    // The watchdog's re-dispatch is the only step that can create a second
    // sweep; assert it is refused for a live-claimed issue.
    let err = reg
        .dispatch(&SweepKind::Issue(4563), None, None, None, None)
        .unwrap_err();
    assert!(
        err.downcast_ref::<LiveClaimDispatchError>().is_some(),
        "the watchdogs' shared re-dispatch entry point must refuse; got: {err}"
    );
}

// --- watchdog_decision state machine ---

#[test]
fn watchdog_decision_progress_is_always_healthy() {
    // Progress observed ⇒ Healthy regardless of elapsed / retried.
    let t = Duration::from_secs(120);
    assert_eq!(
        watchdog_decision(Duration::from_secs(9999), t, true, false),
        WatchdogDecision::Healthy
    );
    assert_eq!(
        watchdog_decision(Duration::from_secs(9999), t, true, true),
        WatchdogDecision::Healthy
    );
}

#[test]
fn watchdog_decision_within_timeout_is_healthy() {
    let t = Duration::from_secs(120);
    assert_eq!(
        watchdog_decision(Duration::from_secs(119), t, false, false),
        WatchdogDecision::Healthy
    );
}

#[test]
fn watchdog_decision_hung_first_time_restarts() {
    let t = Duration::from_secs(120);
    assert_eq!(
        watchdog_decision(Duration::from_secs(121), t, false, false),
        WatchdogDecision::Restart
    );
}

#[test]
fn watchdog_decision_hung_after_retry_gives_up() {
    // Bounded: a second hang past the timeout does not restart again.
    let t = Duration::from_secs(120);
    assert_eq!(
        watchdog_decision(Duration::from_secs(500), t, false, true),
        WatchdogDecision::GiveUp
    );
}

// --- sweep_made_progress: filesystem probes ---

#[test]
fn sweep_made_progress_worktree_and_checkpoint_and_log() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (reg, _rec) = fixture_registry(ws);
    let log = ws.join("sweep.log");

    // Nothing yet ⇒ no progress.
    std::fs::write(&log, "==== loom-daemon dispatch: t sweep_id=s issue=7 ====\n[ts] spawn-claude: using OAuth account 'x' (mode=random)\n").unwrap();
    assert!(!reg.sweep_made_progress(7, &log));

    // A worktree ⇒ progress.
    let wt = ws.join(".loom").join("worktrees").join("issue-7");
    std::fs::create_dir_all(&wt).unwrap();
    assert!(reg.sweep_made_progress(7, &log));
    std::fs::remove_dir_all(&wt).unwrap();
    assert!(!reg.sweep_made_progress(7, &log));

    // A checkpoint ⇒ progress.
    let cp_dir = ws.join(".loom").join("sweep-checkpoint");
    std::fs::create_dir_all(&cp_dir).unwrap();
    std::fs::write(cp_dir.join("issue-7.json"), "{}").unwrap();
    assert!(reg.sweep_made_progress(7, &log));
    std::fs::remove_file(cp_dir.join("issue-7.json")).unwrap();
    assert!(!reg.sweep_made_progress(7, &log));

    // Log output past the header ⇒ progress.
    std::fs::write(
        &log,
        "==== loom-daemon dispatch: t sweep_id=s issue=7 ====\nBuilder: writing code\n",
    )
    .unwrap();
    assert!(reg.sweep_made_progress(7, &log));
}

// ===================================================================
// Mid-build-death watchdog (Issue #3895)
// ===================================================================

// --- midbuild_decision pure state machine ---

#[test]
fn midbuild_decision_no_dirty_worktree_is_healthy() {
    // No dirty worktree ⇒ nothing to recover, regardless of retry state.
    assert_eq!(midbuild_decision(false, false, false, false), MidbuildDecision::Healthy);
    assert_eq!(midbuild_decision(false, false, true, false), MidbuildDecision::Healthy);
}

#[test]
fn midbuild_decision_produced_pr_is_healthy() {
    // A dead sweep that produced a PR is a completed Builder, not a
    // mid-build death — never recovered even with a dirty worktree.
    assert_eq!(midbuild_decision(true, true, false, false), MidbuildDecision::Healthy);
}

#[test]
fn midbuild_decision_dirty_no_pr_first_time_recovers() {
    assert_eq!(midbuild_decision(true, false, false, false), MidbuildDecision::Recover);
}

#[test]
fn midbuild_decision_dirty_no_pr_after_retry_gives_up() {
    // Bounded: a second mid-build death gives up (never loops).
    assert_eq!(midbuild_decision(true, false, true, false), MidbuildDecision::GiveUp);
}

#[test]
fn midbuild_decision_in_use_worktree_is_never_recovered() {
    // #4449: a live holder vetoes the destructive path, and the veto sits
    // ABOVE the retry bookkeeping — it must win over both Recover and
    // GiveUp so a refusal never consumes the single recovery retry.
    assert_eq!(midbuild_decision(true, false, false, true), MidbuildDecision::InUse);
    assert_eq!(midbuild_decision(true, false, true, true), MidbuildDecision::InUse);
}

#[test]
fn midbuild_decision_in_use_is_irrelevant_when_not_dirty_or_pr_exists() {
    // The in-use flag must not manufacture work: a clean worktree or a
    // produced PR is still Healthy even while a session holds the worktree.
    assert_eq!(midbuild_decision(false, false, false, true), MidbuildDecision::Healthy);
    assert_eq!(midbuild_decision(true, true, false, true), MidbuildDecision::Healthy);
}

#[test]
fn worktree_dirty_and_clean_roundtrip() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (reg, _rec) = fixture_registry(ws);

    // No worktree ⇒ not dirty.
    assert!(!reg.worktree_dirty(70));

    // A worktree with an untracked file ⇒ dirty.
    make_dirty_git_worktree(ws, 70);
    assert!(reg.worktree_dirty(70));

    // Cleaning discards the untracked edit; the committed file survives.
    reg.clean_worktree(70).unwrap();
    assert!(!reg.worktree_dirty(70), "clean_worktree cleared the dirty state");
    assert!(ws.join(".loom/worktrees/issue-70/committed.txt").exists());
    assert!(!ws.join(".loom/worktrees/issue-70/dirty.txt").exists());
}

// --- midbuild_watchdog_once: detection + bounded recovery ---

#[test]
fn midbuild_recovers_dead_sweep_with_dirty_worktree_once() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, rec) = fixture_registry(ws);

    // A sweep that got into the Builder phase (dirty worktree) then its
    // child died (terminal Exited) without producing a PR.
    make_dirty_git_worktree(ws, 6001);
    insert_terminal_issue(&mut reg, "sweep-issue-6001-dead", 6001, None);

    // Detected + recovered: worktree cleaned, issue re-dispatched once.
    let recovered = reg.midbuild_watchdog_once();
    assert_eq!(recovered, 1, "mid-build death detected and re-dispatched");
    assert!(reg.midbuild_retried.contains(&6001), "issue marked recovered (bounded)");

    // Worktree was cleaned before the re-dispatch.
    assert!(!ws.join(".loom/worktrees/issue-6001/dirty.txt").exists());
    assert!(ws.join(".loom/worktrees/issue-6001/committed.txt").exists());

    // A fresh sweep child actually ran, and an active entry now exists.
    assert!(
        wait_for_contents(&rec, "/loom:sweep 6001", 5000),
        "fake spawn ran for the re-dispatch"
    );
    assert!(reg.issue_has_active_sweep(6001), "a fresh sweep is now active for the issue");
}

#[test]
fn midbuild_recovery_is_bounded_to_one() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, _rec) = fixture_registry(ws);

    // The issue already used its single recovery; the re-dispatched sweep
    // ALSO died mid-build (dirty worktree, terminal, no PR).
    make_dirty_git_worktree(ws, 6002);
    insert_terminal_issue(&mut reg, "sweep-issue-6002-dead2", 6002, None);
    reg.midbuild_retried.insert(6002);

    let recovered = reg.midbuild_watchdog_once();
    assert_eq!(recovered, 0, "bounded: a second mid-build death is not re-dispatched");
    assert!(reg.midbuild_gaveup.contains(&6002), "give-up recorded for the operator");
    // The worktree is left intact for operator inspection (not cleaned).
    assert!(ws.join(".loom/worktrees/issue-6002/dirty.txt").exists());
}

#[test]
fn midbuild_skips_sweep_that_produced_a_pr() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, _rec) = fixture_registry(ws);

    // A dead sweep with a dirty worktree BUT a PR recorded is a completed
    // Builder, not a mid-build death — never recovered.
    make_dirty_git_worktree(ws, 6004);
    insert_terminal_issue(&mut reg, "sweep-issue-6004-pr", 6004, Some(4321));

    assert_eq!(reg.midbuild_watchdog_once(), 0, "a sweep that produced a PR is not recovered");
    assert!(!reg.midbuild_retried.contains(&6004));
}

#[test]
fn midbuild_leaves_clean_worktree_alone() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, _rec) = fixture_registry(ws);

    // A dead sweep whose worktree exists but is CLEAN (committed, no
    // uncommitted edits) is not a "dirty mid-build death" and is left alone.
    let wt = make_dirty_git_worktree(ws, 6005);
    std::fs::remove_file(wt.join("dirty.txt")).unwrap();
    assert!(!reg.worktree_dirty(6005), "precondition: worktree is clean");
    insert_terminal_issue(&mut reg, "sweep-issue-6005-clean", 6005, None);

    assert_eq!(reg.midbuild_watchdog_once(), 0, "a clean worktree is not recovered");
    assert!(!reg.midbuild_retried.contains(&6005));
}

#[test]
fn midbuild_token_gate_defers_when_pool_exhausted_then_proceeds() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, _rec) = fixture_registry(ws);

    make_dirty_git_worktree(ws, 6003);
    insert_terminal_issue(&mut reg, "sweep-issue-6003-dead", 6003, None);

    // Every account exhausted/blocked ⇒ the pre-flight gate defers WITHOUT
    // consuming the single retry.
    let tokens = ws.join(".loom").join("tokens");
    std::fs::create_dir_all(&tokens).unwrap();
    std::fs::write(tokens.join(".ranking"), "agent-1|exhausted\nagent-2|blocked\n").unwrap();

    assert_eq!(
        reg.midbuild_watchdog_once(),
        0,
        "token gate defers re-dispatch when every account is exhausted/blocked"
    );
    assert!(
        !reg.midbuild_retried.contains(&6003),
        "a deferral must NOT consume the single recovery"
    );
    // The dirty worktree is untouched while deferred.
    assert!(ws.join(".loom/worktrees/issue-6003/dirty.txt").exists());

    // Once a healthy account appears, recovery proceeds on the next tick.
    std::fs::write(tokens.join(".ranking"), "agent-1|exhausted\nagent-2|available\n").unwrap();
    assert_eq!(
        reg.midbuild_watchdog_once(),
        1,
        "recovery proceeds once a healthy account is available"
    );
    assert!(reg.midbuild_retried.contains(&6003));
    assert!(
        !ws.join(".loom/worktrees/issue-6003/dirty.txt").exists(),
        "worktree cleaned on recovery"
    );
}

#[test]
fn midbuild_refuses_to_wipe_worktree_held_by_in_use_marker() {
    // The #4449 incident shape: the daemon's tracked sweep really did die
    // (terminal, no PR) but a SEPARATE live session is still using the
    // worktree. A `.loom-in-use` marker is the explicit form of that signal.
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, _rec) = fixture_registry(ws);

    let wt = make_dirty_git_worktree(ws, 6101);
    insert_terminal_issue(&mut reg, "sweep-issue-6101-dead", 6101, None);
    std::fs::write(
        wt.join(".loom-in-use"),
        r#"{"shepherd_task_id": "recovery-doctor", "pid": 4321}"#,
    )
    .unwrap();

    assert!(!reg.worktree_in_use(6101).is_empty(), "marker is detected as a live holder");
    assert_midbuild_refused(&mut reg, ws, 6101, "a .loom-in-use marker names a live session");

    // Once the holder releases the worktree, the legitimate dead-sweep
    // recovery still works — the veto defers, it does not disable.
    std::fs::remove_file(wt.join(".loom-in-use")).unwrap();
    assert_eq!(reg.midbuild_watchdog_once(), 1, "recovery resumes once the holder releases");
    assert!(reg.midbuild_retried.contains(&6101));
    assert!(!reg.midbuild_inuse.contains(&6101), "the log-once latch is cleared");
    assert!(!wt.join("dirty.txt").exists(), "worktree cleaned on the real recovery");
}

#[test]
fn midbuild_refuses_to_wipe_worktree_with_git_operation_in_flight() {
    // The precise window #4449 lost work in: a `git commit` was mid-write,
    // so git held index.lock. Never reset a worktree in that state.
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, _rec) = fixture_registry(ws);

    let wt = make_dirty_git_worktree(ws, 6102);
    insert_terminal_issue(&mut reg, "sweep-issue-6102-dead", 6102, None);
    let index_lock = git_index_lock_path(&wt).expect("index.lock path resolves for a real repo");
    std::fs::write(&index_lock, "").unwrap();

    assert_midbuild_refused(&mut reg, ws, 6102, "a git index.lock write is in flight");

    // Committing finishes (lock released) ⇒ recovery is available again.
    std::fs::remove_file(&index_lock).unwrap();
    assert_eq!(reg.midbuild_watchdog_once(), 1, "recovery resumes once index.lock clears");
}

#[test]
fn midbuild_refuses_to_wipe_worktree_with_live_claim_lock_owner() {
    // A claim-lock whose owner PID is ALIVE means some session (daemon or
    // not) still owns this issue — never reset its worktree underneath it.
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, _rec) = fixture_registry(ws);

    make_dirty_git_worktree(ws, 6103);
    insert_terminal_issue(&mut reg, "sweep-issue-6103-dead", 6103, None);
    let lock = reg.config.locks_dir().join("issue-6103");
    std::fs::create_dir_all(&lock).unwrap();
    // Our own PID is trivially alive — stands in for the live holder.
    std::fs::write(
            lock.join("owner.json"),
            format!(
                r#"{{"issue": 6103, "owner_pid": {}, "acquired_at": "{}", "sweep_id": "manual-session"}}"#,
                std::process::id(),
                Utc::now().to_rfc3339()
            ),
        )
        .unwrap();

    assert_midbuild_refused(&mut reg, ws, 6103, "a live claim-lock owner still holds the issue");
}

#[test]
fn midbuild_ignores_stale_claim_lock_with_dead_owner() {
    // The inverse guard: the dead sweep's OWN claim-lock, left behind
    // because the reaper never released it, must not wedge the legitimate
    // dead-sweep recovery path forever.
    //
    // The lock's `sweep_id` is deliberately the dead entry's own id. A lock
    // naming a *different* sweep is a separate, pre-existing refusal
    // (`lock_owned_by_other`, #4463: a newer sweep superseded this one) that
    // fails closed on the id comparison alone and is covered by its own
    // tests. This test isolates the #4449 live-use veto: a dead owner PID is
    // not live-use evidence, so recovery proceeds.
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, _rec) = fixture_registry(ws);

    make_dirty_git_worktree(ws, 6104);
    insert_terminal_issue(&mut reg, "sweep-issue-6104-dead", 6104, None);
    let lock = reg.config.locks_dir().join("issue-6104");
    std::fs::create_dir_all(&lock).unwrap();
    std::fs::write(
            lock.join("owner.json"),
            format!(
                r#"{{"issue": 6104, "owner_pid": 2147483640, "acquired_at": "{}", "sweep_id": "sweep-issue-6104-dead"}}"#,
                Utc::now().to_rfc3339()
            ),
        )
        .unwrap();

    assert!(
        reg.worktree_in_use(6104).is_empty(),
        "a dead owner's lock is not live-use evidence"
    );
    assert_eq!(reg.midbuild_watchdog_once(), 1, "a stale lock does not block recovery");
    assert!(!ws.join(".loom/worktrees/issue-6104/dirty.txt").exists());
}

// --- #4564: the clean runs while the watchdog OWNS the issue lock --------

#[test]
fn midbuild_refuses_to_clean_worktree_when_a_peer_owns_the_issue_lock() {
    // A cross-instance sweep holds issue #6106's claim lock. Its owner PID
    // is deliberately DEAD so the #4449 live-use veto contributes nothing
    // (`worktree_in_use` ignores dead owners) — the ONLY thing that can stop
    // the destructive arm here is the ownership check in
    // `claim_lock_for_midbuild`. This is the shape the pre-#4564 read-only
    // probe could lose to: a peer holding the claim while the watchdog
    // `git reset --hard`s the worktree it just claimed.
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, _rec) = fixture_registry(ws);

    make_dirty_git_worktree(ws, 6106);
    insert_terminal_issue(&mut reg, "sweep-issue-6106-dead", 6106, None);
    let lock = write_lock_owner(&reg, 6106, "sweep-issue-6106-peer", 2_147_483_640);

    assert!(
        reg.worktree_in_use(6106).is_empty(),
        "precondition: a dead owner PID is not live-use evidence, so only the \
             lock-ownership check can refuse here"
    );

    assert_eq!(reg.midbuild_watchdog_once(), 0, "a peer-owned claim blocks the recovery");
    assert!(
        ws.join(".loom/worktrees/issue-6106/dirty.txt").exists(),
        "the peer's uncommitted work MUST survive (#4564)"
    );
    assert!(
        !reg.midbuild_retried.contains(&6106),
        "a refusal must NOT consume the single recovery retry"
    );

    // The peer's lock is left exactly as it was — the watchdog neither
    // released it nor took it over.
    let owner: LockOwner =
        serde_json::from_str(&std::fs::read_to_string(lock.join("owner.json")).unwrap()).unwrap();
    assert_eq!(owner.sweep_id, "sweep-issue-6106-peer", "the peer's claim is untouched");
}

#[test]
fn midbuild_claim_holds_the_issue_lock_across_the_clean() {
    // The structural fix for the probe→clean TOCTOU (#4564): the watchdog no
    // longer merely *reads* the lock before cleaning, it *holds* it. This
    // exercises `claim_lock_for_midbuild` directly, because once the claim
    // and the clean are one operation there is no longer an in-between
    // moment a test could inject a peer into — the invariant to pin down is
    // "while the watchdog holds the claim, a peer cannot acquire it".
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (reg, _rec) = fixture_registry(ws);

    // 1. Free lock ⇒ claimed via the POSIX-atomic `mkdir` path.
    let held = reg
        .claim_lock_for_midbuild(6107, "sweep-issue-6107-dead")
        .expect("a free claim lock is acquired");
    assert_eq!(held, "midbuild-watchdog-sweep-issue-6107-dead");

    // 2. THE POINT: a peer racing in during the clean window now loses.
    //    Before #4564 the probe had already returned "free" and the peer's
    //    `acquire_lock` would have succeeded, handing it a live claim on a
    //    worktree the watchdog was about to reset.
    assert!(
        reg.acquire_lock(6107, "sweep-issue-6107-peer").is_err(),
        "a peer cannot acquire the claim while the watchdog holds it (#4564)"
    );

    // 3. The claim is released under the WATCHDOG's id so `dispatch` can
    //    re-acquire it under its own fresh sweep id.
    assert_eq!(reg.release_lock_owned(6107, &held), LockReleaseOutcome::Released);
    assert!(reg.acquire_lock(6107, "sweep-issue-6107-peer").is_ok(), "released ⇒ acquirable");

    // 4. A claim already held by a DIFFERENT sweep is refused outright.
    assert!(
        reg.claim_lock_for_midbuild(6107, "sweep-issue-6107-dead")
            .is_none(),
        "a peer-owned claim is never taken over"
    );

    // 5. The dead sweep's OWN stale claim is taken over IN PLACE (the dir is
    //    never freed and re-created, which would re-open the very window
    //    being closed) — and remains un-acquirable by a peer throughout.
    write_lock_owner(&reg, 6108, "sweep-issue-6108-dead", 2_147_483_640);
    let held = reg
        .claim_lock_for_midbuild(6108, "sweep-issue-6108-dead")
        .expect("the dead sweep's own stale claim is taken over");
    assert_eq!(held, "midbuild-watchdog-sweep-issue-6108-dead");
    let owner: LockOwner = serde_json::from_str(
        &std::fs::read_to_string(reg.config.locks_dir().join("issue-6108/owner.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(owner.sweep_id, held, "owner.json now records the watchdog as the holder");
    assert_eq!(owner.owner_pid, std::process::id(), "…with a live owner PID");
    assert!(
        reg.acquire_lock(6108, "sweep-issue-6108-peer").is_err(),
        "the takeover never leaves the lock momentarily free"
    );
}

#[test]
fn midbuild_releases_its_own_claim_before_re_dispatching() {
    // The watchdog's claim must be handed off, not leaked: a lock left
    // behind would fail the re-dispatch on a collision AND (owned by the
    // live daemon PID) wedge every later tick behind the #4449 live-claim
    // veto. Start from the dead sweep's own stale lock so the takeover path
    // — not the plain `mkdir` path — is the one exercised.
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, _rec) = fixture_registry(ws);

    make_dirty_git_worktree(ws, 6109);
    insert_terminal_issue(&mut reg, "sweep-issue-6109-dead", 6109, None);
    write_lock_owner(&reg, 6109, "sweep-issue-6109-dead", 2_147_483_640);

    assert_eq!(reg.midbuild_watchdog_once(), 1, "recovery proceeds and re-dispatches");
    assert!(!ws.join(".loom/worktrees/issue-6109/dirty.txt").exists(), "worktree cleaned");

    // The lock now belongs to the freshly dispatched sweep — proof the
    // watchdog released its own claim before dispatching (`dispatch` would
    // otherwise have failed on the lock collision).
    let owner: LockOwner = serde_json::from_str(
        &std::fs::read_to_string(reg.config.locks_dir().join("issue-6109/owner.json")).unwrap(),
    )
    .unwrap();
    assert!(
        owner.sweep_id.starts_with("sweep-issue-6109-"),
        "the re-dispatched sweep owns the claim, got {}",
        owner.sweep_id
    );
    assert!(
        !owner.sweep_id.starts_with("midbuild-watchdog-"),
        "the watchdog's transient claim must not be left behind"
    );
}

#[test]
fn midbuild_refuses_to_wipe_worktree_with_live_process_cwd_inside() {
    // The signal that would have saved the #4449 session with no cooperation
    // from it at all: a live process whose cwd is inside the worktree.
    // `find_processes_using_directory` degrades to an empty list on hosts
    // where it cannot probe (no /proc, no lsof), so self-skip there rather
    // than assert something the host cannot express.
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, _rec) = fixture_registry(ws);

    let wt = make_dirty_git_worktree(ws, 6105);
    insert_terminal_issue(&mut reg, "sweep-issue-6105-dead", 6105, None);

    let mut child = Command::new("sleep")
        .arg("30")
        .current_dir(&wt)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn a live process with cwd inside the worktree");

    // The child's `chdir` happens after `fork`, so poll briefly rather than
    // read the probe once and race it.
    let mut detected = Vec::new();
    for _ in 0..40 {
        detected = crate::worktree_ops::safety::find_processes_using_directory(&wt);
        if !detected.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if detected.is_empty() {
        // Probe unavailable on this host — nothing to assert; don't leak.
        let _ = child.kill();
        let _ = child.wait();
        return;
    }
    assert!(
        detected.contains(&child.id()),
        "the probe found the spawned holder: {detected:?}"
    );

    assert_midbuild_refused(&mut reg, ws, 6105, "a live process has its cwd in the worktree");

    let _ = child.kill();
    let _ = child.wait();
}

/// Issue #7466: the cwd-only signal above misses a process that opens a
/// file inside the worktree via an **absolute path** while its own cwd
/// sits elsewhere entirely — a build tool that writes to a path it was
/// handed on the command line, never `chdir()`ing there first. This
/// pins that `worktree_in_use()` (and therefore the mid-build watchdog's
/// destructive-reset veto) now catches it too.
#[test]
fn midbuild_refuses_to_wipe_worktree_with_absolute_path_writer_cwd_outside() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, _rec) = fixture_registry(ws);

    let wt = make_dirty_git_worktree(ws, 6110);
    insert_terminal_issue(&mut reg, "sweep-issue-6110-dead", 6110, None);
    let elsewhere = tempdir().unwrap();
    let target_file = wt.join("output.txt");

    // cwd is `elsewhere` (never inside the worktree at all), but a file
    // inside the worktree is opened via an absolute path and held open.
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(format!("exec 3>'{}' && sleep 300", target_file.display()))
        .current_dir(elsewhere.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn an absolute-path writer with cwd outside the worktree");

    let mut detected = Vec::new();
    for _ in 0..40 {
        detected = crate::worktree_ops::safety::find_processes_using_directory(&wt);
        if detected.contains(&child.id()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if detected.is_empty() {
        // Probe unavailable on this host (no /proc, no lsof) — nothing
        // to assert; don't leak the child.
        let _ = child.kill();
        let _ = child.wait();
        return;
    }
    assert!(
        detected.contains(&child.id()),
        "the probe found the absolute-path writer despite its cwd being \
             outside the worktree: {detected:?}"
    );

    assert_midbuild_refused(
        &mut reg,
        ws,
        6110,
        "an absolute-path writer holds a file open inside the worktree, cwd notwithstanding",
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn midbuild_ignores_running_sweeps() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, _rec) = fixture_registry(ws);

    // A still-Running sweep (not terminal) is never a mid-build-death
    // candidate, even with a dirty worktree.
    make_dirty_git_worktree(ws, 6006);
    reg.entries.insert(
        "sweep-issue-6006-live".to_string(),
        SweepInfo {
            pgid: None,
            sweep_id: "sweep-issue-6006-live".to_string(),
            kind: SweepKind::Issue(6006),
            pid: 2_147_483_640,
            token_name: "unknown".into(),
            runtime: "unknown".into(),
            runtime_source: None,
            log_path: reg.compute_log_path(6006),
            idempotency_key: None,
            started_at: Utc::now(),
            state: SweepState::Running,
            latest_phase: None,
            pr_number: None,
            model: None,
            effort: None,
            depends_on: None,
            repo: None,
        },
    );

    assert_eq!(reg.midbuild_watchdog_once(), 0, "a Running sweep is not a mid-build death");
    assert!(!reg.midbuild_retried.contains(&6006));
}

// --- #7612: forge-lease ownership fence -------------------------------
//
// A live in-session sweep (B) redispatches an issue whose earlier
// dispatch (A) crashed. B publishes a fresh forge lease and starts
// editing the shared worktree BEFORE A's own delayed mid-build-death
// watchdog tick fires. None of the #4449/#4556/#4564 purely-local vetoes
// can see B — it never wrote `.loom/locks/issue-<N>/`, whether it runs on
// this host under a different sweep id or on a completely different
// host. `midbuild_lease_veto` (backed by the forge lease record) is the
// only signal that can catch it.

/// Same-host, different-`sweep_id` variant of the #7612 incident: the
/// freshest lease on the issue names a sweep other than the dying
/// dispatch, published under THIS host's own opaque id. The destructive
/// reset/clean/redispatch must not fire, and B's dirty edit must survive
/// untouched.
#[test]
#[serial]
fn midbuild_refuses_when_a_fresh_lease_names_a_different_sweep_same_host() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let issue = 8801;
    let dead_sweep_id = "sweep-issue-8801-1789334653";
    let live_sweep_id = "sweep-20260913T214322Z-31447-0502586b";
    let host = SweepRegistry::new(SweepRegistryConfig::new(ws.to_path_buf())).published_host_id();
    let now = Utc::now();
    let comments = format!(
        "{{\"id\":1,\"created_at\":\"{t}\",\"updated_at\":\"{t}\",\"body\":\"<!-- \
             loom:lease host={host} sweep={live_sweep_id} -->\"}}",
        t = now.to_rfc3339(),
    );
    let mut reg = fixture_registry_with_lease_gh(ws, &comments, 0);

    make_dirty_git_worktree(ws, issue);
    insert_terminal_issue(&mut reg, dead_sweep_id, issue, None);

    assert_eq!(
        reg.midbuild_watchdog_once(),
        0,
        "a fresh lease naming a different, live sweep must refuse the destructive \
             reset/clean/redispatch (#7612)"
    );
    assert!(
        ws.join(format!(".loom/worktrees/issue-{issue}/dirty.txt"))
            .exists(),
        "the newer owner's dirty, uncommitted work MUST survive (#7612)"
    );
    assert!(
        !reg.midbuild_retried.contains(&issue),
        "a lease-supersession refusal must NOT consume the single recovery retry"
    );
    assert!(
        reg.midbuild_lease_superseded.contains(&issue),
        "the refusal is recorded (and logged once)"
    );
    assert!(
        reg.entries.values().all(|i| i.state.is_terminal()),
        "no re-dispatch (new sweep) was created"
    );
}

/// Different-host variant of the same race: the freshest lease names a
/// host that is demonstrably not this one. The fence must refuse
/// identically — it never compares the lease's `host` against this
/// daemon's own identity, only the `sweep_id` against the dying
/// dispatch's.
#[test]
#[serial]
fn midbuild_refuses_when_a_fresh_lease_names_a_different_sweep_different_host() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let issue = 8802;
    let dead_sweep_id = "sweep-issue-8802-1789334653";
    let live_sweep_id = "sweep-20260913T214322Z-99999-abcdef01";
    let other_host = "host-d9142cf3";
    let now = Utc::now();
    let comments = format!(
        "{{\"id\":1,\"created_at\":\"{t}\",\"updated_at\":\"{t}\",\"body\":\"<!-- \
             loom:lease host={other_host} sweep={live_sweep_id} -->\"}}",
        t = now.to_rfc3339(),
    );
    let mut reg = fixture_registry_with_lease_gh(ws, &comments, 0);

    make_dirty_git_worktree(ws, issue);
    insert_terminal_issue(&mut reg, dead_sweep_id, issue, None);

    assert_eq!(
        reg.midbuild_watchdog_once(),
        0,
        "a fresh lease on a DIFFERENT HOST must refuse the destructive \
             reset/clean/redispatch exactly like a same-host peer (#7612)"
    );
    assert!(
        ws.join(format!(".loom/worktrees/issue-{issue}/dirty.txt"))
            .exists(),
        "the newer, cross-host owner's dirty work MUST survive (#7612)"
    );
    assert!(!reg.midbuild_retried.contains(&issue));
    assert!(reg.midbuild_lease_superseded.contains(&issue));
}

/// An unreadable lease probe (non-zero `gh api` exit) is ambiguous, not
/// evidence of anything — and #7612 deliberately fails CLOSED here,
/// unlike every fail-open forge probe elsewhere in this module, because
/// the operation being gated is an irreversible `git reset --hard` +
/// `git clean -fd`.
#[test]
#[serial]
fn midbuild_refuses_when_the_lease_read_fails() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let issue = 8803;
    let dead_sweep_id = "sweep-issue-8803-dead";
    let mut reg = fixture_registry_with_lease_gh(ws, "boom", 1);

    make_dirty_git_worktree(ws, issue);
    insert_terminal_issue(&mut reg, dead_sweep_id, issue, None);

    assert_eq!(
        reg.midbuild_watchdog_once(),
        0,
        "an unreadable lease probe must fail CLOSED (never destroy on ambiguity) (#7612)"
    );
    assert!(ws
        .join(format!(".loom/worktrees/issue-{issue}/dirty.txt"))
        .exists());
    assert!(!reg.midbuild_retried.contains(&issue));
    assert!(reg.midbuild_lease_superseded.contains(&issue));
}

/// The complementary case: no lease record exists at all (a verified
/// empty read). Absence of evidence is not evidence of a newer owner —
/// the veto must be a no-op so #3895's classic "genuinely dead, nobody
/// else has claimed it" recovery is unaffected.
#[test]
#[serial]
fn midbuild_lease_veto_proceeds_when_no_lease_found() {
    let dir = tempdir().unwrap();
    let reg = fixture_registry_with_lease_gh(dir.path(), "", 0);
    assert_eq!(
        reg.midbuild_lease_veto(9001, "sweep-dead"),
        None,
        "no lease evidence at all must never manufacture a refusal"
    );
}

/// The freshest lease belongs to the dying dispatch itself (its own,
/// never-superseded claim) — not a newer owner, so the veto must not
/// fire.
#[test]
#[serial]
fn midbuild_lease_veto_proceeds_when_the_freshest_lease_is_the_dead_sweeps_own() {
    let dir = tempdir().unwrap();
    let host =
        SweepRegistry::new(SweepRegistryConfig::new(dir.path().to_path_buf())).published_host_id();
    let now = Utc::now();
    let comments = format!(
        "{{\"id\":1,\"created_at\":\"{t}\",\"updated_at\":\"{t}\",\"body\":\"<!-- \
             loom:lease host={host} sweep=sweep-dead -->\"}}",
        t = now.to_rfc3339(),
    );
    let reg = fixture_registry_with_lease_gh(dir.path(), &comments, 0);
    assert_eq!(reg.midbuild_lease_veto(9002, "sweep-dead"), None);
}

/// The freshest lease names a different sweep, but it stopped being
/// renewed long past the TTL — a genuinely abandoned claim, not a live
/// competing owner. The veto must not fire.
#[test]
#[serial]
fn midbuild_lease_veto_proceeds_when_the_freshest_lease_has_expired() {
    let dir = tempdir().unwrap();
    let host =
        SweepRegistry::new(SweepRegistryConfig::new(dir.path().to_path_buf())).published_host_id();
    let ttl = crate::claim_reconciliation::resolve_lease_ttl_minutes();
    #[allow(clippy::cast_possible_truncation)]
    let stale = Utc::now() - chrono::Duration::minutes(ttl as i64 + 5);
    let comments = format!(
        "{{\"id\":1,\"created_at\":\"{t}\",\"updated_at\":\"{t}\",\"body\":\"<!-- \
             loom:lease host={host} sweep=sweep-other -->\"}}",
        t = stale.to_rfc3339(),
    );
    let reg = fixture_registry_with_lease_gh(dir.path(), &comments, 0);
    assert_eq!(
        reg.midbuild_lease_veto(9003, "sweep-dead"),
        None,
        "an expired lease is an abandoned claim, not a live competing owner"
    );
}

/// Every `fixture_registry`-based midbuild test (skip_label_flip = true)
/// relies on the veto being a complete no-op — pin that explicitly so a
/// future change to the skip-condition is caught here, not by a wave of
/// unrelated test failures elsewhere in this file.
#[test]
fn midbuild_lease_veto_is_a_noop_when_forge_interaction_is_disabled() {
    let dir = tempdir().unwrap();
    let (reg, _rec) = fixture_registry(dir.path());
    assert_eq!(reg.midbuild_lease_veto(1, "sweep-dead"), None);
}

#[test]
fn watchdog_restarts_hung_sweep_once_then_gives_up() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let mut reg = hung_child_registry(ws);

    // 1. Dispatch a hung sweep for issue 4242.
    let out = reg
        .dispatch(&SweepKind::Issue(4242), None, None, None, None)
        .unwrap();
    assert!(
        wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS),
        "hung fixture child should start"
    );
    let first_id = out.sweep_id.clone();

    // 2. Healthy while inside the timeout window.
    assert_eq!(reg.watchdog_once(Duration::from_secs(120)), 0, "a fresh sweep is not disturbed");

    // 3. Backdate so it looks hung, then run the watchdog.
    backdate(&mut reg, &first_id, 600);
    let restarts = reg.watchdog_once(Duration::from_secs(60));
    assert_eq!(restarts, 1, "the hung sweep is auto-restarted once");
    assert!(reg.watchdog_retried.contains(&4242), "issue marked retried (bounded)");

    // A fresh Running sweep now exists for the issue (the re-dispatch).
    // Note: `generate_sweep_id` is second-granular, so within this fast
    // test the re-dispatched id may coincide with the original — in
    // production the watchdog fires ≥120s later, so ids differ. Either way,
    // the registry holds exactly one Running entry for the issue again.
    let _ = first_id;
    let second_id = running_issue_sweep_id(&reg, 4242).expect("a fresh sweep was re-dispatched");

    // 4. Backdate the NEW sweep too; the watchdog must NOT restart again
    //    (bounded) — it gives up instead.
    backdate(&mut reg, &second_id, 600);
    let restarts2 = reg.watchdog_once(Duration::from_secs(60));
    assert_eq!(restarts2, 0, "bounded: never a second auto-restart");
    assert!(reg.watchdog_gaveup.contains(&4242), "give-up recorded for the issue");
    // The second sweep is still running (left for the operator).
    assert!(running_issue_sweep_id(&reg, 4242).is_some());

    // Cleanup: cancel the lingering hung child.
    if let Some(id) = running_issue_sweep_id(&reg, 4242) {
        let _ = reg.cancel(&id, Duration::from_secs(2));
    }
}

/// Issue #5302: `WatchdogDecision::GiveUp` must surface beyond the
/// daemon log — a forge comment on the issue — exactly once per issue.
/// Reuses the same dedup fixture shape as
/// `watchdog_restarts_hung_sweep_once_then_gives_up` above (drive a real
/// hung sweep through its one bounded auto-restart to a genuine
/// give-up), but with real `gh`/label-flip wiring enabled (fake `gh`
/// binary, `skip_label_flip = false`) so the forge comment call is
/// observable, and asserts (a) exactly one `gh issue comment` call fires
/// on the tick that reaches `GiveUp`, carrying the give-up marker, and
/// (b) several further ticks that keep observing the same already-given-up
/// state post NO additional comments — the dedup is per-issue, not
/// per-tick.
#[test]
#[serial]
fn watchdog_gaveup_posts_forge_comment_exactly_once() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    touch_sweep_command(ws);

    let gh_log = ws.join("gh-invocations.log");
    let fake_gh = install_fake_gh(ws, &gh_log, "", 0);

    let scripts_dir = ws.join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let fake_spawn = scripts_dir.join("spawn-claude.sh");
    std::fs::write(
        &fake_spawn,
        "#!/usr/bin/env bash\n\
             echo \"spawn-claude: using OAuth account 'faketok' (mode=random)\"\n\
             sleep 30\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&fake_spawn).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_spawn, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_spawn) {
        let _ = f.sync_all();
    }

    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.spawn_bin = Some(fake_spawn);
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false; // exercise the real comment-post path
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    let mut reg = SweepRegistry::new(config);

    // 1. Dispatch a hung sweep for issue 5302.
    let out = reg
        .dispatch(&SweepKind::Issue(5302), None, None, None, None)
        .unwrap();
    assert!(
        wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS),
        "hung fixture child should start"
    );
    let first_id = out.sweep_id.clone();

    // 2. Backdate so it looks hung, then run the watchdog — one bounded
    //    auto-restart, no give-up (and no comment) yet.
    backdate(&mut reg, &first_id, 600);
    let restarts = reg.watchdog_once(Duration::from_secs(60));
    assert_eq!(restarts, 1, "the hung sweep is auto-restarted once");

    let second_id = running_issue_sweep_id(&reg, 5302).expect("a fresh sweep was re-dispatched");

    // 3. Backdate the NEW sweep too — this tick reaches GiveUp.
    backdate(&mut reg, &second_id, 600);
    let restarts2 = reg.watchdog_once(Duration::from_secs(60));
    assert_eq!(restarts2, 0, "bounded: never a second auto-restart");
    assert!(reg.watchdog_gaveup.contains(&5302), "give-up recorded for the issue");

    // Issue #6179 (Epic #6165 Phase 1): every successful dispatch above
    // ALSO posts a lease comment (`issue comment 5302 --body <!--
    // loom:lease ...`), so "issue comment 5302" alone is no longer a
    // unique fingerprint for the give-up comment specifically — filter on
    // the give-up marker text too, matching this test's actual intent
    // (the give-up comment posts exactly once and dedups per issue, not
    // per tick), not "no other comment of any kind was ever posted".
    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    let comment_lines: Vec<&str> = gh_calls
        .lines()
        .filter(|l| l.contains("issue comment 5302") && l.contains(WATCHDOG_GAVEUP_COMMENT_MARKER))
        .collect();
    assert_eq!(
        comment_lines.len(),
        1,
        "give-up must post exactly one forge comment; got: {gh_calls:?}"
    );
    assert!(
        comment_lines[0].contains(WATCHDOG_GAVEUP_COMMENT_MARKER),
        "comment body should carry the give-up marker; got: {comment_lines:?}"
    );

    // 4. Several further ticks that re-observe the same GiveUp state must
    //    NOT post additional comments (dedup is per-issue, not per-tick).
    for _ in 0..3 {
        let restarts_n = reg.watchdog_once(Duration::from_secs(60));
        assert_eq!(restarts_n, 0, "still bounded — no further restarts");
    }
    let gh_calls_final = std::fs::read_to_string(&gh_log).unwrap_or_default();
    let comment_count_final = gh_calls_final
        .lines()
        .filter(|l| l.contains("issue comment 5302") && l.contains(WATCHDOG_GAVEUP_COMMENT_MARKER))
        .count();
    assert_eq!(
        comment_count_final, 1,
        "repeated ticks after give-up must not post additional comments; got: {gh_calls_final:?}"
    );

    // Cleanup: cancel the lingering hung child.
    if let Some(id) = running_issue_sweep_id(&reg, 5302) {
        let _ = reg.cancel(&id, Duration::from_secs(2));
    }
}

#[test]
fn watchdog_leaves_progressing_sweep_alone() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let mut reg = hung_child_registry(ws);

    let out = reg
        .dispatch(&SweepKind::Issue(4343), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));

    // Simulate progress: create a worktree for the issue.
    let wt = ws.join(".loom").join("worktrees").join("issue-4343");
    std::fs::create_dir_all(&wt).unwrap();

    // Even backdated well past the timeout, an issue with a worktree is
    // never restarted.
    backdate(&mut reg, &out.sweep_id, 9999);
    assert_eq!(reg.watchdog_once(Duration::from_secs(10)), 0);
    assert!(!reg.watchdog_retried.contains(&4343));

    // Cleanup.
    let _ = reg.cancel(&out.sweep_id, Duration::from_secs(2));
}

// --- progress latch (Issue #4088) ---

/// AC5 regression (the headline bug): a sweep that made progress (worktree
/// present), then had that worktree AND its checkpoint torn down at
/// completion while still `Running`, with `elapsed` far past the timeout,
/// must NOT be cancelled or re-dispatched. On `origin/main` the stateless
/// probe reads "no progress" after cleanup and re-dispatches against the
/// now-closed issue; the per-`SweepId` latch prevents that.
#[test]
fn watchdog_does_not_redispatch_completed_sweep_after_cleanup() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let mut reg = hung_child_registry(ws);

    let out = reg
        .dispatch(&SweepKind::Issue(4078), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));

    // Progress appears (Builder created a worktree), and a tick observes +
    // latches it.
    let wt = ws.join(".loom").join("worktrees").join("issue-4078");
    std::fs::create_dir_all(&wt).unwrap();
    assert_eq!(reg.watchdog_once(Duration::from_secs(10)), 0);
    assert!(
        reg.watchdog_progressed.contains(&out.sweep_id),
        "progress is latched for the sweep"
    );

    // Completion tears down every progress signal (merge-pr.sh removes the
    // worktree; /loom:sweep deletes the checkpoint). The stateless probe now
    // reads no-progress — the exact #4078 condition.
    std::fs::remove_dir_all(&wt).unwrap();
    assert!(
        !reg.sweep_made_progress(4078, &out.log_path),
        "stateless probe reads no-progress after cleanup (the bug's precondition)"
    );

    // Even backdated far past the timeout, the latched sweep is left alone.
    backdate(&mut reg, &out.sweep_id, 9999);
    assert_eq!(
        reg.watchdog_once(Duration::from_secs(10)),
        0,
        "a completed-then-cleaned-up sweep is never re-dispatched (AC5)"
    );
    assert!(
        !reg.watchdog_retried.contains(&4078),
        "no retry recorded for the completed sweep"
    );

    let _ = reg.cancel(&out.sweep_id, Duration::from_secs(2));
}

/// AC2 on re-dispatch (the Finding 6 trap): the latch is keyed by `SweepId`,
/// not issue. A latch keyed by issue would make a *re-dispatched* sweep that
/// genuinely hangs read as "already progressed" and never be rescued —
/// silently defanging the watchdog for the very issues it already rescued
/// once. A prior sweep's latch (distinct `SweepId`) must not cover a new
/// hung sweep for the same issue.
#[test]
fn watchdog_latch_is_scoped_by_sweep_id_so_redispatch_is_still_rescued() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let mut reg = hung_child_registry(ws);

    let out = reg
        .dispatch(&SweepKind::Issue(4060), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));

    // Simulate a PRIOR, now-gone sweep for the SAME issue having progressed:
    // its distinct SweepId is latched. An issue-keyed latch would instead
    // hold `4060` and wrongly cover the current sweep.
    reg.watchdog_progressed
        .insert("sweep-issue-4060-prior".to_string());
    assert!(
        !reg.watchdog_progressed.contains(&out.sweep_id),
        "the current (hung) sweep is not itself latched"
    );

    // The current sweep never progressed; backdate it past the timeout.
    backdate(&mut reg, &out.sweep_id, 600);
    assert_eq!(
        reg.watchdog_once(Duration::from_secs(60)),
        1,
        "a re-dispatched sweep that hangs at startup is still rescued (AC2)"
    );
    assert!(reg.watchdog_retried.contains(&4060));

    if let Some(id) = running_issue_sweep_id(&reg, 4060) {
        let _ = reg.cancel(&id, Duration::from_secs(2));
    }
}

/// The latch is monotonic (stays true across ticks once observed) AND scoped
/// to a single `SweepId` — a sibling sweep that never progressed is
/// unaffected and still eligible for rescue.
#[test]
fn watchdog_latch_is_monotonic_and_per_sweep() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let mut reg = hung_child_registry(ws);

    let a = reg
        .dispatch(&SweepKind::Issue(5001), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(a.pid, FIXTURE_CHILD_WAIT_MS));
    let b = reg
        .dispatch(&SweepKind::Issue(5002), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(b.pid, FIXTURE_CHILD_WAIT_MS));

    // Only A makes progress.
    let wt_a = ws.join(".loom").join("worktrees").join("issue-5001");
    std::fs::create_dir_all(&wt_a).unwrap();
    assert_eq!(reg.watchdog_once(Duration::from_secs(10)), 0);
    assert!(reg.watchdog_progressed.contains(&a.sweep_id), "A latched");
    assert!(
        !reg.watchdog_progressed.contains(&b.sweep_id),
        "B never progressed ⇒ not latched"
    );

    // Monotonic: remove A's worktree; a later tick keeps A latched.
    std::fs::remove_dir_all(&wt_a).unwrap();
    assert_eq!(reg.watchdog_once(Duration::from_secs(10)), 0);
    assert!(
        reg.watchdog_progressed.contains(&a.sweep_id),
        "A stays latched across ticks even with its worktree gone"
    );

    // B, never progressing and backdated, is still restarted — A's latch is
    // scoped to A and does not cover its sibling.
    backdate(&mut reg, &b.sweep_id, 600);
    assert_eq!(
        reg.watchdog_once(Duration::from_secs(60)),
        1,
        "the un-latched sibling is rescued"
    );
    assert!(reg.watchdog_retried.contains(&5002));
    assert!(!reg.watchdog_retried.contains(&5001));

    for issue in [5001u32, 5002u32] {
        if let Some(id) = running_issue_sweep_id(&reg, issue) {
            let _ = reg.cancel(&id, Duration::from_secs(2));
        }
    }
}

/// Latch pruning: entries for sweeps GC'd from `entries` are dropped from
/// the latch, so the per-`SweepId` set cannot grow unbounded across many
/// dispatches.
#[test]
fn watchdog_latch_pruned_on_entry_gc() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, _rec) = fixture_registry(ws);

    // A terminal entry aged past the retention window, with its SweepId
    // latched — exactly the state left behind by a completed sweep.
    let sid = "sweep-issue-6001-done".to_string();
    let old = Utc::now() - chrono::Duration::seconds(TERMINAL_RETENTION_SECS + 60);
    reg.entries.insert(
        sid.clone(),
        SweepInfo {
            pgid: None,
            sweep_id: sid.clone(),
            kind: SweepKind::Issue(6001),
            pid: 2_147_483_640,
            token_name: "unknown".into(),
            runtime: "unknown".into(),
            runtime_source: None,
            log_path: ws.join(".loom/logs/sweep-issue-6001.log"),
            idempotency_key: None,
            started_at: old,
            state: SweepState::Exited {
                code: Some(0),
                at: old,
            },
            latest_phase: None,
            pr_number: None,
            model: None,
            effort: None,
            depends_on: None,
            repo: None,
        },
    );
    reg.watchdog_progressed.insert(sid.clone());

    // GC drops the terminal entry and must prune its latch entry with it.
    reg.reap_once();
    assert!(!reg.entries.contains_key(&sid), "terminal entry is GC'd");
    assert!(
        !reg.watchdog_progressed.contains(&sid),
        "the latch entry is pruned alongside the GC'd sweep"
    );
}

// ===================================================================
// Occupancy accounting — startup-proof grace (Issue #4003)
// ===================================================================

#[test]
fn startup_proof_grace_setter_roundtrips() {
    let tmp = tempdir().unwrap();
    let (mut reg, _rec) = fixture_registry(tmp.path());
    assert_eq!(
        reg.startup_proof_grace(),
        Duration::from_secs(DEFAULT_STARTUP_PROOF_GRACE_SECS),
        "default matches the shipped constant"
    );
    reg.set_startup_proof_grace(Duration::from_secs(12));
    assert_eq!(reg.startup_proof_grace(), Duration::from_secs(12));
}

/// Test-plan item (a): a dispatched sweep that never emits the
/// startup-proof signal releases its admission slot **before** the 300s
/// watchdog fires. `hung_child_registry`'s fixture child produces zero
/// progress signal (no worktree, no checkpoint, log stuck at the spawn
/// header) for its whole life, exactly the "wedged at startup" case #4003
/// targets.
#[test]
fn occupied_issues_excludes_unproven_sweep_past_grace_window() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let mut reg = hung_child_registry(ws);
    reg.set_startup_proof_grace(Duration::from_millis(50));

    let out = reg
        .dispatch(&SweepKind::Issue(7001), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));

    // Well past the 50ms grace, with zero progress signal.
    backdate(&mut reg, &out.sweep_id, 5);
    let occupied = reg.occupied_issues();
    assert!(
        !occupied.contains(&7001),
        "an unproven sweep past its grace window must stop consuming an \
             admission slot, freeing capacity long before the (unchanged) 300s \
             startup watchdog would cancel/re-dispatch it"
    );
    // The registry's own liveness bookkeeping is untouched: the entry is
    // still `Running` and still the authoritative in-flight/dedup view —
    // discounting occupancy never re-dispatches the SAME issue.
    assert!(matches!(reg.get(&out.sweep_id).unwrap().state, SweepState::Running));

    let _ = reg.cancel(&out.sweep_id, Duration::from_secs(2));
}

/// A freshly-dispatched sweep — even one that will eventually turn out to
/// be hung — counts toward occupancy while inside its grace window. This
/// is what keeps a burst dispatch from immediately under-counting its own
/// occupancy the instant `dispatch()` returns.
#[test]
fn occupied_issues_keeps_fresh_dispatch_inside_grace_window() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let mut reg = hung_child_registry(ws);
    reg.set_startup_proof_grace(Duration::from_secs(DEFAULT_STARTUP_PROOF_GRACE_SECS));

    let out = reg
        .dispatch(&SweepKind::Issue(7002), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));

    // No backdating: elapsed is ~0s, well inside the 90s default grace.
    let occupied = reg.occupied_issues();
    assert!(
        occupied.contains(&7002),
        "a fresh dispatch must count toward occupancy immediately, \
             regardless of whether it has produced any startup-proof signal yet"
    );

    let _ = reg.cancel(&out.sweep_id, Duration::from_secs(2));
}

/// Test-plan item (c) (throughput regression guard): a sweep that HAS
/// proven startup progress must never be discounted, no matter how long
/// ago it was dispatched or how short the configured grace is. Without
/// this, a normal sweep whose Builder phase legitimately runs for hours
/// would eventually be discounted from occupancy — silently inflating the
/// effective concurrency cap for reasons unrelated to health. Proven
/// progress must dominate elapsed time, unconditionally.
#[test]
fn occupied_issues_never_discounts_proven_sweep_regardless_of_age() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let mut reg = hung_child_registry(ws);
    // A pathologically tiny grace: if elapsed-vs-grace were the only
    // signal, this sweep would be discounted instantly.
    reg.set_startup_proof_grace(Duration::from_millis(1));

    let out = reg
        .dispatch(&SweepKind::Issue(7003), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));

    // Simulate progress (Builder created a worktree) AND age the entry
    // far past any plausible grace or watchdog window.
    let wt = ws.join(".loom").join("worktrees").join("issue-7003");
    std::fs::create_dir_all(&wt).unwrap();
    backdate(&mut reg, &out.sweep_id, 9999);

    let occupied = reg.occupied_issues();
    assert!(
        occupied.contains(&7003),
        "a sweep that proved startup progress must never be discounted \
             from occupancy, regardless of elapsed time — this is the \
             guarantee that a fleet of normally-starting sweeps dispatches at \
             the same rate as before #4003"
    );

    let _ = reg.cancel(&out.sweep_id, Duration::from_secs(2));
}

/// The occupancy check and the startup watchdog (#3887/#4088) share the
/// SAME per-`SweepId` progress latch (`watchdog_progressed`): once either
/// call site observes progress, neither ever "un-sees" it — even after
/// the underlying filesystem signal is torn down (e.g. at completion, or
/// in this test, a manual removal standing in for that teardown).
#[test]
fn occupied_issues_latch_is_shared_with_watchdog() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let mut reg = hung_child_registry(ws);
    reg.set_startup_proof_grace(Duration::from_millis(1));

    let out = reg
        .dispatch(&SweepKind::Issue(7004), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));

    let wt = ws.join(".loom").join("worktrees").join("issue-7004");
    std::fs::create_dir_all(&wt).unwrap();
    backdate(&mut reg, &out.sweep_id, 9999);

    // Observe progress via occupancy accounting first — this latches it.
    assert!(reg.occupied_issues().contains(&7004));
    assert!(reg.watchdog_progressed.contains(&out.sweep_id));

    // Tear down the filesystem signal (mirrors what happens at
    // completion) and confirm BOTH consumers still treat it as proven.
    std::fs::remove_dir_all(&wt).unwrap();
    assert!(
        reg.occupied_issues().contains(&7004),
        "occupancy must not re-discount a sweep once the latch has fired"
    );
    assert_eq!(
        reg.watchdog_once(Duration::from_secs(10)),
        0,
        "the startup watchdog must not restart a sweep the occupancy \
             check already latched as progressed"
    );

    let _ = reg.cancel(&out.sweep_id, Duration::from_secs(2));
}

/// Observability (Issue #4003 AC): the daemon can report how long a sweep
/// has spent in the spawned-but-not-started state, and the report clears
/// the instant progress is observed.
#[test]
fn unproven_startups_reports_elapsed_and_clears_once_proven() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let mut reg = hung_child_registry(ws);

    let out = reg
        .dispatch(&SweepKind::Issue(7005), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));
    backdate(&mut reg, &out.sweep_id, 42);

    let unproven = reg.unproven_startups();
    let entry = unproven.iter().find(|(issue, _)| *issue == 7005);
    assert!(
        entry.is_some(),
        "an unproven live sweep must be reported by unproven_startups()"
    );
    let (_, elapsed) = entry.unwrap();
    assert!(
        *elapsed >= Duration::from_secs(42),
        "reported elapsed should reflect the backdated dispatch time, got {elapsed:?}"
    );

    // Progress appears — the report must clear immediately.
    let wt = ws.join(".loom").join("worktrees").join("issue-7005");
    std::fs::create_dir_all(&wt).unwrap();
    assert!(
        !reg.unproven_startups()
            .iter()
            .any(|(issue, _)| *issue == 7005),
        "a sweep that has proven progress must not be reported as unproven"
    );

    let _ = reg.cancel(&out.sweep_id, Duration::from_secs(2));
}

// --- resolve_startup_proof_grace precedence ---

#[test]
#[serial]
fn resolve_startup_proof_grace_precedence() {
    std::env::remove_var(STARTUP_PROOF_GRACE_ENV);
    assert_eq!(
        resolve_startup_proof_grace(&StartupRaceConfig::default()),
        Duration::from_secs(DEFAULT_STARTUP_PROOF_GRACE_SECS)
    );
    let cfg = StartupRaceConfig {
        startup_proof_grace_secs: Some(30),
        ..Default::default()
    };
    assert_eq!(resolve_startup_proof_grace(&cfg), Duration::from_secs(30));
    std::env::set_var(STARTUP_PROOF_GRACE_ENV, "5");
    assert_eq!(resolve_startup_proof_grace(&cfg), Duration::from_secs(5));
    std::env::remove_var(STARTUP_PROOF_GRACE_ENV);
}

#[test]
fn startup_race_config_missing_is_all_none() {
    let tmp = tempdir().unwrap();
    assert_eq!(read_startup_race_config(tmp.path()), StartupRaceConfig::default());
}

#[test]
fn startup_race_config_full_block_parsed() {
    let tmp = tempdir().unwrap();
    write_cfg(
        tmp.path(),
        r#"{"autonomous":{"dispatchStaggerMs":3000,"watchdog":{"enabled":false,"timeoutSecs":90,"intervalSecs":15,"reviewStall":false,"reviewStallTimeoutSecs":1800,"startupProofGraceSecs":45,"staleSweep":false,"staleSweepAgeSecs":9000}}}"#,
    );
    assert_eq!(
        read_startup_race_config(tmp.path()),
        StartupRaceConfig {
            dispatch_stagger_ms: Some(3000),
            watchdog_enabled: Some(false),
            watchdog_timeout_secs: Some(90),
            watchdog_interval_secs: Some(15),
            review_stall_enabled: Some(false),
            review_stall_timeout_secs: Some(1800),
            startup_proof_grace_secs: Some(45),
            stale_sweep_enabled: Some(false),
            stale_sweep_age_secs: Some(9000),
        }
    );
}

#[test]
fn startup_race_config_zero_stagger_is_honored() {
    // A 0 stagger is a real "disable" value and must be preserved (unlike
    // the interval/timeout fields where 0 is dropped to None).
    let tmp = tempdir().unwrap();
    write_cfg(tmp.path(), r#"{"autonomous":{"dispatchStaggerMs":0}}"#);
    assert_eq!(read_startup_race_config(tmp.path()).dispatch_stagger_ms, Some(0));
}

#[test]
#[serial(loom_config_env)]
fn startup_race_config_project_tier_only_is_honored_like_legacy() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempdir().unwrap();
    write_project_cfg(
        tmp.path(),
        r#"{"autonomous":{"dispatchStaggerMs":3000,"watchdog":{"enabled":false,"timeoutSecs":90}}}"#,
    );
    let cfg = read_startup_race_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg.dispatch_stagger_ms, Some(3000));
    assert_eq!(cfg.watchdog_enabled, Some(false));
    assert_eq!(cfg.watchdog_timeout_secs, Some(90));
}

#[test]
#[serial(loom_config_env)]
fn startup_race_config_project_tier_overrides_legacy_overlap_and_supplies_non_overlap() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempdir().unwrap();
    write_cfg(
        tmp.path(),
        r#"{"autonomous":{"dispatchStaggerMs":3000,"watchdog":{"timeoutSecs":90}}}"#,
    );
    write_project_cfg(tmp.path(), r#"{"autonomous":{"dispatchStaggerMs":750}}"#);
    let cfg = read_startup_race_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    // Overlapping `dispatchStaggerMs` -> project tier wins.
    assert_eq!(cfg.dispatch_stagger_ms, Some(750));
    // Non-overlapping `watchdog.timeoutSecs` still supplied by legacy tier.
    assert_eq!(cfg.watchdog_timeout_secs, Some(90));
}

#[test]
#[serial(loom_config_env)]
fn startup_race_config_local_tier_overrides_legacy_and_project() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempdir().unwrap();
    write_cfg(tmp.path(), r#"{"autonomous":{"dispatchStaggerMs":3000}}"#);
    write_project_cfg(tmp.path(), r#"{"autonomous":{"dispatchStaggerMs":750}}"#);
    write_local_cfg(tmp.path(), r#"{"autonomous":{"dispatchStaggerMs":10}}"#);
    let cfg = read_startup_race_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg.dispatch_stagger_ms, Some(10));
}

/// Regression (#4058): `dispatchStaggerMs: 0` set only at the project
/// tier must still be read as `Some(0)` ("disable stagger"), not dropped
/// to `None` like a zero `watchdog.timeoutSecs` would be.
#[test]
#[serial(loom_config_env)]
fn startup_race_config_project_tier_dispatch_stagger_zero_is_meaningful() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempdir().unwrap();
    write_project_cfg(tmp.path(), r#"{"autonomous":{"dispatchStaggerMs":0}}"#);
    let cfg = read_startup_race_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg.dispatch_stagger_ms, Some(0));
}

/// Explicit `null` at the project tier clears a legacy-tier value —
/// documents the `deep_merge` "null clears" semantics (#4058) at this
/// migrated site.
#[test]
#[serial(loom_config_env)]
fn startup_race_config_explicit_null_in_project_tier_clears_legacy_value() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let tmp = tempdir().unwrap();
    write_cfg(tmp.path(), r#"{"autonomous":{"dispatchStaggerMs":3000}}"#);
    write_project_cfg(tmp.path(), r#"{"autonomous":{"dispatchStaggerMs":null}}"#);
    let cfg = read_startup_race_config(tmp.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(cfg.dispatch_stagger_ms, None);
}

#[test]
#[serial]
fn resolve_dispatch_stagger_precedence() {
    std::env::remove_var(DISPATCH_STAGGER_ENV);
    // Default when nothing set.
    assert_eq!(
        resolve_dispatch_stagger(&StartupRaceConfig::default()),
        Duration::from_millis(DEFAULT_DISPATCH_STAGGER_MS)
    );
    // Config used when env unset.
    let cfg = StartupRaceConfig {
        dispatch_stagger_ms: Some(500),
        ..Default::default()
    };
    assert_eq!(resolve_dispatch_stagger(&cfg), Duration::from_millis(500));
    // Env overrides config.
    std::env::set_var(DISPATCH_STAGGER_ENV, "750");
    assert_eq!(resolve_dispatch_stagger(&cfg), Duration::from_millis(750));
    // Env 0 disables (overriding a non-zero config).
    std::env::set_var(DISPATCH_STAGGER_ENV, "0");
    assert_eq!(resolve_dispatch_stagger(&cfg), Duration::ZERO);
    std::env::remove_var(DISPATCH_STAGGER_ENV);
}

#[test]
#[serial]
fn resolve_watchdog_enabled_precedence() {
    std::env::remove_var(WATCHDOG_ENABLE_ENV);
    // Default ON (self-healing backstop).
    assert!(resolve_watchdog_enabled(&StartupRaceConfig::default()));
    // Config can disable.
    let off = StartupRaceConfig {
        watchdog_enabled: Some(false),
        ..Default::default()
    };
    assert!(!resolve_watchdog_enabled(&off));
    // Env overrides config in both directions.
    std::env::set_var(WATCHDOG_ENABLE_ENV, "1");
    assert!(resolve_watchdog_enabled(&off));
    std::env::set_var(WATCHDOG_ENABLE_ENV, "0");
    let on = StartupRaceConfig {
        watchdog_enabled: Some(true),
        ..Default::default()
    };
    assert!(!resolve_watchdog_enabled(&on));
    std::env::remove_var(WATCHDOG_ENABLE_ENV);
}

#[test]
#[serial]
fn resolve_watchdog_timeout_and_interval_precedence() {
    std::env::remove_var(WATCHDOG_TIMEOUT_ENV);
    std::env::remove_var(WATCHDOG_INTERVAL_ENV);
    // AC1 (#4088): the default no-progress window is 300s — clear of the
    // observed 110–150s healthy dispatch→worktree distribution.
    assert_eq!(DEFAULT_WATCHDOG_TIMEOUT_SECS, 300);
    assert_eq!(
        resolve_watchdog_timeout(&StartupRaceConfig::default()),
        Duration::from_secs(300)
    );
    assert_eq!(
        resolve_watchdog_timeout(&StartupRaceConfig::default()),
        Duration::from_secs(DEFAULT_WATCHDOG_TIMEOUT_SECS)
    );
    assert_eq!(
        resolve_watchdog_interval(&StartupRaceConfig::default()),
        Duration::from_secs(DEFAULT_WATCHDOG_INTERVAL_SECS)
    );
    let cfg = StartupRaceConfig {
        watchdog_timeout_secs: Some(200),
        watchdog_interval_secs: Some(45),
        ..Default::default()
    };
    assert_eq!(resolve_watchdog_timeout(&cfg), Duration::from_secs(200));
    assert_eq!(resolve_watchdog_interval(&cfg), Duration::from_secs(45));
    std::env::set_var(WATCHDOG_TIMEOUT_ENV, "77");
    std::env::set_var(WATCHDOG_INTERVAL_ENV, "11");
    assert_eq!(resolve_watchdog_timeout(&cfg), Duration::from_secs(77));
    assert_eq!(resolve_watchdog_interval(&cfg), Duration::from_secs(11));
    std::env::remove_var(WATCHDOG_TIMEOUT_ENV);
    std::env::remove_var(WATCHDOG_INTERVAL_ENV);
}

// ===================================================================
// Review-phase stall watchdog (Issue #3910)
// ===================================================================

// --- review_stall_decision pure state machine ---

#[test]
fn review_stall_decision_within_timeout_is_healthy() {
    let t = Duration::from_secs(2700);
    // Log written recently ⇒ alive ⇒ Healthy, regardless of retry state.
    assert_eq!(
        review_stall_decision(Duration::from_secs(120), t, false),
        WatchdogDecision::Healthy
    );
    assert_eq!(
        review_stall_decision(Duration::from_secs(2699), t, true),
        WatchdogDecision::Healthy
    );
}

#[test]
fn review_stall_decision_silent_first_time_restarts() {
    let t = Duration::from_secs(2700);
    assert_eq!(
        review_stall_decision(Duration::from_secs(2701), t, false),
        WatchdogDecision::Restart
    );
}

#[test]
fn review_stall_decision_silent_after_retry_gives_up() {
    // Bounded: a second stall past the timeout does not restart again.
    let t = Duration::from_secs(2700);
    assert_eq!(
        review_stall_decision(Duration::from_secs(9999), t, true),
        WatchdogDecision::GiveUp
    );
}

// --- log_idle filesystem probe ---

#[test]
fn log_idle_none_for_missing_some_for_present() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (reg, _rec) = fixture_registry(ws);

    // Missing file ⇒ None (cannot assess).
    let missing = ws.join("nope.log");
    assert!(reg.log_idle(&missing).is_none());

    // A freshly written file ⇒ Some, and its idle is tiny.
    let present = ws.join("sweep.log");
    std::fs::write(&present, "hello\n").unwrap();
    let idle = reg
        .log_idle(&present)
        .expect("present file has a readable mtime");
    assert!(idle < Duration::from_secs(60), "a just-written log is not idle: {idle:?}");
}

// --- resolve_review_stall_* precedence ---

#[test]
#[serial]
fn resolve_review_stall_enabled_precedence() {
    std::env::remove_var(REVIEW_STALL_ENABLE_ENV);
    // Default ON (self-healing backstop).
    assert!(resolve_review_stall_enabled(&StartupRaceConfig::default()));
    // Config can disable.
    let off = StartupRaceConfig {
        review_stall_enabled: Some(false),
        ..Default::default()
    };
    assert!(!resolve_review_stall_enabled(&off));
    // Env overrides config in both directions.
    std::env::set_var(REVIEW_STALL_ENABLE_ENV, "1");
    assert!(resolve_review_stall_enabled(&off));
    std::env::set_var(REVIEW_STALL_ENABLE_ENV, "0");
    let on = StartupRaceConfig {
        review_stall_enabled: Some(true),
        ..Default::default()
    };
    assert!(!resolve_review_stall_enabled(&on));
    std::env::remove_var(REVIEW_STALL_ENABLE_ENV);
}

#[test]
#[serial]
fn resolve_review_stall_timeout_precedence() {
    std::env::remove_var(REVIEW_STALL_TIMEOUT_ENV);
    assert_eq!(
        resolve_review_stall_timeout(&StartupRaceConfig::default()),
        Duration::from_secs(DEFAULT_REVIEW_STALL_TIMEOUT_SECS)
    );
    let cfg = StartupRaceConfig {
        review_stall_timeout_secs: Some(1800),
        ..Default::default()
    };
    assert_eq!(resolve_review_stall_timeout(&cfg), Duration::from_secs(1800));
    std::env::set_var(REVIEW_STALL_TIMEOUT_ENV, "600");
    assert_eq!(resolve_review_stall_timeout(&cfg), Duration::from_secs(600));
    // A zero/invalid env value is dropped, falling back to config.
    std::env::set_var(REVIEW_STALL_TIMEOUT_ENV, "0");
    assert_eq!(resolve_review_stall_timeout(&cfg), Duration::from_secs(1800));
    std::env::remove_var(REVIEW_STALL_TIMEOUT_ENV);
}

// --- review_stall_watchdog_once: bounded auto-restart end-to-end ---

#[test]
fn review_stall_watchdog_ignores_prestartup_sweep() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let mut reg = hung_child_registry(ws);

    let out = reg
        .dispatch(&SweepKind::Issue(5150), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));

    // No worktree/checkpoint yet ⇒ NOT past startup ⇒ the review-stall
    // watchdog leaves it entirely to the #3887 startup watchdog, even with a
    // zero timeout that would otherwise force a stall.
    assert_eq!(reg.review_stall_watchdog_once(Duration::ZERO), 0);
    assert!(!reg.review_stall_retried.contains(&5150));

    let _ = reg.cancel(&out.sweep_id, Duration::from_secs(2));
}

#[test]
fn review_stall_watchdog_restarts_stalled_sweep_once_then_gives_up() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let mut reg = hung_child_registry(ws);

    // 1. Dispatch a sweep for issue 5252 and mark it past startup by
    //    creating its worktree (the review-stall watchdog only acts on
    //    sweeps that already made progress).
    let out = reg
        .dispatch(&SweepKind::Issue(5252), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS), "fixture child should start");
    let wt = ws.join(".loom").join("worktrees").join("issue-5252");
    std::fs::create_dir_all(&wt).unwrap();

    // 2. With a generous timeout the freshly-written log is NOT idle ⇒ the
    //    sweep is healthy and untouched.
    assert_eq!(
        reg.review_stall_watchdog_once(Duration::from_secs(3600)),
        0,
        "a sweep still emitting log output is not disturbed"
    );

    // 3. A zero timeout forces the stall verdict (any log idle >= 0) ⇒ the
    //    wedged sweep is auto-cancelled and re-dispatched exactly once.
    let restarts = reg.review_stall_watchdog_once(Duration::ZERO);
    assert_eq!(restarts, 1, "the stalled sweep is auto-restarted once");
    assert!(reg.review_stall_retried.contains(&5252), "issue marked retried (bounded)");
    let second_id = running_issue_sweep_id(&reg, 5252).expect("a fresh sweep was re-dispatched");

    // 4. The re-dispatched sweep still has a worktree (past startup) and a
    //    fresh log; a zero timeout stalls it again, but the watchdog is
    //    bounded — it gives up instead of restarting a second time.
    let restarts2 = reg.review_stall_watchdog_once(Duration::ZERO);
    assert_eq!(restarts2, 0, "bounded: never a second auto-restart");
    assert!(reg.review_stall_gaveup.contains(&5252), "give-up recorded for the issue");
    assert!(
        running_issue_sweep_id(&reg, 5252).is_some(),
        "the sweep is left running for the operator"
    );

    // Cleanup: cancel the lingering child.
    let _ = reg.cancel(&second_id, Duration::from_secs(2));
}

// --- review_stall_watchdog_once: open-linked-PR handoff (Issue #7649) ---

/// Core acceptance criterion: a review-stalled sweep whose issue already
/// has a CONFIRMED open linked PR converts its recovery to
/// `SweepKind::PrSet` instead of restarting `Issue` work — no second
/// Builder/Curator spawn, the issue's own worktree (and any uncommitted
/// bytes in it) is left completely untouched, and the returned/dispatched
/// kind is `PrSet`, not `Issue`.
#[test]
#[serial]
fn review_stall_watchdog_converts_open_pr_recovery_to_prset() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, gh_log) = open_pr_review_stall_registry(ws, "9200", 0);

    // 1. Dispatch a sweep for issue 9100 and mark it past startup with a
    //    worktree carrying uncommitted bytes — the recovery conversion
    //    below must leave this file untouched (never clean/reset the
    //    worktree, per the issue's explicit constraint).
    let out = reg
        .dispatch(&SweepKind::Issue(9100), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS), "fixture child should start");
    let wt = ws.join(".loom").join("worktrees").join("issue-9100");
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(wt.join("dirty.txt"), "uncommitted Builder output\n").unwrap();

    // 2. Generous timeout ⇒ the freshly-written log is not idle ⇒ healthy.
    assert_eq!(
        reg.review_stall_watchdog_once(Duration::from_secs(3600)),
        0,
        "a sweep still emitting log output is not disturbed"
    );

    // 3. Zero timeout forces the stall verdict. The Issue-keyed recovery
    //    re-dispatch hits the real #4123 open-PR guard (PR #9200 is
    //    open) and is refused — the fix converts that refusal into a
    //    `PrSet([9200])` dispatch instead of stranding the recovery.
    let restarts = reg.review_stall_watchdog_once(Duration::ZERO);
    assert_eq!(restarts, 1, "the conversion counts as one recovered sweep");

    // No Issue-kind sweep for 9100 is running — no Builder/Curator was
    // re-spawned for it.
    assert!(
        running_issue_sweep_id(&reg, 9100).is_none(),
        "the Issue-keyed recovery must not run — its open PR already exists"
    );
    // A PrSet([9200]) sweep IS running — Judge/Doctor -> Merge picks up
    // the existing PR directly.
    let prset_id = running_prset_sweep_id(&reg, &[9200])
        .expect("the recovery must convert to a PrSet([9200]) dispatch");

    // Bounded retry bookkeeping unchanged: the single allowed attempt for
    // issue 9100 is consumed exactly like an ordinary Issue-kind restart.
    assert!(reg.review_stall_retried.contains(&9100), "issue marked retried (bounded)");

    // The worktree — and its uncommitted bytes — must be completely
    // untouched: no clean, no reset.
    assert_eq!(
        std::fs::read_to_string(wt.join("dirty.txt")).unwrap(),
        "uncommitted Builder output\n",
        "the PrSet conversion must never touch the issue's worktree"
    );

    // The issue's claim was restored by `cancel()` (loom:building ->
    // loom:issue) exactly as an ordinary recovery would, but no SECOND
    // claim flip (loom:issue -> loom:building) happened for 9100 — the
    // guard refused before any label mutation, so the original claim
    // (at the initial `dispatch()` above) is the ONLY claim flip issue
    // 9100 ever sees.
    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        calls.contains("issue edit 9100 --remove-label loom:building --add-label loom:issue"),
        "cancel() must restore the issue's claim; got: {calls:?}"
    );
    let claim_count = calls
        .lines()
        .filter(|l| {
            l.contains("issue edit 9100 --remove-label loom:issue --add-label loom:building")
        })
        .count();
    assert_eq!(
        claim_count, 1,
        "only the ORIGINAL dispatch may claim issue 9100 — no second reclaim on the refused \
             recovery; got: {calls:?}"
    );

    // Cleanup: cancel the lingering PrSet child + release its PR lock.
    let _ = reg.cancel(&prset_id, Duration::from_secs(2));
    std::env::remove_var("LOOM_REPO");
}

/// No linked PR ⇒ the Issue-keyed recovery is unaffected: the open-PR
/// guard finds nothing, so the ordinary re-dispatch proceeds exactly as
/// before this fix — proven against the REAL guard (`skip_label_flip =
/// false`), not the guard-skipping `hung_child_registry` fixture the
/// pre-existing bounded-retry test above uses.
#[test]
#[serial]
fn review_stall_watchdog_retains_issue_recovery_when_no_linked_pr() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, _gh_log) = open_pr_review_stall_registry(ws, "", 0);

    let out = reg
        .dispatch(&SweepKind::Issue(9101), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));
    let wt = ws.join(".loom").join("worktrees").join("issue-9101");
    std::fs::create_dir_all(&wt).unwrap();

    let restarts = reg.review_stall_watchdog_once(Duration::ZERO);
    assert_eq!(restarts, 1, "an ordinary Issue-keyed restart, unconverted");
    let second_id = running_issue_sweep_id(&reg, 9101)
        .expect("no open PR ⇒ the Issue recovery must proceed normally");
    assert!(
        reg.entries
            .values()
            .all(|i| !matches!(i.kind, SweepKind::PrSet(_))),
        "no PrSet conversion should ever happen when there is no open linked PR"
    );

    let _ = reg.cancel(&second_id, Duration::from_secs(2));
    std::env::remove_var("LOOM_REPO");
}

/// A live peer sweep already owns the PR-set claim lock for the open
/// linked PR (the "existing PR owner deduplicates" acceptance criterion):
/// the conversion attempt must fail closed (PR lock collision), never
/// invent a duplicate PrSet sweep, and must not panic or otherwise crash
/// the watchdog tick — the issue is simply left recoverable for the next
/// pass (bounded: the single retry for THIS issue is still consumed, so
/// it does not loop).
#[test]
#[serial]
fn review_stall_watchdog_prset_conversion_dedupes_against_existing_pr_owner() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, _gh_log) = open_pr_review_stall_registry(ws, "9300", 0);

    // A peer sweep already holds PR #9300's claim lock.
    reg.acquire_pr_lock(9300, "peer-sweep-already-running")
        .unwrap();

    let out = reg
        .dispatch(&SweepKind::Issue(9102), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));
    let wt = ws.join(".loom").join("worktrees").join("issue-9102");
    std::fs::create_dir_all(&wt).unwrap();

    // The conversion is attempted but the PR lock collision refuses it —
    // no sweep recovered this tick, and no duplicate PrSet spawned.
    let restarts = reg.review_stall_watchdog_once(Duration::ZERO);
    assert_eq!(restarts, 0, "a PR-lock collision must not be counted as a recovery");
    assert!(
        running_issue_sweep_id(&reg, 9102).is_none(),
        "no Issue-keyed sweep should be running either — the guard still refuses it"
    );
    assert!(
        running_prset_sweep_id(&reg, &[9300]).is_none(),
        "must not spawn a duplicate PrSet sweep for a PR another sweep already owns"
    );
    // Bounded: the single retry is still consumed even though the
    // conversion attempt itself failed, so this issue never loops.
    assert!(reg.review_stall_retried.contains(&9102));

    // Cleanup: release the peer's lock.
    let _ = reg.release_pr_lock_owned(9300, "peer-sweep-already-running");
    std::env::remove_var("LOOM_REPO");
}

/// An ambiguous/failed open-PR probe (a `gh api graphql` outage) must
/// never be misread as a confirmed open PR — `dispatch()`'s guard already
/// fails OPEN in that case (proceeding with the ordinary Issue-keyed
/// dispatch, unchanged pre-#7649 behavior), so the conversion path here
/// is never even reached; a `downcast_ref` on a generic dispatch error
/// must not accidentally match.
#[test]
#[serial]
fn review_stall_watchdog_probe_failure_never_triggers_prset_conversion() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    // `api graphql` exits non-zero ⇒ open-PR state unknown ⇒ the dispatch
    // guard fails open, same as `dispatch_fails_open_when_open_pr_lookup_errors`.
    let (mut reg, _gh_log) = open_pr_review_stall_registry(ws, "", 1);

    let out = reg
        .dispatch(&SweepKind::Issue(9103), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));
    let wt = ws.join(".loom").join("worktrees").join("issue-9103");
    std::fs::create_dir_all(&wt).unwrap();

    let restarts = reg.review_stall_watchdog_once(Duration::ZERO);
    assert_eq!(restarts, 1, "a probe failure fails OPEN to the ordinary Issue recovery");
    let second_id = running_issue_sweep_id(&reg, 9103)
        .expect("an ambiguous probe must never select an invented PR or convert to PrSet");
    assert!(
        reg.entries
            .values()
            .all(|i| !matches!(i.kind, SweepKind::PrSet(_))),
        "no PrSet conversion may happen on a probe failure"
    );

    let _ = reg.cancel(&second_id, Duration::from_secs(2));
    std::env::remove_var("LOOM_REPO");
}

/// A sweep this daemon instance cannot actually cancel — no retained
/// `Child` handle survives to signal — must never reach either dispatch
/// call (the ordinary Issue path or the new PrSet-conversion path), so it
/// cannot spuriously dispatch a replacement for a sweep that was never
/// actually torn down.
///
/// `cancel()`'s only error path (`begin_cancel`'s "unknown sweep_id") is
/// unreachable here: the candidate snapshot and `cancel()`'s own lookup
/// both read the SAME `self.entries` map inside one synchronous,
/// `&mut self` call, so a hand-selected candidate can never disagree with
/// itself moments later. Losing the retained `self.children` handle —
/// the daemon-restart / handle-already-reaped shape `watchdog_once`'s own
/// candidate filter comment documents — is the practical, reachable
/// equivalent: without it, `review_stall_watchdog_once`'s candidate
/// filter excludes the sweep entirely, which is exactly the outcome that
/// matters (no cancel attempt ⇒ no dispatch ⇒ no replacement, and the
/// bounded retry is left unconsumed for a later tick).
#[test]
fn review_stall_watchdog_uncancelable_sweep_never_dispatches_replacement() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let mut reg = hung_child_registry(ws);

    let out = reg
        .dispatch(&SweepKind::Issue(9104), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));
    let wt = ws.join(".loom").join("worktrees").join("issue-9104");
    std::fs::create_dir_all(&wt).unwrap();

    // Drop the retained Child handle out from under the registry before
    // the watchdog tick runs, reproducing "nothing to cancel" without
    // fabricating a fake error type.
    reg.children.remove(&out.sweep_id);

    let restarts = reg.review_stall_watchdog_once(Duration::ZERO);
    assert_eq!(restarts, 0, "no dispatch without a retained handle to cancel");
    assert!(
        !reg.review_stall_retried.contains(&9104),
        "a candidate with no retained Child handle is not even eligible — never counted \
             as an attempted (let alone consumed) retry"
    );
}

/// cross-owner workspace's installation-token `GH_CONFIG_DIR` through to
/// the real `gh issue comment` child, mirroring the coverage added for
/// `guards::classify_preflip_labels` and `quarantine::apply_quarantine_label`.
#[test]
#[serial]
fn post_watchdog_gaveup_comment_applies_registered_gh_config_dir() {
    crate::credential_preflight::clear_owner_root_registry();
    let dir = tempdir().unwrap();
    let gh_log = dir.path().join("gh.log");
    let owner_dir = dir.path().join(".loom/gh-config-by-owner/2AMLogic");
    crate::credential_preflight::register_root_gh_config_dir(dir.path(), &owner_dir);

    let fake_gh = install_fake_gh_env_logger(dir.path(), &gh_log, "", 0);
    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false;
    let registry = SweepRegistry::new(config);

    registry.post_watchdog_gaveup_comment(9601, Duration::from_secs(120));

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        gh_calls.contains(&format!("GH_CONFIG_DIR={}", owner_dir.display())),
        "expected the registered owner's GH_CONFIG_DIR on the gh child; got: {gh_calls:?}"
    );

    crate::credential_preflight::clear_owner_root_registry();
}

/// The unregistered-root counterpart: a single-owner workspace must leave
/// `GH_CONFIG_DIR` untouched on the give-up comment's child.
#[test]
#[serial]
fn post_watchdog_gaveup_comment_is_a_noop_for_unregistered_workspace_root() {
    // #5651: scrub the test process's own ambient GH_CONFIG_DIR before
    // asserting the child inherits "<unset>" — otherwise this leaks
    // whatever the invoking host's environment happens to contain (e.g.
    // any real Loom fleet worker, which exports GH_CONFIG_DIR
    // process-wide for the daemon, #4458) and the assertion below fails
    // even though the no-op production behavior it exercises is
    // unaffected.
    let _env_guard = ClearedGhConfigDirEnv::new();
    crate::credential_preflight::clear_owner_root_registry();
    let dir = tempdir().unwrap();
    let gh_log = dir.path().join("gh.log");
    let fake_gh = install_fake_gh_env_logger(dir.path(), &gh_log, "", 0);
    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false;
    let registry = SweepRegistry::new(config);

    registry.post_watchdog_gaveup_comment(9602, Duration::from_secs(120));

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        gh_calls.contains("GH_CONFIG_DIR=<unset>"),
        "an unregistered root must not set GH_CONFIG_DIR on the child; got: {gh_calls:?}"
    );
}

// ------------------------------------------------------------------------
// Stale-untracked-sweep backstop (Issue #7529)
// ------------------------------------------------------------------------

#[test]
fn is_stale_untracked_sweep_too_young_is_never_stale() {
    assert!(!is_stale_untracked_sweep(
        Duration::from_secs(10),
        Duration::from_secs(3600),
        None,
        Duration::from_secs(60),
    ));
}

#[test]
fn is_stale_untracked_sweep_old_but_actively_logging_is_healthy() {
    // Old enough to be judged, but the log was appended to a moment ago —
    // mirrors every other watchdog's "any observed progress is Healthy"
    // rule, so a genuinely alive, still-working sweep that merely
    // survived a restart is never disturbed.
    assert!(!is_stale_untracked_sweep(
        Duration::from_secs(20_000),
        Duration::from_secs(3600),
        Some(Duration::from_secs(5)),
        Duration::from_secs(2700),
    ));
}

#[test]
fn is_stale_untracked_sweep_old_and_log_silent_is_stale() {
    assert!(is_stale_untracked_sweep(
        Duration::from_secs(20_000),
        Duration::from_secs(3600),
        Some(Duration::from_secs(3000)),
        Duration::from_secs(2700),
    ));
}

#[test]
fn is_stale_untracked_sweep_old_with_unreadable_log_is_stale() {
    // No readable mtime at all degrades to "cannot prove it's alive and
    // working" — treated as stale, not as healthy-by-default.
    assert!(is_stale_untracked_sweep(
        Duration::from_secs(20_000),
        Duration::from_secs(3600),
        None,
        Duration::from_secs(2700),
    ));
}

/// Issue #7529's core regression pin: `stale_sweep_findings` must find an
/// entry that has NO retained `Child` handle (exactly the shape
/// `reconstruct()`/`adopt_live_journal_sweeps` produce after a daemon
/// restart) once it is old enough and its log is unreadable/silent — even
/// though nothing here ever invoked the watchdog tick loop at all. This is
/// the "does not depend on the same tick loop that failed to protect the
/// original incident" property: the finding is computed on demand.
#[test]
fn stale_sweep_findings_surfaces_an_untracked_aged_sweep_with_zero_tick_activity() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, _rec) = fixture_registry(ws);

    let old_start = Utc::now() - chrono::Duration::seconds(20_000);
    insert_running_with_pid_at(&mut reg, 7529, 1, std::process::id(), old_start);

    // No watchdog task was ever spawned, no tick ever ran — call the pure
    // finder directly, exactly as `build_daemon_status` would on a fresh
    // IPC round-trip.
    let findings = reg.stale_sweep_findings(Duration::from_secs(3600), Duration::from_secs(2700));
    assert_eq!(findings.len(), 1, "the untracked aged sweep must be found");
    assert_eq!(findings[0].issue, 7529);
    assert!(findings[0].elapsed >= Duration::from_secs(20_000));
}

#[test]
fn stale_sweep_findings_ignores_a_too_young_untracked_sweep() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, _rec) = fixture_registry(ws);

    insert_running_with_pid_at(&mut reg, 7530, 1, std::process::id(), Utc::now());

    let findings = reg.stale_sweep_findings(Duration::from_secs(3600), Duration::from_secs(2700));
    assert!(findings.is_empty(), "a freshly-adopted sweep must not be flagged yet");
}

/// No double-reap / no interference (Issue #7529's edge-case AC): a
/// sweep this daemon instance DID spawn — and therefore still holds a
/// `Child` handle for, and which is the other three watchdogs' remit —
/// must never be reachable by this backstop, no matter how old it is.
#[test]
fn stale_sweep_findings_never_reaches_a_properly_tracked_sweep() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let mut reg = hung_child_registry(ws);

    let out = reg
        .dispatch(&SweepKind::Issue(7531), None, None, None, None)
        .unwrap();
    assert!(wait_until_alive(out.pid, FIXTURE_CHILD_WAIT_MS));
    // Backdate it well past every threshold; it is STILL in
    // `self.children` (dispatch retained the handle), so it must remain
    // exclusively the startup-hang/review-stall watchdogs' territory.
    if let Some(info) = reg.entries.get_mut(&out.sweep_id) {
        info.started_at = Utc::now() - chrono::Duration::seconds(20_000);
    }

    let findings = reg.stale_sweep_findings(Duration::from_secs(3600), Duration::from_secs(2700));
    assert!(
        findings.is_empty(),
        "a properly-tracked (children-retained) sweep must never be a stale-sweep finding"
    );

    let _ = reg.cancel(&out.sweep_id, Duration::from_secs(2));
}

/// End-to-end reap: an untracked, aged, log-silent sweep backed by a REAL
/// process (so `cancel()`'s SIGTERM/SIGKILL delivery is exercised safely)
/// is cancelled, its issue's claim lock released, and it is not reaped
/// twice.
#[test]
fn stale_sweep_watchdog_once_reaps_an_untracked_aged_sweep_exactly_once() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, _rec) = fixture_registry(ws);

    // A real, harmless child this registry never spawned itself (so it
    // is never in `self.children`) — safe to signal, unlike the test
    // process's own pid.
    let mut child = Command::new("sleep")
        .arg("30")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn a throwaway live process");
    let pid = child.id();
    assert!(wait_until_alive(pid, FIXTURE_CHILD_WAIT_MS));

    let old_start = Utc::now() - chrono::Duration::seconds(20_000);
    let sweep_id = insert_running_with_pid_at(&mut reg, 7532, 1, pid, old_start);

    let reaped =
        reg.stale_sweep_watchdog_once(Duration::from_secs(3600), Duration::from_secs(2700));
    assert_eq!(reaped, 1, "the untracked aged sweep is reaped");
    assert!(
        reg.entries
            .get(&sweep_id)
            .is_some_and(|i| i.state.is_terminal()),
        "the reaped entry must be transitioned to a terminal state"
    );

    // Give the OS a moment to actually reap the signalled process, then
    // confirm it is gone (bounded poll, mirrors other cancel tests).
    // `child.try_wait()` — not `is_pid_alive` — is load-bearing here: the
    // test process is this child's real OS parent (unlike production,
    // where the pid this backstop targets belongs to a LONG-GONE parent
    // daemon instance, so there is no zombie to reap on this side at
    // all), so until something calls `wait()`/`try_wait()` on it, a
    // terminated child is a zombie whose pid `kill(pid, 0)` still reports
    // alive — exactly the caveat `SweepRegistry::children`'s own doc
    // comment names.
    let mut reaped_exit = None;
    for _ in 0..40 {
        if let Ok(Some(status)) = child.try_wait() {
            reaped_exit = Some(status);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        reaped_exit.is_some(),
        "the stale sweep's process must have been signalled and reaped"
    );

    // No double-reap: a second tick finds nothing (the entry is terminal,
    // no longer Running/Pending).
    let reaped_again =
        reg.stale_sweep_watchdog_once(Duration::from_secs(3600), Duration::from_secs(2700));
    assert_eq!(reaped_again, 0, "bounded: never reaped twice");

    let _ = child.wait();
}

#[test]
fn stale_sweep_watchdog_once_leaves_a_healthy_untracked_sweep_alone() {
    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    let (mut reg, _rec) = fixture_registry(ws);

    let mut child = Command::new("sleep")
        .arg("30")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn a throwaway live process");
    let pid = child.id();
    assert!(wait_until_alive(pid, FIXTURE_CHILD_WAIT_MS));

    // Old enough by wall clock, but its log was JUST written — a live,
    // still-producing sweep that merely survived a restart.
    let old_start = Utc::now() - chrono::Duration::seconds(20_000);
    let sweep_id = insert_running_with_pid_at(&mut reg, 7533, 1, pid, old_start);
    if let Some(info) = reg.entries.get(&sweep_id) {
        std::fs::create_dir_all(info.log_path.parent().unwrap()).unwrap();
        std::fs::write(&info.log_path, "still working\n").unwrap();
    }

    let reaped =
        reg.stale_sweep_watchdog_once(Duration::from_secs(3600), Duration::from_secs(2700));
    assert_eq!(reaped, 0, "an actively-logging sweep must never be reaped");
    assert!(
        reg.entries
            .get(&sweep_id)
            .is_some_and(|i| matches!(i.state, SweepState::Running | SweepState::Pending)),
        "left running"
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn resolve_stale_sweep_enabled_and_age_honor_env_precedence() {
    assert!(resolve_stale_sweep_enabled(&StartupRaceConfig::default()));
    std::env::set_var(STALE_SWEEP_ENABLE_ENV, "0");
    assert!(!resolve_stale_sweep_enabled(&StartupRaceConfig::default()));
    std::env::remove_var(STALE_SWEEP_ENABLE_ENV);

    assert_eq!(
        resolve_stale_sweep_age(&StartupRaceConfig::default()),
        Duration::from_secs(DEFAULT_STALE_SWEEP_AGE_SECS)
    );
    let cfg = StartupRaceConfig {
        stale_sweep_age_secs: Some(7200),
        ..Default::default()
    };
    assert_eq!(resolve_stale_sweep_age(&cfg), Duration::from_secs(7200));
    std::env::set_var(STALE_SWEEP_AGE_ENV, "1800");
    assert_eq!(resolve_stale_sweep_age(&cfg), Duration::from_secs(1800));
    std::env::remove_var(STALE_SWEEP_AGE_ENV);
}

/// The reap comment posts with the expected marker + explanation.
#[test]
#[serial]
fn post_stale_sweep_comment_posts_with_the_expected_marker() {
    let dir = tempdir().unwrap();
    let gh_log = dir.path().join("gh.log");
    let fake_gh = install_fake_gh_env_logger(dir.path(), &gh_log, "", 0);
    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false;
    let registry = SweepRegistry::new(config);

    registry.post_stale_sweep_comment(&StaleSweepFinding {
        issue: 7534,
        sweep_id: "sweep-issue-7534-1".to_string(),
        pid: 4242,
        elapsed: Duration::from_secs(20_000),
        log_idle: None,
    });

    let gh_calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(gh_calls.contains("issue"), "must be a gh issue comment call: {gh_calls:?}");
}
