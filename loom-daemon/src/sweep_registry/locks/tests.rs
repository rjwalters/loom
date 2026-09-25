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

#[test]
fn reconstruct_admits_live_lock_owners() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());

    // Write a lock dir with our own PID as the owner (guaranteed alive).
    let locks = registry.config.locks_dir();
    std::fs::create_dir_all(&locks).unwrap();
    let lock = locks.join("issue-77");
    std::fs::create_dir(&lock).unwrap();
    let owner = LockOwner {
        pgid: None,
        model: None,
        effort: None,
        issue: 77,
        owner_pid: std::process::id(),
        acquired_at: Utc::now().to_rfc3339(),
        sweep_id: "sweep-issue-77-reconstruct".to_string(),
    };
    std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap()).unwrap();

    let root = &registry.config.workspace_root;
    let store = crate::telemetry::trace::store::TraceStore::new(root);
    let saved = store.load_or_create(root, &owner.sweep_id).unwrap();
    let journal =
        crate::telemetry::trace::journal::Journal::for_context(&store.path(root, &owner.sweep_id));
    journal
        .start(
            saved.context.clone(),
            None,
            crate::telemetry::trace::SpanName::Sweep,
            Utc::now(),
            Default::default(),
        )
        .unwrap();
    journal
        .set_supervisor(&saved.context, 2_147_483_640)
        .unwrap();

    let admitted = registry.reconstruct().unwrap();
    assert!(admitted >= 1);
    let info = registry.get("sweep-issue-77-reconstruct").unwrap();
    assert_eq!(info.pid, std::process::id());
    assert!(matches!(info.state, SweepState::Running));
    let restored = journal.active().unwrap();
    assert_eq!(
        restored[0].supervisor_pid,
        Some(std::process::id()),
        "lock-based adoption must renew trace supervision before collector exposure"
    );
    assert_eq!(restored[0].owner_pid, owner.owner_pid);
}

/// Issue #4214: a live-locked issue with **no** matching registry entry at
/// all (no `reconstruct()` has run, no dispatch entry exists) must surface
/// via `unregistered_locked_issues` — this is the "vanish window" case: the
/// lock (filesystem-durable) proves the sweep is alive even though nothing
/// in memory currently reflects it.
#[test]
fn unregistered_locked_issues_surfaces_live_lock_with_no_entry() {
    let dir = tempdir().unwrap();
    let (registry, _record_log) = fixture_registry(dir.path());

    let locks = registry.config.locks_dir();
    std::fs::create_dir_all(&locks).unwrap();
    let lock = locks.join("issue-4201");
    std::fs::create_dir(&lock).unwrap();
    let owner = LockOwner {
        pgid: None,
        model: None,
        effort: None,
        issue: 4201,
        owner_pid: std::process::id(), // guaranteed alive
        acquired_at: Utc::now().to_rfc3339(),
        sweep_id: "sweep-issue-4201-1785221507".to_string(),
    };
    std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap()).unwrap();

    let unregistered = registry.unregistered_locked_issues();
    assert_eq!(
        unregistered,
        vec![(4201, std::process::id())],
        "a live-locked issue with no in-memory entry must surface as unregistered_locked"
    );
}

/// Issue #4214: once the registry has admitted the lock's sweep as a
/// non-terminal entry (e.g. via `reconstruct()`, or a normal `dispatch()`),
/// the same lock must NOT be reported as unregistered — it is registered.
#[test]
fn unregistered_locked_issues_excludes_registered_live_entry() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());

    let locks = registry.config.locks_dir();
    std::fs::create_dir_all(&locks).unwrap();
    let lock = locks.join("issue-4202");
    std::fs::create_dir(&lock).unwrap();
    let owner = LockOwner {
        pgid: None,
        model: None,
        effort: None,
        issue: 4202,
        owner_pid: std::process::id(),
        acquired_at: Utc::now().to_rfc3339(),
        sweep_id: "sweep-issue-4202-registered".to_string(),
    };
    std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap()).unwrap();

    // Reconstruct admits the lock's sweep as a `Running` entry.
    let admitted = registry.reconstruct().unwrap();
    assert!(admitted >= 1);

    let unregistered = registry.unregistered_locked_issues();
    assert!(
        unregistered.is_empty(),
        "a lock whose sweep is already a registered non-terminal entry must not be \
         reported as unregistered_locked; got: {unregistered:?}"
    );
}

