//! Confirmed-live sweep-claim probe (Issue #4556).
//!
//! ## Problem
//!
//! Every pre-#4556 dedup signal for "is issue #N already being worked?" is
//! either *scoped to one daemon process* or *only proves a file exists*:
//!
//! | Signal | Weakness |
//! |---|---|
//! | [`crate::types::SweepInfo`] entries (`in_flight()`) | in-memory; wiped by a daemon restart, invisible to a second daemon instance |
//! | `loom:building` label | reverted by [`crate::claim_reconciliation`] the moment a recorded PID *looks* dead |
//! | `.loom/locks/issue-<N>/` ([`crate::sweep_registry::SweepRegistry`]'s `acquire_lock`) | existence-only; every reaper / cancel / watchdog path *releases* it on a dead-PID verdict, and the release is what re-opens the door |
//!
//! Issue #4275 was dispatched **seven times in 77 minutes** on one host
//! because those weaknesses compose: the reconciler saw a dead recorded PID and
//! reverted `loom:building` -> `loom:issue`; the mid-build and review-stall
//! watchdogs each released the lock and re-dispatched; and three further
//! dispatches came from a *second* `loom-daemon` instance on the same host
//! (a debug build run out of a worktree) that shared neither the first
//! daemon's memory nor its `.loom/locks/`. At the peak four sweep processes for
//! one issue were alive at once, all writing into one worktree.
//!
//! ## What this module adds
//!
//! A single **read-only** probe answering the strictly stronger question: *is
//! there a sweep process for issue #N that is confirmed running right now?*
//! It never mutates the filesystem, so it can gate destructive work (a lock
//! release, a label revert, a re-dispatch) without freeing anything itself.
//!
//! Three independent evidence legs, cheapest-first, short-circuiting on the
//! first hit:
//!
//! 1. **Live claim lock** — `.loom/locks/issue-<N>/owner.json` whose
//!    `owner_pid` is a live, non-zombie process. Unlike `acquire_lock`'s
//!    `AlreadyExists` check this distinguishes a *live* holder from an
//!    abandoned lock dir, so it is safe to consult *before* a release.
//! 2. **Machine-level sweep journal** — `~/.loom/sweeps.json`
//!    ([`crate::sweep_journal`]) entry whose `pid` is live. This file is
//!    machine-level, so it survives a daemon restart **and** is shared by every
//!    `loom-daemon` instance on the host. Repo matching is deliberately
//!    *related-path* rather than exact (see [`repos_are_related`]) so a daemon
//!    running out of `.loom/worktrees/issue-N` and one running out of the
//!    parent checkout see each other's claims — they are the same repo, so
//!    issue #N means the same GitHub issue to both.
//! 3. **Live sweep process scan** — a process whose cwd is inside the
//!    workspace root and whose argv contains `/loom:sweep <N>`
//!    ([`cmdline_targets_sweep_issue`]). This is the last-resort leg that
//!    catches a sweep no local bookkeeping knows about at all: the exact
//!    signature of the three unattributed #4275 dispatches, where `ps` showed
//!    `claude -p /loom:sweep 4275 --claim-owned 4275` processes with no
//!    corresponding entry in the production daemon's log.
//!
//! ## Fail-open by design
//!
//! Every leg resolves to "no evidence" on any ambiguity — missing/corrupt
//! `owner.json`, unreadable journal, no `/proc`, an unreadable `cwd` symlink.
//! A garbage file must never wedge an issue permanently; the guards built on
//! this probe only ever refuse on **positively-confirmed** liveness. That is
//! the same fail-open contract [`crate::sweep_registry`]'s `lock_owned_by_other`
//! (#4463) already follows.
//!
//! Zombies are explicitly **not** live ([`pid_is_live_process`]): a terminated
//! -but-unreaped child keeps its PID allocated, so a bare `kill(pid, 0)` probe
//! reports it alive forever — precisely the false-positive that would turn this
//! guard into a permanent wedge.

