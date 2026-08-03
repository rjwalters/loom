//! Reap a process tree that outlived the sweep that spawned it (Issue #5110).
//!
//! # Why this exists
//!
//! [`crate::sweep_registry::reaper`]'s pgid-scoped teardown (#4980) signals a
//! sweep's whole process **group** on cancel/crash, which reaches every
//! descendant that stayed in that group. It does NOT reach a descendant that
//! escaped into a fresh process group or session — which is exactly what GNU
//! `timeout` does to its child unless invoked with `--foreground` (its own
//! `--help` documents the distinction). A multi-level driver script
//! (`bash run_all.sh` → `python3` → `timeout` → `ngspice`) can therefore
//! survive a pgid-scoped kill entirely, and if the sweep that launched it
//! already died, nothing ever asks "is anything still running for this
//! issue?" — the process keeps consuming a whole host's CPU indefinitely
//! (the 2026-08-03 incident this issue documents: an orphaned driver held a
//! worker host at load 65 for 5h52m, starving that host's own dispatched
//! sweep).
//!
//! # What this module does instead
//!
//! Rather than trusting any recorded pgid, this asks the same "worktree
//! ownership" question [`crate::worktree_reaper`] already asks about
//! *directories* — "is anything still using this worktree, and does a live
//! sweep still own it?" — but about **processes**:
//!
//! 1. For every `issue-<N>` worktree, find PIDs with their cwd inside it
//!    ([`crate::worktree_ops::safety::find_processes_using_directory`] — the
//!    same probe [`crate::worktree_ops::clean::classify_worktree`] already
//!    uses to protect a worktree with live work from removal).
//! 2. Skip it unless BOTH of this issue's acceptance-criteria gates hold:
//!    the worktree carries the `.loom-managed` sentinel (never touch a
//!    user-provisioned worktree), and issue N has NO live sweep claim
//!    (checked the same way [`crate::worktree_ops::liveness::active_spawn_loop_issues`]
//!    already does for the removal pass — a claim-lock directory or spawn-loop
//!    state entry).
//! 3. For every surviving candidate PID, walk its full descendant tree by
//!    PID (`pgrep -P`, recursive) — **not** scoped to any process group or
//!    session, so a tree that escaped via `setsid`/`timeout` into a fresh
//!    pgid+sid is still fully reached — and terminate every PID found
//!    (SIGTERM, then SIGKILL for anything still alive after a short grace).
//!
//! This is the module-level precedent [`crate::terminal`]'s
//! `collect_descendants`/`kill_process_tree` already established for tmux
//! pane teardown — generalized here to a bare PID input/output so it is
//! reusable outside tmux, and logged so an operator can see what was killed
//! (this issue's AC).

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::worktree_ops::clean::is_loom_managed;
use crate::worktree_ops::liveness::active_spawn_loop_issues;
use crate::worktree_ops::naming::issue_from_worktree;
use crate::worktree_ops::safety::find_processes_using_directory;

/// Grace between SIGTERM and SIGKILL escalation for a reaped orphan tree.
/// Short — unlike the crash-path group reap (#4980, deferred to a later
/// reaper tick to avoid blocking the registry mutex), this pass runs on its
/// own `spawn_blocking` task (mirroring [`crate::worktree_reaper::reap_repo`])
/// so a brief inline sleep is fine.
pub const ORPHAN_PROCESS_REAP_GRACE: Duration = Duration::from_secs(3);

// ============================================================================
// Process-tree primitives (generalizes `terminal.rs`'s tmux-only
// `collect_descendants` / `kill_process_tree` to a bare PID)
// ============================================================================