/// Issue #4214: a **stale** lock (dead `owner_pid`) must NOT be reported as
/// `unregistered_locked` — that lock is `reconstruct()`'s cleanup remit, not
/// evidence the sweep is still alive.
#[test]
fn unregistered_locked_issues_excludes_stale_dead_pid_lock() {
    let dir = tempdir().unwrap();
    let (registry, _record_log) = fixture_registry(dir.path());

    let locks = registry.config.locks_dir();
    std::fs::create_dir_all(&locks).unwrap();
    let lock = locks.join("issue-4203");
    std::fs::create_dir(&lock).unwrap();
    let owner = LockOwner {
        pgid: None,
        model: None,
        effort: None,
        issue: 4203,
        owner_pid: 2_147_483_640, // dead
        acquired_at: Utc::now().to_rfc3339(),
        sweep_id: "sweep-issue-4203-stale".to_string(),
    };
    std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap()).unwrap();

    let unregistered = registry.unregistered_locked_issues();
    assert!(
        unregistered.is_empty(),
        "a stale (dead-pid) lock must never surface as unregistered_locked; got: \
         {unregistered:?}"
    );
}

#[test]
fn reconstruct_drops_stale_locks() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());

    let locks = registry.config.locks_dir();
    std::fs::create_dir_all(&locks).unwrap();
    let lock = locks.join("issue-78");
    std::fs::create_dir(&lock).unwrap();
    let owner = LockOwner {
        pgid: None,
        model: None,
        effort: None,
        issue: 78,
        owner_pid: 2_147_483_640, // dead
        acquired_at: Utc::now().to_rfc3339(),
        sweep_id: "sweep-issue-78-stale".to_string(),
    };
    std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap()).unwrap();

    let _ = registry.reconstruct().unwrap();
    assert!(!lock.exists(), "stale lock should be removed");
    assert!(registry.get("sweep-issue-78-stale").is_none());
}

/// Issue #3808: a checkpoint with no corresponding daemon-owned lock is an
/// in-session `/loom:sweep` run the daemon never dispatched. `reconstruct`
/// must NOT synthesize a phantom `Crashed` entry for it. (Replaces the old
/// `reconstruct_admits_orphan_checkpoints_as_crashed`, which locked in the
/// pre-#3808 overly-broad behavior.)
#[test]
fn reconstruct_skips_in_session_checkpoints_without_lock() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());

    let cp_dir = registry.config.checkpoint_dir();
    std::fs::create_dir_all(&cp_dir).unwrap();
    // In-session checkpoint: no lock dir was ever written for it.
    std::fs::write(cp_dir.join("issue-91.json"), r#"{"phase":"judge","issue":91}"#).unwrap();

    let admitted = registry.reconstruct().unwrap();
    assert_eq!(admitted, 0, "in-session checkpoint must not be recovered");
    let crashed = registry.list(Some(&SweepState::Crashed { at: Utc::now() }));
    assert!(crashed.is_empty(), "no phantom Crashed entry for issue 91");
    assert!(registry.list(None).is_empty(), "registry must be empty");
}

/// Issue #3808: genuine daemon-crash recovery is preserved. A checkpoint
/// whose issue had a daemon-owned lock with a now-dead owner PID (the
/// daemon dispatched it, then crashed along with its child) IS recovered as
/// a `Crashed` entry so the next dispatch resumes it.
#[test]
fn reconstruct_recovers_daemon_owned_checkpoint() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());

    // Daemon-owned lock with a dead owner PID (crashed daemon + child).
    let locks = registry.config.locks_dir();
    std::fs::create_dir_all(&locks).unwrap();
    let lock = locks.join("issue-91");
    std::fs::create_dir(&lock).unwrap();
    let owner = LockOwner {
        pgid: None,
        model: None,
        effort: None,
        issue: 91,
        owner_pid: 2_147_483_640, // dead
        acquired_at: Utc::now().to_rfc3339(),
        sweep_id: "sweep-issue-91-daemon".to_string(),
    };
    std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap()).unwrap();

    // Matching checkpoint written by the (now-gone) daemon-dispatched child.
    let cp_dir = registry.config.checkpoint_dir();
    std::fs::create_dir_all(&cp_dir).unwrap();
    std::fs::write(cp_dir.join("issue-91.json"), r#"{"phase":"judge","issue":91}"#).unwrap();

    let admitted = registry.reconstruct().unwrap();
    assert!(admitted >= 1, "daemon-owned checkpoint must be recovered");
    let crashed = registry.list(Some(&SweepState::Crashed { at: Utc::now() }));
    assert_eq!(crashed.len(), 1);
    assert_eq!(crashed[0].latest_phase.as_deref(), Some("judge"));
    // The stale daemon lock is cleaned up as part of recovery.
    assert!(!lock.exists(), "stale daemon lock should be removed");
}