use std::path::Path;
// Only leg 3's `/proc` scan owns a `PathBuf` (the canonicalized workspace root);
// gating the import keeps a non-Linux build warning-free under `-D warnings`.
#[cfg(target_os = "linux")]
use std::path::PathBuf;

use crate::sweep_journal::{self, SweepJournal};

/// Which signal proved a live claim. Carried into the refusal log line / typed
/// error so an operator can tell a live-lock refusal from a cross-instance
/// process-scan refusal without re-deriving the evidence by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveClaimEvidence {
    /// `.loom/locks/issue-<N>/owner.json` records a live owner PID.
    ClaimLock { pid: u32, sweep_id: String },
    /// The machine-level sweep journal records a live PID for this issue.
    Journal { pid: u32, repo: String },
    /// A live process rooted in this workspace is running `/loom:sweep <N>`.
    SweepProcess { pid: u32 },
}

impl LiveClaimEvidence {
    /// The confirmed-live PID behind this evidence.
    #[must_use]
    pub fn pid(&self) -> u32 {
        match self {
            Self::ClaimLock { pid, .. }
            | Self::Journal { pid, .. }
            | Self::SweepProcess { pid } => *pid,
        }
    }
}

impl std::fmt::Display for LiveClaimEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClaimLock { pid, sweep_id } => {
                write!(f, "a live claim lock owned by sweep {sweep_id} (pid {pid})")
            }
            Self::Journal { pid, repo } => {
                write!(f, "a live sweep-journal record for {repo} (pid {pid})")
            }
            Self::SweepProcess { pid } => {
                write!(f, "a live `/loom:sweep` process (pid {pid})")
            }
        }
    }
}

/// Minimal read-side view of a claim lock's `owner.json`.
///
/// Deliberately its own type rather than a re-export of `sweep_registry`'s
/// private `LockOwner`: this module only needs two fields, and keeping the
/// read-side shape local means a future writer-side field addition cannot
/// break the probe (unknown fields are ignored by serde).
#[derive(Debug, serde::Deserialize)]
struct LockOwnerView {
    owner_pid: u32,
    #[serde(default)]
    sweep_id: String,
}

/// Whether `pid` is a **live, non-zombie** process.
///
/// `kill(pid, 0)` alone is insufficient: a terminated-but-unreaped child is a
/// zombie whose PID is still allocated, so the bare probe reports it alive
/// indefinitely (the same trap documented on `SweepRegistry::children`). A
/// zombie holds no locks and writes no files, so treating it as a live claim
/// would wedge an issue forever — exactly the failure mode this guard must not
/// introduce.
#[must_use]
pub fn pid_is_live_process(pid: u32) -> bool {
    crate::sweep_registry::is_pid_alive(pid) && !pid_is_zombie(pid)
}

/// Linux: read `/proc/<pid>/stat` and report whether the process state is `Z`.
/// Any read/parse failure resolves to `false` (not-a-zombie) so an unreadable
/// `stat` never *adds* liveness evidence it cannot prove.
#[cfg(target_os = "linux")]
fn pid_is_zombie(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // Format: `pid (comm) state ...`. `comm` may contain spaces AND parens, so
    // the only safe split point is the LAST ')'.
    let Some(after_comm) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
        return false;
    };
    after_comm.split_whitespace().next() == Some("Z")
}

#[cfg(not(target_os = "linux"))]
fn pid_is_zombie(_pid: u32) -> bool {
    false
}

/// Whether `pid` is a live process whose argv targets `/loom:sweep <issue>`.
///
/// The **positive-confirmation** primitive behind
/// [`crate::sweep_registry::LockReleaseOutcome::HolderAlive`]: refusing a lock
/// release on bare PID liveness would also refuse when an unrelated process has
/// recycled the PID, wedging the issue permanently. Requiring the argv to name
/// this exact issue's sweep makes a false refusal essentially impossible.
///
/// Linux reads `/proc/<pid>/cmdline`. On other platforms there is no cheap
/// portable equivalent, so this returns `false` — "cannot confirm" — and every
/// caller falls open to its pre-#4556 behavior rather than refusing on a guess.
#[must_use]
pub fn pid_is_sweep_process_for(pid: u32, issue: u32) -> bool {
    if !pid_is_live_process(pid) {
        return false;
    }
    read_pid_cmdline(pid).is_some_and(|c| cmdline_targets_sweep_issue(&c, issue))
}