/// Whether `pid` is still alive, via `kill -0` (portable across macOS/Linux,
/// matching the shell-based approach the rest of this module and
/// `terminal.rs::kill_process_tree` already use).
#[must_use]
pub fn is_pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Recursively collect every descendant PID of `pid` (depth-first,
/// grandchildren before children), via repeated `pgrep -P`.
///
/// Deliberately **not** scoped to any process group or session — that is the
/// entire point (#5110): a tree that called `setsid`/was launched by
/// `timeout` into a fresh pgid+sid is still walked correctly, because this
/// follows the OS parent/child (`ppid`) relationship, which nothing in
/// between can escape short of re-parenting to init.
#[must_use]
pub fn collect_descendant_pids(pid: u32) -> Vec<u32> {
    let mut pids = Vec::new();
    collect_descendant_pids_into(pid, &mut pids);
    pids
}

fn collect_descendant_pids_into(pid: u32, out: &mut Vec<u32>) {
    let Ok(output) = Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let children: Vec<u32> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    for child in children {
        collect_descendant_pids_into(child, out);
        out.push(child);
    }
}

/// SIGTERM every pid in `pids`, then SIGKILL any survivor after `grace`.
///
/// Best-effort throughout: a pid that has already exited between discovery
/// and signal delivery is simply not reported as signaled (never an error —
/// "already gone" is the goal, not a failure). Returns the pids that were
/// actually signaled at least once.
pub fn kill_pids(pids: &[u32], grace: Duration) -> Vec<u32> {
    let mut signaled = Vec::new();
    for &pid in pids {
        if Command::new("kill")
            .args(["-15", &pid.to_string()])
            .output()
            .is_ok_and(|o| o.status.success())
        {
            signaled.push(pid);
        }
    }
    if signaled.is_empty() {
        return signaled;
    }
    std::thread::sleep(grace);
    for &pid in &signaled {
        if is_pid_alive(pid) {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
        }
    }
    signaled
}

// ============================================================================
// The reap pass
// ============================================================================

/// One worktree's orphan-process outcome.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrphanReapEntry {
    /// The issue number the worktree belongs to.
    pub issue: u32,
    /// PIDs found with cwd inside the worktree — the roots of each tree walked.
    pub root_pids: Vec<u32>,
    /// Every PID (roots + transitive descendants) that was signaled.
    pub killed_pids: Vec<u32>,
}

/// What one orphan-process reap pass over one repo did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrphanReapReport {
    /// `issue-<N>` worktree directories examined.
    pub scanned: usize,
    /// Worktrees that had at least one orphan process reaped.
    pub reaped: Vec<OrphanReapEntry>,
}

impl OrphanReapReport {
    /// A compact one-line summary for the daemon log.
    #[must_use]
    pub fn summary(&self) -> String {
        format!("scanned={} reaped_worktrees={}", self.scanned, self.reaped.len())
    }
}

