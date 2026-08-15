//! Startup seeding of in-flight/capacity accounting for sweeps that survived a
//! daemon restart (Issue #6262).
//!
//! ## The incident
//!
//! 2026-08-14 (robb-studio): after several same-day daemon restarts the host was
//! observed running ~28 concurrent sweeps against a configured cap of 12. A
//! restart deliberately leaves in-flight sweeps running (they are detached
//! children), but every restart rebuilds the daemon's capacity accounting from
//! scratch — so any survivor the rebuild misses is a slot the work finder
//! believes is free and immediately refills. Repeat that across three restarts
//! and the survivors stack.
//!
//! ## What already existed, and where it fell short
//!
//! [`SweepRegistry::reconstruct`](crate::sweep_registry::SweepRegistry::reconstruct)
//! is the primary recovery path and remains so: it re-adopts a sweep from its
//! `.loom/locks/issue-<N>/owner.json` claim lock, recovering the sweep id,
//! dispatch timestamp, token attribution, runtime, and process group — none of
//! which any other source records. Its blind spot is narrow but total:
//!
//! **A survivor whose claim lock did not survive is invisible to it, and
//! nothing later re-adopts it.** `reconstruct` deletes a lock dir whose
//! `owner.json` is missing or unparseable *without ever asking whether a
//! process is still running for that issue*, and any path that released the
//! lock while the child kept running (an operator `loom-clean`, a mid-build
//! watchdog takeover, a partially completed release) leaves a live sweep with
//! no lock at all. [`crate::claim_reconciliation`] — the periodic pass the
//! incident report assumed was the 30-minute backstop — does the opposite job:
//! it reconciles the forge labels of sweeps it can prove are **dead**, and has
//! no path that re-admits a live one into capacity accounting. So an unlocked
//! survivor's slot reads as free for the entire life of the daemon, and the
//! work finder dispatches on top of it.
//!
//! ## What this module adds
//!
//! One synchronous startup pass, run **before any dispatch producer is
//! spawned**, that for every registered root:
//!
//! 1. Provisions the root's `SweepRegistry` (and therefore runs its lock-based
//!    `reconstruct()`) at one deterministic point, rather than leaving it to
//!    whichever consumer happens to touch that root first. This is not itself a
//!    bug fix — [`crate::ipc::count_in_flight_sweeps`] and the work-finder tick
//!    both provision on demand — it is what makes the seed below a single,
//!    observable startup fact instead of a side effect of the first tick.
//! 2. Unions in any still-live sweep the machine-level journal
//!    (`~/.loom/sweeps.json`, [`crate::sweep_journal`]) records for that root
//!    which the lock pass did not recover. The journal is host-global,
//!    pid-keyed, written by every `dispatch()`, and survives exactly the
//!    restart that wipes the in-memory registry — it is the only remaining
//!    evidence for an unlocked survivor.
//!
//! The union direction matters: journal adoption is a *safety net under* the
//! lock pass, never a replacement for it. An issue the lock pass already
//! admitted is skipped, so occupancy can never be double-counted, and every
//! richer field the lock carries is preserved.
//!
//! The adopted count is recorded in a process-global counter so
//! `loom-daemon status` can show that the accounting was seeded (and by how
//! much) rather than leaving an operator to infer it from the log.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::sweep_registry::is_pid_alive;
use crate::workspace_pool::WorkspacePool;

/// How many surviving sweeps the startup pass adopted from the machine journal.
/// Read by the `DaemonStatus` report builder (#6262 AC4).
static JOURNAL_ADOPTED_AT_STARTUP: AtomicUsize = AtomicUsize::new(0);

/// The number of surviving sweeps [`seed_capacity_from_journal`] adopted from
/// the machine-level sweep journal at daemon startup.
///
/// `0` on a daemon that started with an empty host (the common case) and on any
/// daemon that has not run the pass yet.
#[must_use]
pub fn journal_adopted_at_startup() -> usize {
    JOURNAL_ADOPTED_AT_STARTUP.load(Ordering::Relaxed)
}