#[cfg(target_os = "linux")]
fn read_pid_cmdline(pid: u32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(String::from_utf8_lossy(&raw).replace('\0', " "))
}

#[cfg(not(target_os = "linux"))]
fn read_pid_cmdline(_pid: u32) -> Option<String> {
    None
}

/// The liveness predicate the *evidence legs* use: `pid` is live **and**, when
/// this platform can read its argv, that argv targets `/loom:sweep <issue>`.
///
/// Both bookkeeping legs record the PID of the process the daemon spawned —
/// `spawn-worker.sh -p "/loom:sweep <N> --claim-owned <N>" …`, which `exec`s
/// through to `claude` **without changing PID**, so the needle is present for
/// the whole life of a real sweep (see `SweepRegistry::spawn_child`). Requiring
/// it therefore costs nothing for a genuine claim while removing the only way
/// this guard could wedge an issue permanently: a *stale* lock or journal
/// record whose PID has since been recycled by an unrelated live process. Both
/// records are removed on reap/cancel and pruned on the next journal write, but
/// a daemon that dies between a sweep's exit and its reap leaves exactly that
/// stale record behind, and an unverified PID probe would then refuse every
/// future dispatch of the issue forever.
///
/// Unverifiable argv (non-Linux, or a `/proc` hardened against reading another
/// process's `cmdline`) falls back to bare liveness — the same signal every
/// pre-#4556 liveness check already used, so the guard is never *weaker* than
/// the code it supplements on those hosts.
#[must_use]
pub fn pid_is_live_claim_for(pid: u32, issue: u32) -> bool {
    if !pid_is_live_process(pid) {
        return false;
    }
    match read_pid_cmdline(pid) {
        Some(cmdline) => cmdline_targets_sweep_issue(&cmdline, issue),
        // Cannot read the argv on this platform: fall back to bare liveness.
        None => true,
    }
}

/// Leg 1: the per-issue claim lock's owner, when its PID is confirmed live.
///
/// `is_live` is injected so tests can drive both verdicts without spawning
/// processes. Fail-open: a missing lock dir, unreadable or unparseable
/// `owner.json`, or a dead/zombie owner all resolve to `None`.
#[must_use]
pub fn live_lock_owner_in(
    locks_dir: &Path,
    issue: u32,
    is_live: &dyn Fn(u32) -> bool,
) -> Option<LiveClaimEvidence> {
    let owner_path = locks_dir.join(format!("issue-{issue}")).join("owner.json");
    let raw = std::fs::read_to_string(&owner_path).ok()?;
    let owner: LockOwnerView = serde_json::from_str(&raw).ok()?;
    if !is_live(owner.owner_pid) {
        return None;
    }
    Some(LiveClaimEvidence::ClaimLock {
        pid: owner.owner_pid,
        sweep_id: owner.sweep_id,
    })
}

/// Whether two workspace-root strings name the **same repository** for the
/// purpose of issue-number identity.
///
/// Exact equality is not sufficient. The three unattributed #4275 dispatches
/// came from a `loom-daemon` whose `workspace_root` was
/// `<repo>/.loom/worktrees/issue-4385` — a git worktree of the very same
/// checkout. Two roots in an ancestor/descendant relationship therefore refer
/// to one GitHub repo, and "issue #N" means the same issue to both, so each
/// must be able to see the other's journal claim.
///
/// Path-component-wise (never a raw string prefix), so `/repo/loom` and
/// `/repo/loom-2` are correctly *unrelated*.
#[must_use]
pub fn repos_are_related(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let (pa, pb) = (Path::new(a), Path::new(b));
    pa.starts_with(pb) || pb.starts_with(pa)
}