// --- ownership-checked lock release (Issue #4463) ---

/// Core invariant: `release_lock_owned` must NOT delete a lock whose
/// `owner.json` records a DIFFERENT `sweep_id` — a newer sweep re-acquired
/// the claim after the releasing (older) sweep died. The lock survives and
/// `Superseded` is returned so the caller skips any re-dispatch. This is the
/// exact double-dispatch mechanism from the incident: reaping an old dead
/// sweep must never free a live sweep's claim.
#[test]
fn release_lock_owned_preserves_lock_owned_by_different_sweep() {
    let dir = tempdir().unwrap();
    let (registry, _record_log) = fixture_registry(dir.path());

    // Newer sweep B owns the lock; older sweep A tries to release it.
    let lock = write_lock_owner(&registry, 4463, "sweep-issue-4463-newer", std::process::id());

    let outcome = registry.release_lock_owned(4463, "sweep-issue-4463-older-dead");
    assert_eq!(outcome, LockReleaseOutcome::Superseded);
    assert!(
        lock.exists(),
        "the newer sweep's live lock must survive an older sweep's release"
    );
    let owner: LockOwner =
        serde_json::from_str(&std::fs::read_to_string(lock.join("owner.json")).unwrap()).unwrap();
    assert_eq!(owner.sweep_id, "sweep-issue-4463-newer", "owner.json must be left untouched");
}

/// A sweep releasing its OWN lock (matching `sweep_id`) removes it —
/// unchanged from the legacy unconditional release for the common case.
#[test]
fn release_lock_owned_removes_matching_owner() {
    let dir = tempdir().unwrap();
    let (registry, _record_log) = fixture_registry(dir.path());

    let lock = write_lock_owner(&registry, 4464, "sweep-issue-4464-mine", std::process::id());

    let outcome = registry.release_lock_owned(4464, "sweep-issue-4464-mine");
    assert_eq!(outcome, LockReleaseOutcome::Released);
    assert!(!lock.exists(), "a sweep must be able to release its own lock");
}

/// FAIL-OPEN: a corrupt / unparseable `owner.json` falls back to the legacy
/// unconditional removal — a garbage lock file must never wedge an issue.
#[test]
fn release_lock_owned_fails_open_on_corrupt_owner() {
    let dir = tempdir().unwrap();
    let (registry, _record_log) = fixture_registry(dir.path());

    let locks = registry.config.locks_dir();
    std::fs::create_dir_all(&locks).unwrap();
    let lock = locks.join("issue-4465");
    std::fs::create_dir_all(&lock).unwrap();
    std::fs::write(lock.join("owner.json"), b"{ this is not valid json").unwrap();

    let outcome = registry.release_lock_owned(4465, "sweep-issue-4465-whoever");
    assert_eq!(outcome, LockReleaseOutcome::Released);
    assert!(!lock.exists(), "a corrupt owner.json must not wedge the lock (fail-open)");
}

/// FAIL-OPEN: a lock dir with a MISSING `owner.json` releases too (legacy
/// spawn-loop locks predating the owner record, or a partially-written one).
#[test]
fn release_lock_owned_fails_open_on_missing_owner_json() {
    let dir = tempdir().unwrap();
    let (registry, _record_log) = fixture_registry(dir.path());

    let locks = registry.config.locks_dir();
    std::fs::create_dir_all(&locks).unwrap();
    let lock = locks.join("issue-4466");
    std::fs::create_dir_all(&lock).unwrap();
    // No owner.json written.

    let outcome = registry.release_lock_owned(4466, "sweep-issue-4466-whoever");
    assert_eq!(outcome, LockReleaseOutcome::Released);
    assert!(!lock.exists(), "a lock with no owner.json must release (fail-open)");
}