/// Reset the counter. Test-only seam — the daemon runs the startup pass exactly
/// once per process.
#[cfg(test)]
pub(crate) fn reset_journal_adopted_at_startup() {
    JOURNAL_ADOPTED_AT_STARTUP.store(0, Ordering::Relaxed);
}

/// What one [`seed_capacity_from_journal`] pass did, for logging and tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StartupAdoption {
    /// Registered roots whose `SweepRegistry` this pass provisioned or touched.
    pub roots: usize,
    /// Live journal entries adopted into capacity accounting because the
    /// lock-based `reconstruct()` had not already recovered them.
    pub adopted: usize,
    /// Live journal entries that were already tracked by the lock-based pass —
    /// i.e. the union skipped them. Reported so an operator can see the primary
    /// mechanism doing its job rather than only the safety net's residue.
    pub already_tracked: usize,
}

/// Seed every registered root's in-flight/capacity accounting from disk before
/// the work finder's first tick (Issue #6262). See the module docs for why the
/// lock-based `reconstruct()` alone was not a sufficient seed.
///
/// Best-effort throughout: an unresolvable/corrupt journal degrades to "no
/// journal evidence" (an empty entry list), which reduces this pass to nothing
/// more than eagerly provisioning each root — never an error that could block
/// daemon startup.
pub fn seed_capacity_from_journal(pool: &Arc<WorkspacePool>, roots: &[PathBuf]) -> StartupAdoption {
    let journal_path = match crate::sweep_journal::default_journal_path() {
        Ok(p) => Some(p),
        Err(e) => {
            log::warn!(
                "startup_adoption: could not resolve the machine sweep journal path ({e}) — \
                 seeding capacity from claim locks only (#6262)"
            );
            None
        }
    };
    seed_capacity_from_journal_at(pool, roots, journal_path.as_deref())
}

/// [`seed_capacity_from_journal`] with an explicit journal path — the test seam
/// (and the path a caller with a non-default journal location would use).
///
/// `journal_path == None` means "no journal evidence available"; the pass still
/// provisions every root, so the lock-based `reconstruct()` runs eagerly.
pub fn seed_capacity_from_journal_at(
    pool: &Arc<WorkspacePool>,
    roots: &[PathBuf],
    journal_path: Option<&Path>,
) -> StartupAdoption {
    let live_entries = journal_path.map_or_else(Vec::new, |path| {
        let mut journal = crate::sweep_journal::load(path);
        let pruned = crate::sweep_journal::prune_dead(&mut journal, is_pid_alive);
        if pruned > 0 {
            log::debug!(
                "startup_adoption: ignored {pruned} journal record(s) whose pid is no longer alive"
            );
        }
        journal.entries
    });

    let mut result = StartupAdoption {
        roots: roots.len(),
        ..StartupAdoption::default()
    };

    for root in roots {
        let registry = pool.get_or_provision(root);
        let mut registry = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = registry.list(None).len();
        let adopted = registry.adopt_live_journal_sweeps(&live_entries);
        debug_assert_eq!(registry.list(None).len(), before + adopted);
        result.adopted += adopted;
    }

    // Every live journal record that named a managed root but was NOT adopted
    // was already tracked by the lock-based pass. Counted separately so the
    // startup log distinguishes "the lock pass covered everything" (the healthy
    // shape) from "the lock pass covered nothing" (the #6262 failure shape).
    let live_for_managed_roots = live_entries
        .iter()
        .filter(|e| roots.iter().any(|r| repo_matches_root(r, &e.repo)))
        .count();
    result.already_tracked = live_for_managed_roots.saturating_sub(result.adopted);

    JOURNAL_ADOPTED_AT_STARTUP.store(result.adopted, Ordering::Relaxed);

    if result.adopted > 0 {
        log::warn!(
            "startup_adoption: adopted {} surviving sweep(s) from the machine journal at startup \
             across {} root(s) ({} more were already recovered from claim locks) — capacity \
             accounting is seeded before the first work-finder tick (#6262)",
            result.adopted,
            result.roots,
            result.already_tracked
        );
    } else {
        log::info!(
            "startup_adoption: seeded capacity accounting across {} root(s) before the first \
             work-finder tick; {} surviving sweep(s) recovered from claim locks, 0 needed \
             journal adoption (#6262)",
            result.roots,
            result.already_tracked
        );
    }

    result
}