/// Leg 2: a machine-level sweep-journal record for this issue whose PID is
/// confirmed live, from this workspace root or any related one
/// ([`repos_are_related`]).
#[must_use]
pub fn live_journal_claim(
    journal: &SweepJournal,
    workspace_root: &Path,
    issue: u32,
    is_live: &dyn Fn(u32) -> bool,
) -> Option<LiveClaimEvidence> {
    let root = workspace_root.display().to_string();
    journal
        .entries
        .iter()
        .find(|e| e.issue == issue && repos_are_related(&e.repo, &root) && is_live(e.pid))
        .map(|e| LiveClaimEvidence::Journal {
            pid: e.pid,
            repo: e.repo.clone(),
        })
}

/// Whether a process command line is running `/loom:sweep <issue>`.
///
/// `cmdline` is the process argv joined by single spaces (the daemon spawns
/// `claude -p "/loom:sweep <N> --claim-owned <N>"`, so the issue number lives
/// *inside* one argv element — see `SweepRegistry::spawn_child`).
///
/// Deliberately strict, to keep the process-scan leg from ever refusing a
/// legitimate dispatch:
///
/// - The token immediately after `/loom:sweep` must parse as exactly `issue`,
///   so `/loom:sweep 42751` never matches issue 4275.
/// - `/loom:sweep` must be followed by whitespace, so `/loom:sweeper` and a
///   bare `/loom:sweep` with no argument never match.
/// - PR-set mode (`/loom:sweep --prs 4275`) does **not** match: it claims no
///   issue, so it is not a competing issue sweep.
#[must_use]
pub fn cmdline_targets_sweep_issue(cmdline: &str, issue: u32) -> bool {
    const NEEDLE: &str = "/loom:sweep";
    let mut rest = cmdline;
    while let Some(idx) = rest.find(NEEDLE) {
        let after = &rest[idx + NEEDLE.len()..];
        if after.starts_with(char::is_whitespace)
            && after
                .split_whitespace()
                .next()
                .and_then(|t| t.parse::<u32>().ok())
                == Some(issue)
        {
            return true;
        }
        rest = after;
    }
    false
}

/// Whether `path` is `root` itself or lives beneath it (component-wise).
///
/// Only leg 3's `/proc` scan needs this, so it is gated to match its single
/// caller — otherwise a non-Linux build trips `dead_code` under `-D warnings`.
#[cfg(target_os = "linux")]
fn path_within(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

/// Leg 3 (Linux): scan `/proc` for a live process whose cwd is inside
/// `workspace_root` and whose argv targets `/loom:sweep <issue>`.
///
/// Ordered cheap-first: the `cwd` readlink is one syscall and prunes to the
/// handful of processes rooted in this workspace before any `cmdline` read
/// happens. Scoping on cwd is what makes this leg **repo-scoped** (the daemon
/// spawns every sweep child with `current_dir(workspace_root)`), so a sweep for
/// another checkout's issue #N can never refuse a dispatch here.
///
/// Zombies are skipped for free: a zombie's `cwd` symlink is no longer
/// readable, so it never reaches the `cmdline` check.
#[cfg(target_os = "linux")]
#[must_use]
pub fn live_sweep_process_in(workspace_root: &Path, issue: u32) -> Option<LiveClaimEvidence> {
    let root: PathBuf = workspace_root
        .canonicalize()
        .unwrap_or_else(|_| workspace_root.to_path_buf());
    let self_pid = std::process::id();
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let Ok(cwd) = std::fs::read_link(entry.path().join("cwd")) else {
            continue;
        };
        if !path_within(&cwd, &root) {
            continue;
        }
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let cmdline = String::from_utf8_lossy(&raw).replace('\0', " ");
        if cmdline_targets_sweep_issue(&cmdline, issue) {
            return Some(LiveClaimEvidence::SweepProcess { pid });
        }
    }
    None
}