/// A non-existent lock is an idempotent no-op (`Released`), never a spurious
/// `Superseded`.
#[test]
fn release_lock_owned_noop_when_no_lock() {
    let dir = tempdir().unwrap();
    let (registry, _record_log) = fixture_registry(dir.path());
    let outcome = registry.release_lock_owned(4467, "sweep-issue-4467-none");
    assert_eq!(outcome, LockReleaseOutcome::Released);
}

/// Release-side half of the fix: a lock whose owner is this very sweep, but
/// whose PID is still a live `/loom:sweep <N>` process, is NOT released —
/// the caller's dead-sweep verdict was wrong (`HolderAlive`), so the label
/// restore and re-dispatch it would have triggered are both skipped.
#[cfg(target_os = "linux")]
#[test]
fn release_lock_owned_refuses_when_the_owner_is_a_live_sweep_process() {
    let dir = tempdir().unwrap();
    let (registry, _record_log) = fixture_registry(dir.path());
    let sweep = FakeSweep::spawn(4564);
    let lock = write_lock_owner(&registry, 4564, "sweep-issue-4564-mine", sweep.pid());

    let outcome = registry.release_lock_owned(4564, "sweep-issue-4564-mine");

    assert_eq!(outcome, LockReleaseOutcome::HolderAlive);
    assert!(outcome.retained(), "HolderAlive must suppress the label restore / re-dispatch");
    assert!(lock.exists(), "a live sweep's lock must survive a false-dead release");
}

/// The inverse: a live PID that is *not* a sweep for this issue (a recycled
/// PID) must NOT block the release. Without this the guard could wedge an
/// issue permanently.
#[test]
fn release_lock_owned_releases_when_a_live_pid_is_not_this_issues_sweep() {
    let dir = tempdir().unwrap();
    let (registry, _record_log) = fixture_registry(dir.path());
    // Our own PID: live, but its argv is the test binary, not `/loom:sweep`.
    let lock = write_lock_owner(&registry, 4565, "sweep-issue-4565-mine", std::process::id());

    let outcome = registry.release_lock_owned(4565, "sweep-issue-4565-mine");
    assert_eq!(outcome, LockReleaseOutcome::Released);
    assert!(!outcome.retained());
    assert!(!lock.exists(), "a recycled PID must not wedge the lock");
}

#[test]
fn lock_release_outcome_retained_covers_both_refusals() {
    assert!(!LockReleaseOutcome::Released.retained());
    assert!(LockReleaseOutcome::Superseded.retained());
    assert!(LockReleaseOutcome::HolderAlive.retained());
}

/// Issue #4173: adoption recovers the real OAuth account from the surviving
/// per-sweep log (the `using OAuth account '<name>'` line anchored to the
/// lock's `sweep_id`), so `status` shows the account, not `unknown`.
#[test]
fn reconstruct_recovers_token_name_from_log() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());

    let locks = registry.config.locks_dir();
    std::fs::create_dir_all(&locks).unwrap();
    let lock = locks.join("issue-401");
    std::fs::create_dir(&lock).unwrap();
    let sweep_id = "sweep-issue-401-adopt";
    let owner = LockOwner {
        pgid: None,
        model: None,
        effort: None,
        issue: 401,
        owner_pid: std::process::id(), // alive → admitted as Running
        acquired_at: Utc::now().to_rfc3339(),
        sweep_id: sweep_id.to_string(),
    };
    std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap()).unwrap();

    // The per-sweep log survived the restart with the dispatch header and
    // the account-selection line the wrapper wrote.
    let log_path = registry.compute_log_path(401);
    std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
    std::fs::write(
        &log_path,
        format!(
            "sweep_id={sweep_id} issue=401 ====\n\
             spawn-claude: using OAuth account 'agent1-2amlogic' (mode=ranking)\n\
             ...build output...\n"
        ),
    )
    .unwrap();

    let admitted = registry.reconstruct().unwrap();
    assert!(admitted >= 1);
    let info = registry.get(sweep_id).unwrap();
    assert_eq!(
        info.token_name, "agent1-2amlogic",
        "adopted sweep must recover its account from the log (#4173)"
    );
}