/// Scan `repo_root`'s worktree root for `issue-<N>` worktrees with a process
/// whose cwd is inside them but NO live sweep claim for that issue, and
/// terminate the whole descendant tree of every such process.
///
/// The probes are injected (`find_processes`/`collect_descendants`/`kill`) so
/// the whole pass is unit-testable without a live process table — production
/// wiring is [`reap_orphan_processes`].
///
/// # Fail-safe (this issue's AC)
///
/// A worktree is only ever a reap candidate when BOTH gates hold:
/// - it carries the `.loom-managed` sentinel — the same "never touch a
///   user-provisioned worktree" gate [`crate::worktree_reaper`]'s removal
///   pass applies via `CleanOptions::require_managed_sentinel`, and
/// - `active_issues` does NOT contain its issue number — the same live-claim
///   check the removal pass's `SkipInUse` gate already makes via
///   [`crate::worktree_ops::liveness::active_spawn_loop_issues`].
///
/// Both are re-checked, not assumed from an earlier pass's snapshot: a stale
/// verdict here has an irreversible consequence (a live sweep's own process
/// killed out from under it), unlike a stale worktree-removal verdict (a
/// no-op retried on the next tick).
pub fn reap_orphan_processes_with(
    repo_root: &Path,
    active_issues: &HashSet<u32>,
    find_processes: &dyn Fn(&Path) -> Vec<u32>,
    collect_descendants: &dyn Fn(u32) -> Vec<u32>,
    kill: &dyn Fn(&[u32]) -> Vec<u32>,
) -> OrphanReapReport {
    let mut report = OrphanReapReport::default();

    let worktrees_dir = crate::worktree_root::worktree_root(repo_root);
    let Ok(entries) = std::fs::read_dir(&worktrees_dir) else {
        // No worktree root yet (or unreadable) — nothing to reap, not an error.
        return report;
    };

    let mut worktree_dirs: Vec<_> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .collect();
    worktree_dirs.sort_by_key(std::fs::DirEntry::path);

    for entry in worktree_dirs {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(issue) = issue_from_worktree(&name) else {
            continue;
        };
        report.scanned += 1;

        let worktree_path = entry.path().canonicalize().unwrap_or_else(|_| entry.path());

        // Fail-safe gate 1: never touch a user-provisioned worktree.
        if !is_loom_managed(&worktree_path) {
            continue;
        }
        // Fail-safe gate 2: never touch a worktree with a live sweep claim.
        if active_issues.contains(&issue) {
            continue;
        }

        let root_pids = find_processes(&worktree_path);
        if root_pids.is_empty() {
            continue;
        }

        let mut all_pids: Vec<u32> = Vec::new();
        for &root in &root_pids {
            for descendant in collect_descendants(root) {
                if !all_pids.contains(&descendant) {
                    all_pids.push(descendant);
                }
            }
        }
        for &root in &root_pids {
            if !all_pids.contains(&root) {
                all_pids.push(root);
            }
        }

        let killed = kill(&all_pids);
        log::warn!(
            "orphan_process_reaper: {} issue-{issue} has {} process(es) with no live sweep \
             claim for issue #{issue} — reaping the whole tree: roots={root_pids:?} \
             all_pids={all_pids:?} killed={killed:?} (#5110)",
            repo_root.display(),
            root_pids.len()
        );
        report.reaped.push(OrphanReapEntry {
            issue,
            root_pids,
            killed_pids: killed,
        });
    }

    report
}