/// Leg 3 on non-Linux hosts: not implemented.
///
/// The `/proc` scan has no portable equivalent that is both cheap and
/// cwd-scoped (`lsof +d` is neither). Legs 1 and 2 — the live claim lock and
/// the machine-level journal — are fully cross-platform, so the guard still
/// holds for every sweep either daemon bookkeeping knows about; only the
/// "completely untracked ghost sweep" case degrades to no-evidence here.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn live_sweep_process_in(_workspace_root: &Path, _issue: u32) -> Option<LiveClaimEvidence> {
    None
}

/// The full production probe: is there a **confirmed-live** sweep claim on
/// `issue` for the repository rooted at `workspace_root`?
///
/// `journal_path` overrides the machine-level journal location (tests and
/// `SweepRegistryConfig::journal_path` both use this); `None` resolves
/// [`sweep_journal::default_journal_path`].
///
/// Read-only and short-circuiting: leg 1 (a `read_to_string`) and leg 2 (one
/// small JSON file) run before the `/proc` scan, so the common
/// nothing-is-claimed case costs two file reads plus a bounded directory walk.
#[must_use]
pub fn probe(
    workspace_root: &Path,
    journal_path: Option<&Path>,
    issue: u32,
) -> Option<LiveClaimEvidence> {
    // Both bookkeeping legs verify the recorded PID's argv where the platform
    // allows it ([`pid_is_live_claim_for`]), so a *stale* record whose PID has
    // been recycled by an unrelated process can never wedge the issue.
    let is_live: &dyn Fn(u32) -> bool = &|pid| pid_is_live_claim_for(pid, issue);

    let locks_dir = workspace_root.join(".loom").join("locks");
    if let Some(evidence) = live_lock_owner_in(&locks_dir, issue, is_live) {
        return Some(evidence);
    }

    let journal_path = journal_path
        .map(Path::to_path_buf)
        .or_else(|| sweep_journal::default_journal_path().ok());
    if let Some(path) = journal_path {
        let journal = sweep_journal::load(&path);
        if let Some(evidence) = live_journal_claim(&journal, workspace_root, issue, is_live) {
            return Some(evidence);
        }
    }

    live_sweep_process_in(workspace_root, issue)
}