/// Issue #8056: `record_child_pid_in_lock` stamps the dispatched
/// model/effort into `owner.json`, and `reconstruct` restores them onto the
/// adopted entry — so a sweep that outlives the daemon that dispatched it
/// still reports its real model/effort on the `sweep.outcome` telemetry
/// record instead of two nulls.
#[test]
fn reconstruct_restores_model_and_effort_stamped_at_dispatch() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let sweep_id = "sweep-issue-8056-adopt";

    registry.acquire_lock(8056, sweep_id).unwrap();
    // The dispatch-time stamp: the child's real pid, plus the model and
    // effort that only this daemon instance knows.
    registry
        .record_child_pid_in_lock(
            8056,
            std::process::id(), // alive → admitted as Running
            None,
            Some("claude-opus-5"),
            Some("high"),
        )
        .unwrap();

    let admitted = registry.reconstruct().unwrap();
    assert!(admitted >= 1);
    let info = registry.get(sweep_id).unwrap();
    assert_eq!(info.model.as_deref(), Some("claude-opus-5"));
    assert_eq!(info.effort.as_deref(), Some("high"));
}

/// The other half of the contract: an unset dispatch param stamps nothing,
/// and a pre-#8056 `owner.json` (no keys at all) still parses — an
/// unparseable owner is treated as "no owner" and would drop a LIVE
/// sweep's lock, which is far worse than a missing field.
#[test]
fn reconstruct_leaves_model_and_effort_none_without_a_stamp() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let sweep_id = "sweep-issue-8057-adopt";

    let locks = registry.config.locks_dir();
    std::fs::create_dir_all(&locks).unwrap();
    let lock = locks.join("issue-8057");
    std::fs::create_dir(&lock).unwrap();
    // Exactly the pre-#8056 on-disk shape: no `model`/`effort` keys.
    std::fs::write(
        lock.join("owner.json"),
        format!(
            r#"{{"issue":8057,"owner_pid":{},"acquired_at":"{}","sweep_id":"{sweep_id}"}}"#,
            std::process::id(),
            Utc::now().to_rfc3339()
        ),
    )
    .unwrap();

    let admitted = registry.reconstruct().unwrap();
    assert!(admitted >= 1, "a pre-#8056 owner.json must still be parseable and adoptable");
    let info = registry.get(sweep_id).unwrap();
    assert_eq!(info.model, None, "never a fabricated model");
    assert_eq!(info.effort, None, "never a fabricated effort");
}

/// Issue #4173: a missing log, or a log without the selection line, degrades
/// gracefully to `unknown` — adoption never fails on token capture.
#[test]
fn reconstruct_token_recovery_degrades_to_unknown() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());

    let locks = registry.config.locks_dir();
    std::fs::create_dir_all(&locks).unwrap();

    // Case A: log present but WITHOUT a selection line.
    let lock_a = locks.join("issue-402");
    std::fs::create_dir(&lock_a).unwrap();
    let owner_a = LockOwner {
        pgid: None,
        model: None,
        effort: None,
        issue: 402,
        owner_pid: std::process::id(),
        acquired_at: Utc::now().to_rfc3339(),
        sweep_id: "sweep-issue-402-noline".to_string(),
    };
    std::fs::write(lock_a.join("owner.json"), serde_json::to_string_pretty(&owner_a).unwrap())
        .unwrap();
    let log_a = registry.compute_log_path(402);
    std::fs::create_dir_all(log_a.parent().unwrap()).unwrap();
    std::fs::write(&log_a, "sweep_id=sweep-issue-402-noline issue=402 ====\nno selection\n")
        .unwrap();

    // Case B: no log file at all (rotated/truncated away).
    let lock_b = locks.join("issue-403");
    std::fs::create_dir(&lock_b).unwrap();
    let owner_b = LockOwner {
        pgid: None,
        model: None,
        effort: None,
        issue: 403,
        owner_pid: std::process::id(),
        acquired_at: Utc::now().to_rfc3339(),
        sweep_id: "sweep-issue-403-nolog".to_string(),
    };
    std::fs::write(lock_b.join("owner.json"), serde_json::to_string_pretty(&owner_b).unwrap())
        .unwrap();

    let admitted = registry.reconstruct().unwrap();
    assert!(admitted >= 2, "both sweeps admitted despite unrecoverable token");
    assert_eq!(
        registry.get("sweep-issue-402-noline").unwrap().token_name,
        UNKNOWN_TOKEN_NAME,
        "no selection line → unknown"
    );
    assert_eq!(
        registry.get("sweep-issue-403-nolog").unwrap().token_name,
        UNKNOWN_TOKEN_NAME,
        "missing log → unknown"
    );
}
// ===================================================================
// adopt_live_journal_sweeps — restart survivorship safety net (#6262)
// ===================================================================