/// Whether a journal record's `repo` string names `root`. Mirrors the
/// registry-side comparison in `sweep_registry::locks` (exact string first,
/// canonicalized fallback for symlinked roots).
fn repo_matches_root(root: &Path, repo: &str) -> bool {
    let repo_path = Path::new(repo);
    if root == repo_path {
        return true;
    }
    match (root.canonicalize(), repo_path.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::event_bus::EventBus;
    use crate::sweep_journal::{JournalEntry, SweepJournal};
    use crate::types::{SweepKind, SweepState};
    use serial_test::serial;

    fn write_journal(path: &Path, entries: Vec<JournalEntry>) {
        let journal = SweepJournal {
            version: crate::sweep_journal::JOURNAL_VERSION,
            entries,
        };
        std::fs::write(path, serde_json::to_string_pretty(&journal).unwrap()).unwrap();
    }

    fn live_entry(repo: &Path, issue: u32) -> JournalEntry {
        JournalEntry {
            repo: repo.display().to_string(),
            issue,
            pid: std::process::id(), // guaranteed alive
            started_at: chrono::Utc::now(),
        }
    }

    fn pool() -> Arc<WorkspacePool> {
        Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), tokio::runtime::Handle::current()))
    }

    /// The core #6262 property: a sweep that survived the restart but whose
    /// claim lock did NOT survive is still seeded into capacity accounting.
    #[tokio::test]
    #[serial]
    async fn adopts_live_journal_survivor_with_no_claim_lock() {
        reset_journal_adopted_at_startup();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let journal_path = dir.path().join("sweeps.json");
        write_journal(&journal_path, vec![live_entry(&root, 6262)]);

        let pool = pool();
        let result =
            seed_capacity_from_journal_at(&pool, std::slice::from_ref(&root), Some(&journal_path));

        assert_eq!(result.adopted, 1, "the lock-less survivor must be adopted");
        assert_eq!(journal_adopted_at_startup(), 1);

        let registry = pool.get_or_provision(&root);
        let registry = registry.lock().unwrap();
        let running = registry.list(Some(&SweepState::Running));
        assert_eq!(running.len(), 1);
        assert!(matches!(running[0].kind, SweepKind::Issue(6262)));
        assert_eq!(running[0].pid, std::process::id());
    }

    /// A journal record whose pid is dead must never inflate occupancy.
    #[tokio::test]
    #[serial]
    async fn ignores_journal_records_whose_pid_is_dead() {
        reset_journal_adopted_at_startup();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let journal_path = dir.path().join("sweeps.json");
        write_journal(
            &journal_path,
            vec![JournalEntry {
                repo: root.display().to_string(),
                issue: 6263,
                pid: 2_147_483_640, // never a live pid
                started_at: chrono::Utc::now(),
            }],
        );

        let pool = pool();
        let result =
            seed_capacity_from_journal_at(&pool, std::slice::from_ref(&root), Some(&journal_path));

        assert_eq!(result.adopted, 0);
        assert_eq!(journal_adopted_at_startup(), 0);
    }

    /// The journal is machine-level: a record for a repo this daemon does not
    /// manage must not be adopted into any registry.
    #[tokio::test]
    #[serial]
    async fn ignores_journal_records_for_other_repos() {
        reset_journal_adopted_at_startup();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("mine");
        let other = dir.path().join("theirs");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let journal_path = dir.path().join("sweeps.json");
        write_journal(&journal_path, vec![live_entry(&other, 999)]);

        let pool = pool();
        let result =
            seed_capacity_from_journal_at(&pool, std::slice::from_ref(&root), Some(&journal_path));

        assert_eq!(result.adopted, 0);
        let registry = pool.get_or_provision(&root);
        let registry = registry.lock().unwrap();
        assert!(registry.list(None).is_empty());
    }

    /// The pass is idempotent: running it twice must not double-count the same
    /// survivor against the concurrency budget.
    #[tokio::test]
    #[serial]
    async fn adoption_is_idempotent() {
        reset_journal_adopted_at_startup();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let journal_path = dir.path().join("sweeps.json");
        write_journal(&journal_path, vec![live_entry(&root, 6264)]);

        let pool = pool();
        let first =
            seed_capacity_from_journal_at(&pool, std::slice::from_ref(&root), Some(&journal_path));
        let second =
            seed_capacity_from_journal_at(&pool, std::slice::from_ref(&root), Some(&journal_path));

        assert_eq!(first.adopted, 1);
        assert_eq!(second.adopted, 0, "the second pass must find it already tracked");
        assert_eq!(second.already_tracked, 1);

        let registry = pool.get_or_provision(&root);
        let registry = registry.lock().unwrap();
        assert_eq!(registry.list(Some(&SweepState::Running)).len(), 1);
    }

    /// A missing journal degrades to "no evidence" rather than erroring — the
    /// pass still provisions every root so the lock-based `reconstruct()` runs
    /// eagerly instead of lazily inside the first tick.
    #[tokio::test]
    #[serial]
    async fn missing_journal_still_provisions_every_root() {
        reset_journal_adopted_at_startup();
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        let pool = pool();
        let roots = vec![a.clone(), b.clone()];
        let result =
            seed_capacity_from_journal_at(&pool, &roots, Some(&dir.path().join("absent.json")));

        assert_eq!(result.adopted, 0);
        assert_eq!(result.roots, 2);
        for root in [&a, &b] {
            let registry = pool.get_or_provision(root);
            let registry = registry.lock().unwrap();
            assert!(
                registry.list(None).is_empty(),
                "{} must be provisioned and idle",
                root.display()
            );
        }
    }

    /// The cross-root in-flight tally (`count_in_flight_sweeps`) — the input the
    /// saturation brake, the drain supervisor, and the auto-update gate all read
    /// — must already see every root's survivors once this pass has run, rather
    /// than waiting for the first work-finder tick to provision them.
    #[tokio::test]
    #[serial]
    async fn cross_root_in_flight_count_is_seeded_before_the_first_tick() {
        reset_journal_adopted_at_startup();
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let journal_path = dir.path().join("sweeps.json");
        write_journal(
            &journal_path,
            vec![live_entry(&a, 11), live_entry(&a, 12), live_entry(&b, 13)],
        );

        let pool = pool();
        let roots = vec![a.clone(), b.clone()];
        let result = seed_capacity_from_journal_at(&pool, &roots, Some(&journal_path));

        assert_eq!(result.adopted, 3);
        // The cross-root tally every capacity consumer reads is the sum of each
        // provisioned registry's non-terminal entries (what
        // `ipc::count_in_flight_sweeps` computes over the *host's* registered
        // roots — re-derived here over this test's roots so the assertion never
        // touches the real `~/.loom/workspaces.json`).
        let total: usize = [&a, &b]
            .into_iter()
            .map(|root| {
                let registry = pool.get_or_provision(root);
                let registry = registry.lock().unwrap();
                registry
                    .list(None)
                    .into_iter()
                    .filter(|i| !i.state.is_terminal())
                    .count()
            })
            .sum();
        assert_eq!(total, 3, "all three survivors must be visible across both roots");
    }
}