/// Block until a freshly-spawned stand-in sweep child's argv actually names the
/// sweep (test support, shared with [`crate::sweep_registry`]'s tests).
///
/// `Command::spawn` returns as soon as the **fork** succeeds; until the child
/// reaches `exec`, `/proc/<pid>/cmdline` still shows the *parent's* argv (the
/// test binary). Without this wait an argv-verifying probe can read the
/// pre-exec argv and correctly decline, which under a loaded full-suite run
/// turns every such assertion into a flake — observed as three intermittent
/// failures that each passed when run alone. Bounded, and a timeout simply
/// proceeds so the assertion under test reports the real failure rather than
/// hanging.
#[cfg(test)]
pub(crate) fn wait_until_argv_visible(pid: u32, issue: u32) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        match read_pid_cmdline(pid) {
            // Not readable at all (non-Linux): nothing to wait for — the bare
            // liveness fallback in `pid_is_live_claim_for` applies there.
            None => return,
            Some(c) if cmdline_targets_sweep_issue(&c, issue) => return,
            Some(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::sweep_journal::JournalEntry;
    use chrono::Utc;
    use tempfile::tempdir;

    fn write_lock(locks_dir: &Path, issue: u32, pid: u32, sweep_id: &str) {
        let dir = locks_dir.join(format!("issue-{issue}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("owner.json"),
            format!(
                r#"{{"issue": {issue}, "owner_pid": {pid}, "acquired_at": "2026-07-30T00:00:00Z", "sweep_id": "{sweep_id}"}}"#
            ),
        )
        .unwrap();
    }

    /// A stand-in for a real sweep child: a long-lived process whose argv
    /// contains `/loom:sweep <issue>`, killed on drop.
    ///
    /// `sh -c <script> <argv0>` puts the third argument in `$0`, so the needle
    /// lands in the process's argv without being interpreted — the same shape
    /// `SweepRegistry::spawn_child` produces (`spawn-worker.sh -p "/loom:sweep
    /// <N> --claim-owned <N>"`, which `exec`s through to `claude` keeping both
    /// the PID and the needle).
    struct FakeSweep(std::process::Child);

    impl FakeSweep {
        fn spawn(issue: u32) -> Self {
            let child = std::process::Command::new("sh")
                .arg("-c")
                .arg("sleep 120")
                .arg(format!("/loom:sweep {issue} --claim-owned {issue}"))
                .spawn()
                .unwrap();
            wait_until_argv_visible(child.id(), issue);
            Self(child)
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }
    }

    impl Drop for FakeSweep {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn journal_with(repo: &str, issue: u32, pid: u32) -> SweepJournal {
        SweepJournal {
            version: 1,
            entries: vec![JournalEntry {
                repo: repo.to_string(),
                issue,
                pid,
                started_at: Utc::now(),
            }],
        }
    }

    // ---- leg 1: live claim lock ------------------------------------------

    #[test]
    fn live_lock_owner_reports_a_live_pid() {
        let dir = tempdir().unwrap();
        write_lock(dir.path(), 4275, 4242, "sweep-issue-4275-1");
        let evidence = live_lock_owner_in(dir.path(), 4275, &|_| true).unwrap();
        assert_eq!(
            evidence,
            LiveClaimEvidence::ClaimLock {
                pid: 4242,
                sweep_id: "sweep-issue-4275-1".to_string()
            }
        );
        assert_eq!(evidence.pid(), 4242);
    }

    #[test]
    fn live_lock_owner_ignores_a_dead_pid() {
        // The whole point of #4556: a lock file that EXISTS is not a live
        // claim. `acquire_lock` refuses on existence; this probe must not.
        let dir = tempdir().unwrap();
        write_lock(dir.path(), 4275, 4242, "sweep-issue-4275-1");
        assert!(live_lock_owner_in(dir.path(), 4275, &|_| false).is_none());
    }

    #[test]
    fn live_lock_owner_fails_open_on_missing_and_corrupt_owner() {
        let dir = tempdir().unwrap();
        // Missing entirely.
        assert!(live_lock_owner_in(dir.path(), 4275, &|_| true).is_none());
        // Present but unparseable.
        let lock = dir.path().join("issue-4275");
        std::fs::create_dir_all(&lock).unwrap();
        std::fs::write(lock.join("owner.json"), "not json").unwrap();
        assert!(live_lock_owner_in(dir.path(), 4275, &|_| true).is_none());
    }

    #[test]
    fn live_lock_owner_is_scoped_to_the_requested_issue() {
        let dir = tempdir().unwrap();
        write_lock(dir.path(), 4275, 4242, "sweep-issue-4275-1");
        assert!(live_lock_owner_in(dir.path(), 4276, &|_| true).is_none());
    }

    // ---- leg 2: machine-level journal ------------------------------------

    #[test]
    fn live_journal_claim_matches_the_same_root() {
        let journal = journal_with("/repo/loom", 4275, 999);
        let evidence =
            live_journal_claim(&journal, Path::new("/repo/loom"), 4275, &|_| true).unwrap();
        assert_eq!(evidence.pid(), 999);
    }

    #[test]
    fn live_journal_claim_ignores_a_dead_pid() {
        let journal = journal_with("/repo/loom", 4275, 999);
        assert!(live_journal_claim(&journal, Path::new("/repo/loom"), 4275, &|_| false).is_none());
    }

    #[test]
    fn live_journal_claim_sees_a_nested_worktree_daemons_claim() {
        // The three unattributed #4275 dispatches came from a daemon rooted at
        // <repo>/.loom/worktrees/issue-4385. Its journal claim must be visible
        // to the parent checkout's daemon, and vice versa.
        let journal = journal_with("/repo/loom/.loom/worktrees/issue-4385", 4275, 999);
        assert!(live_journal_claim(&journal, Path::new("/repo/loom"), 4275, &|_| true).is_some());

        let parent = journal_with("/repo/loom", 4275, 999);
        assert!(live_journal_claim(
            &parent,
            Path::new("/repo/loom/.loom/worktrees/issue-4385"),
            4275,
            &|_| true
        )
        .is_some());
    }

    #[test]
    fn live_journal_claim_ignores_an_unrelated_repo() {
        let journal = journal_with("/repo/other", 4275, 999);
        assert!(live_journal_claim(&journal, Path::new("/repo/loom"), 4275, &|_| true).is_none());
    }

    #[test]
    fn repos_are_related_is_component_wise_not_string_prefix() {
        assert!(repos_are_related("/repo/loom", "/repo/loom"));
        assert!(repos_are_related("/repo/loom", "/repo/loom/.loom/worktrees/issue-1"));
        assert!(repos_are_related("/repo/loom/.loom/worktrees/issue-1", "/repo/loom"));
        // A sibling whose name merely starts with the same characters is NOT
        // the same repo.
        assert!(!repos_are_related("/repo/loom", "/repo/loom-2"));
        assert!(!repos_are_related("/repo/loom", "/other/loom"));
    }

    // ---- leg 3: sweep-process cmdline matcher ----------------------------

    #[test]
    fn cmdline_matches_the_real_daemon_spawn_shape() {
        // Exactly what `ps` showed during the #4275 incident.
        let cmdline =
            "claude -p /loom:sweep 4275 --claim-owned 4275 --dangerously-skip-permissions";
        assert!(cmdline_targets_sweep_issue(cmdline, 4275));
        assert!(!cmdline_targets_sweep_issue(cmdline, 4276));
    }

    #[test]
    fn cmdline_requires_an_exact_issue_token() {
        // Prefix/suffix digits must never match — the classic substring bug.
        assert!(!cmdline_targets_sweep_issue("claude -p /loom:sweep 42751", 4275));
        assert!(!cmdline_targets_sweep_issue("claude -p /loom:sweep 14275", 4275));
    }

    #[test]
    fn cmdline_ignores_sweep_without_an_issue_argument() {
        assert!(!cmdline_targets_sweep_issue("claude -p /loom:sweep", 4275));
        assert!(!cmdline_targets_sweep_issue("claude -p /loom:sweeper 4275", 4275));
        // PR-set mode claims no issue.
        assert!(!cmdline_targets_sweep_issue("claude -p /loom:sweep --prs 4275", 4275));
    }

    #[test]
    fn cmdline_scans_past_a_non_matching_occurrence() {
        let cmdline = "sh -c echo /loom:sweep --prs 1 ; claude -p /loom:sweep 4275";
        assert!(cmdline_targets_sweep_issue(cmdline, 4275));
    }

    #[test]
    fn cmdline_matcher_terminates_on_repeated_needles() {
        // Regression guard for the scan loop's advance step: a cmdline made of
        // nothing but non-matching needles must terminate, not spin.
        let cmdline = "/loom:sweep /loom:sweep /loom:sweep";
        assert!(!cmdline_targets_sweep_issue(cmdline, 4275));
    }

    // ---- composed probe --------------------------------------------------

    #[test]
    fn probe_finds_nothing_in_a_pristine_workspace() {
        let dir = tempdir().unwrap();
        let journal = dir.path().join("sweeps.json");
        assert!(probe(dir.path(), Some(&journal), 4275).is_none());
    }

    #[test]
    fn probe_reports_the_live_lock_leg_first() {
        let dir = tempdir().unwrap();
        let locks = dir.path().join(".loom").join("locks");
        let sweep = FakeSweep::spawn(4275);
        write_lock(&locks, 4275, sweep.pid(), "sweep-issue-4275-live");
        let journal = dir.path().join("sweeps.json");
        let evidence = probe(dir.path(), Some(&journal), 4275).unwrap();
        assert!(matches!(evidence, LiveClaimEvidence::ClaimLock { .. }));
    }

    #[test]
    fn probe_falls_through_a_dead_lock_to_the_journal_leg() {
        let dir = tempdir().unwrap();
        let locks = dir.path().join(".loom").join("locks");
        // PID 0 is treated as dead by `is_pid_alive`, so leg 1 declines.
        write_lock(&locks, 4275, 0, "sweep-issue-4275-dead");
        let journal_path = dir.path().join("sweeps.json");
        let sweep = FakeSweep::spawn(4275);
        let journal = journal_with(&dir.path().display().to_string(), 4275, sweep.pid());
        std::fs::write(&journal_path, serde_json::to_string(&journal).unwrap()).unwrap();
        let evidence = probe(dir.path(), Some(&journal_path), 4275).unwrap();
        assert!(matches!(evidence, LiveClaimEvidence::Journal { .. }));
    }

    /// A *stale* record whose PID has been recycled by an unrelated live
    /// process must NOT count as a live claim: that is the only way this guard
    /// could wedge an issue permanently (the record is never pruned, because
    /// `prune_dead` also sees the recycled PID as alive).
    #[cfg(target_os = "linux")]
    #[test]
    fn probe_ignores_a_live_pid_whose_argv_is_not_this_issues_sweep() {
        let dir = tempdir().unwrap();
        let locks = dir.path().join(".loom").join("locks");
        // Our own PID: live, but its argv is the test binary, not a sweep.
        write_lock(&locks, 4275, std::process::id(), "sweep-issue-4275-recycled");
        let journal_path = dir.path().join("sweeps.json");
        let journal = journal_with(&dir.path().display().to_string(), 4275, std::process::id());
        std::fs::write(&journal_path, serde_json::to_string(&journal).unwrap()).unwrap();
        assert!(probe(dir.path(), Some(&journal_path), 4275).is_none());
    }

    /// A sweep process for a *different* issue must not satisfy the argv check.
    #[cfg(target_os = "linux")]
    #[test]
    fn pid_is_live_claim_for_is_scoped_to_the_issue() {
        let sweep = FakeSweep::spawn(4275);
        assert!(pid_is_live_claim_for(sweep.pid(), 4275));
        assert!(!pid_is_live_claim_for(sweep.pid(), 4276));
        assert!(!pid_is_live_claim_for(0, 4275), "a dead pid is never a claim");
    }

    #[test]
    fn probe_ignores_a_dead_lock_and_a_dead_journal_entry() {
        let dir = tempdir().unwrap();
        let locks = dir.path().join(".loom").join("locks");
        write_lock(&locks, 4275, 0, "sweep-issue-4275-dead");
        let journal_path = dir.path().join("sweeps.json");
        let journal = journal_with(&dir.path().display().to_string(), 4275, 0);
        std::fs::write(&journal_path, serde_json::to_string(&journal).unwrap()).unwrap();
        // Leg 3 cannot fire: no live process has its cwd inside a fresh tempdir.
        assert!(probe(dir.path(), Some(&journal_path), 4275).is_none());
    }

    #[test]
    fn our_own_pid_is_a_live_process_and_pid_zero_is_not() {
        assert!(pid_is_live_process(std::process::id()));
        assert!(!pid_is_live_process(0));
    }

    #[test]
    fn evidence_display_names_the_leg() {
        assert!(LiveClaimEvidence::ClaimLock {
            pid: 1,
            sweep_id: "s".into()
        }
        .to_string()
        .contains("claim lock"));
        assert!(LiveClaimEvidence::Journal {
            pid: 1,
            repo: "/r".into()
        }
        .to_string()
        .contains("sweep-journal"));
        assert!(LiveClaimEvidence::SweepProcess { pid: 1 }
            .to_string()
            .contains("/loom:sweep"));
    }
}