fn journal_entry(root: &Path, issue: u32, pid: u32) -> crate::sweep_journal::JournalEntry {
    crate::sweep_journal::JournalEntry {
        repo: root.display().to_string(),
        issue,
        pid,
        started_at: Utc::now(),
    }
}

/// The #6262 gap: a sweep that survived the restart but whose claim lock
/// did NOT survive is invisible to `reconstruct()` (there is nothing on
/// disk for it to read). The journal is the only remaining evidence, and
/// adopting it is what stops the work finder from refilling that slot.
#[test]
fn adopt_live_journal_sweeps_admits_a_survivor_with_no_lock() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let root = registry.config.workspace_root.clone();

    // Precondition: the lock-based pass finds nothing at all.
    assert_eq!(registry.reconstruct().unwrap(), 0);

    let adopted =
        registry.adopt_live_journal_sweeps(&[journal_entry(&root, 6262, std::process::id())]);

    assert_eq!(adopted, 1);
    let info = registry
        .list(Some(&SweepState::Running))
        .into_iter()
        .find(|i| matches!(i.kind, SweepKind::Issue(6262)))
        .expect("the survivor must be a Running entry");
    assert_eq!(info.pid, std::process::id());
    assert_eq!(info.pgid, None, "the journal records no process group");
}

/// The union direction: `reconstruct()` stays the primary mechanism, and a
/// sweep it already recovered from the claim lock must NOT be adopted a
/// second time — double-counting a survivor against the cap would starve a
/// host just as surely as under-counting over-dispatched one.
#[test]
fn adopt_live_journal_sweeps_never_double_counts_a_reconstructed_sweep() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let root = registry.config.workspace_root.clone();

    let locks = registry.config.locks_dir();
    std::fs::create_dir_all(&locks).unwrap();
    let lock = locks.join("issue-6262");
    std::fs::create_dir(&lock).unwrap();
    let owner = LockOwner {
        pgid: None,
        model: None,
        effort: None,
        issue: 6262,
        owner_pid: std::process::id(),
        acquired_at: Utc::now().to_rfc3339(),
        sweep_id: "sweep-issue-6262-locked".to_string(),
    };
    std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap()).unwrap();

    assert_eq!(registry.reconstruct().unwrap(), 1);
    let adopted =
        registry.adopt_live_journal_sweeps(&[journal_entry(&root, 6262, std::process::id())]);

    assert_eq!(adopted, 0, "the lock pass already owns this sweep");
    assert_eq!(
        registry
            .list(Some(&SweepState::Running))
            .into_iter()
            .filter(|i| matches!(i.kind, SweepKind::Issue(6262)))
            .count(),
        1,
        "exactly one entry may hold issue #6262's slot"
    );
}

/// A journal record whose pid is dead is not evidence of anything — it must
/// never inflate occupancy and stall an idle host.
#[test]
fn adopt_live_journal_sweeps_rejects_dead_pids() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let root = registry.config.workspace_root.clone();

    let adopted = registry.adopt_live_journal_sweeps(&[journal_entry(&root, 6262, 2_147_483_640)]);

    assert_eq!(adopted, 0);
    assert!(registry.list(None).is_empty());
}

/// The journal is machine-level and spans every managed repo: a record for
/// a different workspace root must not land in this registry.
#[test]
fn adopt_live_journal_sweeps_ignores_other_workspaces() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let elsewhere = dir.path().join("some-other-repo");
    std::fs::create_dir_all(&elsewhere).unwrap();

    let adopted =
        registry.adopt_live_journal_sweeps(&[journal_entry(&elsewhere, 6262, std::process::id())]);

    assert_eq!(adopted, 0);
    assert!(registry.list(None).is_empty());
}