/// Run one production orphan-process reap pass over `repo_root`. Wires the
/// real process-table probes and the real signal delivery.
#[must_use]
pub fn reap_orphan_processes(repo_root: &Path) -> OrphanReapReport {
    let active_issues = active_spawn_loop_issues(repo_root);
    let kill_fn = |pids: &[u32]| kill_pids(pids, ORPHAN_PROCESS_REAP_GRACE);
    reap_orphan_processes_with(
        repo_root,
        &active_issues,
        &find_processes_using_directory,
        &collect_descendant_pids,
        &kill_fn,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs;

    /// Build a repo root with `.loom/worktrees/issue-<N>` directories, each
    /// carrying a `.loom-managed` sentinel unless `managed` says otherwise.
    fn make_repo(issues: &[(u32, bool)]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for (issue, managed) in issues {
            let wt = tmp
                .path()
                .join(".loom/worktrees")
                .join(format!("issue-{issue}"));
            fs::create_dir_all(&wt).unwrap();
            if *managed {
                fs::write(wt.join(".loom-managed"), "").unwrap();
            }
        }
        tmp
    }

    #[test]
    fn test_orphan_with_no_live_claim_is_reaped_transitively() {
        let repo = make_repo(&[(100, true)]);
        let killed_calls: RefCell<Vec<Vec<u32>>> = RefCell::new(Vec::new());

        let find_processes = |_: &Path| vec![111];
        // 111 -> 222 -> 333 (a driver that escaped into a new pgid/sid via a
        // `timeout`-shaped middle process — the scenario this issue's AC
        // requires a fixture for).
        let collect_descendants = |pid: u32| if pid == 111 { vec![222, 333] } else { vec![] };
        let kill = |pids: &[u32]| {
            killed_calls.borrow_mut().push(pids.to_vec());
            pids.to_vec()
        };

        let report = reap_orphan_processes_with(
            repo.path(),
            &HashSet::new(),
            &find_processes,
            &collect_descendants,
            &kill,
        );

        assert_eq!(report.scanned, 1);
        assert_eq!(report.reaped.len(), 1);
        let entry = &report.reaped[0];
        assert_eq!(entry.issue, 100);
        assert_eq!(entry.root_pids, vec![111]);
        // The whole tree — root AND every descendant — must be in the kill set,
        // not just the root pid.
        let mut all: Vec<u32> = entry.killed_pids.clone();
        all.sort_unstable();
        assert_eq!(all, vec![111, 222, 333]);
    }

    #[test]
    fn test_live_sweep_claim_is_never_reaped() {
        let repo = make_repo(&[(101, true)]);
        let find_processes = |_: &Path| vec![999];
        let collect_descendants = |_: u32| vec![];
        let killed_calls: RefCell<Vec<Vec<u32>>> = RefCell::new(Vec::new());
        let kill = |pids: &[u32]| {
            killed_calls.borrow_mut().push(pids.to_vec());
            pids.to_vec()
        };

        let report = reap_orphan_processes_with(
            repo.path(),
            &HashSet::from([101]),
            &find_processes,
            &collect_descendants,
            &kill,
        );

        assert!(report.reaped.is_empty(), "a live sweep's work must never be reaped");
        assert!(killed_calls.borrow().is_empty());
    }

    #[test]
    fn test_unmanaged_worktree_is_never_reaped() {
        // No `.loom-managed` sentinel ⇒ user-provisioned ⇒ never touched,
        // even though a process is using it and no claim is live.
        let repo = make_repo(&[(102, false)]);
        let find_processes = |_: &Path| vec![999];
        let collect_descendants = |_: u32| vec![];
        let kill = |pids: &[u32]| pids.to_vec();

        let report = reap_orphan_processes_with(
            repo.path(),
            &HashSet::new(),
            &find_processes,
            &collect_descendants,
            &kill,
        );

        assert!(report.reaped.is_empty());
    }

    #[test]
    fn test_no_processes_using_worktree_is_a_no_op() {
        let repo = make_repo(&[(103, true)]);
        let find_processes = |_: &Path| Vec::new();
        let collect_descendants = |_: u32| vec![];
        let kill = |pids: &[u32]| pids.to_vec();

        let report = reap_orphan_processes_with(
            repo.path(),
            &HashSet::new(),
            &find_processes,
            &collect_descendants,
            &kill,
        );

        assert_eq!(report.scanned, 1);
        assert!(report.reaped.is_empty());
    }

    #[test]
    fn test_missing_worktree_root_is_a_clean_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let find_processes = |_: &Path| vec![999];
        let collect_descendants = |_: u32| vec![];
        let kill = |pids: &[u32]| pids.to_vec();

        let report = reap_orphan_processes_with(
            tmp.path(),
            &HashSet::new(),
            &find_processes,
            &collect_descendants,
            &kill,
        );

        assert_eq!(report, OrphanReapReport::default());
    }

    #[test]
    fn test_non_issue_directories_are_ignored() {
        let repo = make_repo(&[(200, true)]);
        fs::create_dir_all(repo.path().join(".loom/worktrees/scratch")).unwrap();
        let find_processes = |_: &Path| Vec::new();
        let collect_descendants = |_: u32| vec![];
        let kill = |pids: &[u32]| pids.to_vec();

        let report = reap_orphan_processes_with(
            repo.path(),
            &HashSet::new(),
            &find_processes,
            &collect_descendants,
            &kill,
        );

        assert_eq!(report.scanned, 1);
    }

    #[test]
    fn test_summary_is_compact() {
        let report = OrphanReapReport {
            scanned: 3,
            reaped: vec![OrphanReapEntry {
                issue: 1,
                root_pids: vec![10],
                killed_pids: vec![10, 11],
            }],
        };
        assert_eq!(report.summary(), "scanned=3 reaped_worktrees=1");
    }

    // ========================================================================
    // Real-process fixtures: proves the reap actually kills a live tree that
    // escaped via `setsid` into a fresh pgid+sid (this issue's explicit AC:
    // "Test/fixture covers a tree that has escaped via `timeout` (new pgid +
    // new sid)"). `timeout` itself does exactly this on both macOS (via
    // coreutils, if installed) and Linux, but `setsid` is the portable,
    // always-present primitive that reproduces the SAME escape shape — a
    // subprocess launched into a brand-new process group AND session, which
    // is precisely why #4982's pgid-scoped teardown cannot reach it.
    // ========================================================================

    fn wait_until<F: FnMut() -> bool>(mut cond: F, timeout_ms: u64) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed().as_millis() < u128::from(timeout_ms) {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        cond()
    }

    #[test]
    fn test_kill_pids_terminates_a_setsid_escaped_grandchild() {
        // `setsid <cmd>` runs <cmd> as the leader of a brand-new session AND
        // process group — the same escape `timeout` (without --foreground)
        // performs on its child, per the issue's ps-table evidence. The
        // leader then forks ITS OWN child (a grandchild relative to us) that
        // shares the new group/session, mirroring the ngspice-under-timeout
        // shape.
        if Command::new("setsid").arg("true").output().is_err() {
            eprintln!("skipping: `setsid` not available on this platform");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let gc_pidfile = dir.path().join("grandchild.pid");
        let script = format!("sleep 300 & echo $! > {}; exec sleep 300", gc_pidfile.display());

        let mut child = Command::new("setsid")
            .args(["bash", "-c", &script])
            .spawn()
            .unwrap();
        let leader_pid = child.id();

        assert!(wait_until(|| gc_pidfile.exists(), 2000), "grandchild pid file should appear");
        let gc_pid: u32 = fs::read_to_string(&gc_pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_ne!(leader_pid, gc_pid);
        assert!(is_pid_alive(leader_pid));
        assert!(is_pid_alive(gc_pid));

        // The whole point: walk descendants by PID (NOT by pgid/sid) and kill
        // every one — this must reach the grandchild despite it living in a
        // process group/session this test process never joined.
        let mut all_pids = collect_descendant_pids(leader_pid);
        all_pids.push(leader_pid);
        assert!(
            all_pids.contains(&gc_pid),
            "descendant walk must find the setsid-escaped grandchild: {all_pids:?}"
        );

        let killed = kill_pids(&all_pids, Duration::from_secs(2));
        assert!(killed.contains(&leader_pid));
        assert!(killed.contains(&gc_pid));

        // The leader is a *direct child* of this test process, so once
        // kill_pids signals it the leader becomes a zombie until we reap it —
        // and `is_pid_alive` (kill -0) reports a zombie as still alive, since
        // POSIX keeps the PID entry until the parent wait()s. Poll try_wait()
        // for the leader: it reaps the zombie the moment the process has
        // exited and reports it genuinely dead, which is exactly what happens
        // in production where the orphan reparents to systemd --user/init and
        // that init-role process auto-reaps it (the daemon is never the
        // orphan's parent). The grandchild below, by contrast, reparents to
        // init when the leader dies and is auto-reaped there, so plain
        // is_pid_alive polling is correct for it.
        assert!(
            wait_until(|| matches!(child.try_wait(), Ok(Some(_))), 3000),
            "leader should be dead after kill_pids"
        );
        assert!(
            wait_until(|| !is_pid_alive(gc_pid), 3000),
            "the setsid-escaped grandchild survived — descendant-tree reap did not reach it \
             (the exact #5110 regression: a pgid-scoped kill would have missed this pid)"
        );
    }
}