/// An adopted survivor must be visible to the SAME occupancy accounting the
/// work finder reads (`occupied_issues`), not merely present in `list()` —
/// occupancy is the number that actually bounds dispatch. A survivor
/// mid-Builder has a worktree, which is the #4003 startup-proof signal.
#[test]
fn adopted_survivor_counts_toward_occupancy() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let root = registry.config.workspace_root.clone();
    std::fs::create_dir_all(root.join(".loom").join("worktrees").join("issue-6262")).unwrap();

    assert_eq!(
        registry.adopt_live_journal_sweeps(&[journal_entry(&root, 6262, std::process::id())]),
        1
    );

    assert!(
        registry.occupied_issues().contains(&6262),
        "an adopted survivor must occupy a concurrency slot"
    );
}

// ---------------------------------------------------------------------------
// Issue #8720 — `tracked_sweep_identity`: the adoption evidence the
// observability collector consults when a post-restart lifecycle event has no
// in-process dispatch to correlate against.
// ---------------------------------------------------------------------------

/// Lock-based adoption is the path that genuinely retains the ORIGINAL
/// dispatch id, so the evidence it yields must be that id and the lock's own
/// `acquired_at` — never a value minted at read time.
#[test]
fn lock_adoption_evidence_names_the_original_sweep_id_and_start() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let acquired_at = Utc::now() - chrono::Duration::seconds(600);
    let lock = registry.config.locks_dir().join("issue-8720");
    std::fs::create_dir_all(&lock).unwrap();
    let owner = LockOwner {
        pgid: None,
        model: None,
        effort: None,
        issue: 8720,
        owner_pid: std::process::id(),
        acquired_at: acquired_at.to_rfc3339(),
        sweep_id: "sweep-issue-8720-original".to_string(),
    };
    std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap()).unwrap();

    assert!(registry.reconstruct().unwrap() >= 1);
    let identity = registry.tracked_sweep_identity(8720).unwrap();
    assert_eq!(identity.sweep_id, "sweep-issue-8720-original");
    assert_eq!(identity.started_at.timestamp(), acquired_at.timestamp());
    // Nothing is claimed about an issue this registry does not track.
    assert!(registry.tracked_sweep_identity(8721).is_none());
}

/// Journal-only recovery is covered separately because the original dispatch
/// id is unrecoverable there: the evidence is the `journal-adopted-…` id the
/// registry itself reports for that sweep everywhere else.
#[test]
fn journal_adoption_evidence_names_the_id_the_registry_reports() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let root = registry.config.workspace_root.clone();
    let pid = std::process::id();
    assert_eq!(registry.adopt_live_journal_sweeps(&[journal_entry(&root, 6262, pid)]), 1);
    assert_eq!(
        registry.tracked_sweep_identity(6262).map(|i| i.sweep_id),
        Some(format!("journal-adopted-issue-6262-{pid}"))
    );
}

/// A finished sweep's id is not evidence about a later event for the same
/// issue number — re-attaching it is exactly how a retired row would be
/// resurrected downstream. The honest unknown fallback is correct here.
#[test]
fn a_terminal_entry_is_not_adoption_evidence() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let root = registry.config.workspace_root.clone();
    assert_eq!(
        registry.adopt_live_journal_sweeps(&[journal_entry(&root, 6262, std::process::id())]),
        1
    );
    assert!(registry.tracked_sweep_identity(6262).is_some());

    let sweep_id = registry.list(None)[0].sweep_id.clone();
    registry.entries.get_mut(&sweep_id).unwrap().state = SweepState::Exited {
        code: Some(0),
        at: Utc::now(),
    };
    assert!(registry.tracked_sweep_identity(6262).is_none());
}

/// Two live candidates for one issue would make any answer a coin flip, and a
/// mis-attributed live sweep is worse than the synthesized fallback the caller
/// already has. (The registry's own invariants make this near-unreachable —
/// pinned so it stays a deliberate decline rather than an arbitrary pick.)
#[test]
fn ambiguous_live_entries_decline_to_name_an_authoritative_id() {
    let dir = tempdir().unwrap();
    let (mut registry, _record_log) = fixture_registry(dir.path());
    let root = registry.config.workspace_root.clone();
    assert_eq!(
        registry.adopt_live_journal_sweeps(&[journal_entry(&root, 6262, std::process::id())]),
        1
    );
    let mut duplicate = registry.list(None)[0].clone();
    duplicate.sweep_id = "a-second-live-entry-for-the-same-issue".to_string();
    registry
        .entries
        .insert(duplicate.sweep_id.clone(), duplicate);

    assert!(registry.tracked_sweep_identity(6262).is_none());
}
