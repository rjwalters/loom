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
//! 2. Skip it unless EVERY gate in [`OrphanSkip`] clears: the worktree carries
//!    the `.loom-managed` sentinel, issue N has no live sweep claim, issue N
//!    is closed, and its PR is not open (see "Fail-safe" on
//!    [`reap_orphan_processes_with`]).
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

use chrono::{DateTime, Utc};

use crate::worktree_ops::clean::{
    check_grace_period, is_loom_managed, PrStatus, DEFAULT_GRACE_PERIOD_SECS,
};
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

/// Why a fail-safe gate preserved a worktree's live processes.
///
/// Every variant is a *refusal to kill*: this pass reports "I found running
/// processes and deliberately left them alone", which is the only way an
/// operator can tell a quiet tick apart from a tick that nearly killed a
/// reviewer's build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrphanSkip {
    /// No `.loom-managed` sentinel — user-provisioned, never touched.
    Unmanaged,
    /// A daemon-dispatched sweep holds a claim-lock / spawn-loop entry.
    LiveSweepClaim,
    /// The issue is not `CLOSED` (payload: the observed state, e.g. `OPEN` /
    /// `UNKNOWN`) — work on it may legitimately still be in flight.
    IssueNotClosed(String),
    /// The issue's PR is still open — it is still under review/revision.
    PrOpen,
    /// The forge PR probe failed; "unknown" is never license to kill.
    PrStatusUnknown,
    /// The PR merged, but the post-merge grace period has not elapsed
    /// (payload: seconds remaining) — the merging agent may still be
    /// finishing up inside the worktree.
    MergeGrace(i64),
}

impl OrphanSkip {
    /// Human-readable reason, for the daemon log and the report.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::Unmanaged => "no .loom-managed sentinel (user-provisioned)".to_string(),
            Self::LiveSweepClaim => "live spawn-loop task or claim-lock".to_string(),
            Self::IssueNotClosed(state) => format!("issue is {state}"),
            Self::PrOpen => "PR still open".to_string(),
            Self::PrStatusUnknown => "PR status unknown".to_string(),
            Self::MergeGrace(remaining) => {
                format!("grace period not passed ({remaining}s remaining)")
            }
        }
    }
}

/// What one orphan-process reap pass over one repo did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrphanReapReport {
    /// `issue-<N>` worktree directories examined.
    pub scanned: usize,
    /// Worktrees that had at least one orphan process reaped.
    pub reaped: Vec<OrphanReapEntry>,
    /// Worktrees that had live processes a fail-safe gate preserved.
    pub preserved: Vec<(u32, String)>,
}

impl OrphanReapReport {
    /// A compact one-line summary for the daemon log.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "scanned={} reaped_worktrees={} preserved={}",
            self.scanned,
            self.reaped.len(),
            self.preserved.len()
        )
    }
}

/// Injected probes for [`reap_orphan_processes_with`], so every fail-safe gate
/// is unit-testable without a live process table or forge — the same
/// dependency-injection shape [`crate::worktree_ops::clean::WorktreeProbes`]
/// uses for the sibling worktree-**removal** pass.
pub struct OrphanReapProbes<'a> {
    /// Issues with a live spawn-loop task or claim-lock (daemon-dispatched
    /// sweeps only — which is exactly why it cannot be the sole gate).
    pub active_issues: &'a HashSet<u32>,
    /// PIDs whose cwd is inside the worktree.
    pub processes_using: &'a dyn Fn(&Path) -> Vec<u32>,
    /// Forge issue state (`"OPEN"` / `"CLOSED"` / `"UNKNOWN"`).
    pub issue_state: &'a dyn Fn(u32) -> String,
    /// Forge PR status for the issue's branch.
    pub pr_status: &'a dyn Fn(u32) -> PrStatus,
    /// Full descendant-PID walk for a root pid.
    pub collect_descendants: &'a dyn Fn(u32) -> Vec<u32>,
    /// Signal delivery; returns the pids actually signaled.
    pub kill: &'a dyn Fn(&[u32]) -> Vec<u32>,
    /// How long after a PR merge a worktree's processes become reapable.
    pub grace_period_secs: i64,
    /// Wall clock the grace-period gate measures against.
    pub now: DateTime<Utc>,
}

/// Decide whether issue `issue`'s worktree is provably dead enough that
/// processes still running inside it are orphans. `None` ⇒ reap; `Some(skip)`
/// ⇒ leave every process alone.
///
/// Ordered cheapest-first: the two local (filesystem) gates run before either
/// forge probe, and callers only reach this at all once a live process was
/// actually found, so an idle worktree costs zero REST calls per tick.
#[must_use]
pub fn classify_orphan_candidate(
    worktree_path: &Path,
    issue: u32,
    probes: &OrphanReapProbes<'_>,
) -> Option<OrphanSkip> {
    // Gate 1: never touch a user-provisioned worktree.
    if !is_loom_managed(worktree_path) {
        return Some(OrphanSkip::Unmanaged);
    }
    // Gate 2: never touch a worktree a daemon-dispatched sweep still claims.
    if probes.active_issues.contains(&issue) {
        return Some(OrphanSkip::LiveSweepClaim);
    }
    // Gate 3: never touch a worktree whose issue is still open (or whose
    // state could not be determined) — Manual Orchestration Mode and manual
    // `/loom:sweep` work never registers a claim in gate 2, so this is the
    // gate that actually protects them.
    let issue_state = (probes.issue_state)(issue);
    if issue_state != "CLOSED" {
        return Some(OrphanSkip::IssueNotClosed(issue_state));
    }
    // Gate 4: never touch a worktree whose PR is still open / unknown, and
    // give a just-merged PR the same post-merge grace the removal pass gives
    // it — a Judge or Doctor reviewing that PR reuses this very worktree.
    match (probes.pr_status)(issue) {
        PrStatus::Open => Some(OrphanSkip::PrOpen),
        PrStatus::Unknown => Some(OrphanSkip::PrStatusUnknown),
        PrStatus::Merged { merged_at } => {
            let Ok(dt) = DateTime::parse_from_rfc3339(&merged_at) else {
                // An unparseable timestamp is an unknown merge time, not an
                // expired grace period.
                return Some(OrphanSkip::PrStatusUnknown);
            };
            let (passed, remaining) =
                check_grace_period(dt.with_timezone(&Utc), probes.grace_period_secs, probes.now);
            (!passed).then_some(OrphanSkip::MergeGrace(remaining))
        }
        // Closed issue whose PR merged long ago, closed without merging, or
        // that never had a PR: nothing is legitimately working here.
        PrStatus::ClosedNoMerge | PrStatus::NoPr => None,
    }
}

/// Scan `repo_root`'s worktree root for `issue-<N>` worktrees whose sweep is
/// provably dead but that still have a process running inside them, and
/// terminate the whole descendant tree of every such process.
///
/// The probes are injected ([`OrphanReapProbes`]) so the whole pass is
/// unit-testable without a live process table or forge — production wiring is
/// [`reap_orphan_processes`].
///
/// # Fail-safe (this issue's AC)
///
/// A worktree is only ever a reap candidate when EVERY gate in
/// [`classify_orphan_candidate`] clears — the same gate set the sibling
/// worktree-**removal** pass
/// ([`crate::worktree_ops::clean::classify_worktree`]) applies, minus the ones
/// whose polarity is inverted here:
/// - the `.loom-managed` sentinel (`CleanOptions::require_managed_sentinel`);
/// - no live sweep claim (`SkipInUse` via
///   [`crate::worktree_ops::liveness::active_spawn_loop_issues`]);
/// - the issue is `CLOSED` (`SkipIssueNotClosed`);
/// - the PR is not open, not unknown, and past its post-merge grace period
///   (`SkipPrOpen` / `SkipUnknownPrStatus` / `SkipGrace`).
///
/// The last two matter *more* here than they do for removal. `active_issues`
/// is populated only by `SweepRegistry::dispatch`, i.e. by **daemon-dispatched
/// (Tier 2) sweeps** — Manual Orchestration Mode (`/loom:builder`,
/// `/loom:judge`, `/loom:doctor`) and manual `/loom:sweep` runs never take out
/// a claim, so for them `active_issues` is empty even while they are very much
/// alive. Since the removal pass treats "a process is using this directory" as
/// *protection* while this pass treats it as the *trigger*, the claim check
/// alone would make a Judge's `cargo test` inside an open PR's worktree
/// indistinguishable from the 5h52m runaway driver this module exists to kill.
/// The forge gates are what tell those two apart: an open PR (or an open
/// issue) is independent evidence that the work is still live, and no
/// unattended pass may kill it.
///
/// Every gate is re-checked per tick, never assumed from an earlier pass's
/// snapshot: a stale verdict here has an irreversible consequence (a live
/// agent's own process killed out from under it), unlike a stale
/// worktree-removal verdict (a no-op retried on the next tick).
pub fn reap_orphan_processes_with(
    repo_root: &Path,
    probes: &OrphanReapProbes<'_>,
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

        // Nothing is running here ⇒ nothing to reap, and — deliberately — no
        // forge probe either: an idle worktree must not cost a REST call on
        // every tick. This is the only check that precedes the fail-safe
        // gates, and it can never *authorize* a kill.
        let root_pids = (probes.processes_using)(&worktree_path);
        if root_pids.is_empty() {
            continue;
        }

        // Live processes found — now (and only now) prove the sweep is dead.
        if let Some(skip) = classify_orphan_candidate(&worktree_path, issue, probes) {
            let reason = skip.reason();
            log::info!(
                "orphan_process_reaper: {} preserving issue-{issue} pids={root_pids:?}: {reason}",
                repo_root.display()
            );
            report.preserved.push((issue, reason));
            continue;
        }

        let mut all_pids: Vec<u32> = Vec::new();
        for &root in &root_pids {
            for descendant in (probes.collect_descendants)(root) {
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

        let killed = (probes.kill)(&all_pids);
        log::warn!(
            "orphan_process_reaper: {} issue-{issue} has {} process(es) but issue #{issue} is \
             closed, its PR is not open, and no live sweep claims it — reaping the whole tree: \
             roots={root_pids:?} all_pids={all_pids:?} killed={killed:?} (#5110)",
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
/// real process-table probes, the real (REST-first, same as
/// [`crate::worktree_reaper::reap_repo`]) forge probes, and the real signal
/// delivery.
#[must_use]
pub fn reap_orphan_processes(repo_root: &Path) -> OrphanReapReport {
    use crate::worktree_ops::clean;

    let active_issues = active_spawn_loop_issues(repo_root);
    let kill_fn = |pids: &[u32]| kill_pids(pids, ORPHAN_PROCESS_REAP_GRACE);

    // Resolved once per pass (one REST call), not once per worktree.
    let owner = clean::repo_owner_rest(repo_root);
    let issue_state_fn = |n: u32| crate::worktree_ops::gh::issue_state_rest(repo_root, n);
    let pr_status_fn = |n: u32| match owner.as_deref() {
        Some(owner) => clean::check_pr_merged_rest(repo_root, owner, n),
        None => clean::check_pr_merged(repo_root, n),
    };

    reap_orphan_processes_with(
        repo_root,
        &OrphanReapProbes {
            active_issues: &active_issues,
            processes_using: &find_processes_using_directory,
            issue_state: &issue_state_fn,
            pr_status: &pr_status_fn,
            collect_descendants: &collect_descendant_pids,
            kill: &kill_fn,
            grace_period_secs: DEFAULT_GRACE_PERIOD_SECS,
            now: Utc::now(),
        },
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

    /// Scripted probe values for one pass. The defaults describe the *reapable*
    /// world — a closed issue whose PR merged long ago, no daemon claim, one
    /// process still running inside the worktree — so each test only states the
    /// single fact it is about. Mirrors `worktree_reaper::tests::ProbeSpec`.
    struct Spec {
        active: HashSet<u32>,
        issue_state: String,
        pr_status: PrStatus,
        root_pids: Vec<u32>,
        descendants: fn(u32) -> Vec<u32>,
        grace_period_secs: i64,
    }

    impl Default for Spec {
        fn default() -> Self {
            Self {
                active: HashSet::new(),
                issue_state: "CLOSED".to_string(),
                pr_status: PrStatus::Merged {
                    // Well outside any sane grace period.
                    merged_at: "2020-01-01T00:00:00Z".to_string(),
                },
                root_pids: vec![999],
                descendants: |_| Vec::new(),
                grace_period_secs: DEFAULT_GRACE_PERIOD_SECS,
            }
        }
    }

    /// Run a full pass against `repo` with scripted probes, returning the
    /// report plus every batch of pids the (fake) killer was asked to signal.
    fn run_pass(repo: &Path, spec: &Spec) -> (OrphanReapReport, Vec<Vec<u32>>) {
        let killed: RefCell<Vec<Vec<u32>>> = RefCell::new(Vec::new());
        let processes_using = |_: &Path| spec.root_pids.clone();
        let issue_state = |_: u32| spec.issue_state.clone();
        let pr_status = |_: u32| spec.pr_status.clone();
        let collect_descendants = |pid: u32| (spec.descendants)(pid);
        let kill = |pids: &[u32]| {
            killed.borrow_mut().push(pids.to_vec());
            pids.to_vec()
        };

        let report = reap_orphan_processes_with(
            repo,
            &OrphanReapProbes {
                active_issues: &spec.active,
                processes_using: &processes_using,
                issue_state: &issue_state,
                pr_status: &pr_status,
                collect_descendants: &collect_descendants,
                kill: &kill,
                grace_period_secs: spec.grace_period_secs,
                now: Utc::now(),
            },
        );
        (report, killed.into_inner())
    }

    #[test]
    fn test_orphan_with_no_live_claim_is_reaped_transitively() {
        let repo = make_repo(&[(100, true)]);
        let spec = Spec {
            root_pids: vec![111],
            // 111 -> 222 -> 333 (a driver that escaped into a new pgid/sid via
            // a `timeout`-shaped middle process — the scenario this issue's AC
            // requires a fixture for).
            descendants: |pid| {
                if pid == 111 {
                    vec![222, 333]
                } else {
                    Vec::new()
                }
            },
            ..Spec::default()
        };
        let (report, _) = run_pass(repo.path(), &spec);

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
        let spec = Spec {
            active: HashSet::from([101]),
            ..Spec::default()
        };
        let (report, killed) = run_pass(repo.path(), &spec);

        assert!(report.reaped.is_empty(), "a live sweep's work must never be reaped");
        assert!(killed.is_empty());
        assert_eq!(report.preserved[0].1, "live spawn-loop task or claim-lock");
    }

    #[test]
    fn test_unmanaged_worktree_is_never_reaped() {
        // No `.loom-managed` sentinel ⇒ user-provisioned ⇒ never touched,
        // even though a process is using it and no claim is live.
        let repo = make_repo(&[(102, false)]);
        let (report, killed) = run_pass(repo.path(), &Spec::default());

        assert!(report.reaped.is_empty());
        assert!(killed.is_empty());
        assert_eq!(report.preserved[0].1, "no .loom-managed sentinel (user-provisioned)");
    }

    #[test]
    fn test_open_pr_worktree_is_never_reaped() {
        // The gap this pass had at first review (PR #5121): `active_issues` is
        // populated ONLY by `SweepRegistry::dispatch`, so Manual Orchestration
        // Mode (`/loom:judge`, `/loom:doctor`, `/loom:builder`) and manual
        // `/loom:sweep` runs never appear in it — yet a Judge reviewing an open
        // PR reuses that PR's builder worktree and runs `cargo build`/`cargo
        // test` there. With only the sentinel + claim gates, this pass would
        // have SIGKILLed that live reviewer's process tree. An open PR is
        // independent proof the work is alive.
        let repo = make_repo(&[(5110, true)]);
        let spec = Spec {
            active: HashSet::new(),
            issue_state: "CLOSED".to_string(),
            pr_status: PrStatus::Open,
            root_pids: vec![4242],
            descendants: |_| vec![4243, 4244],
            ..Spec::default()
        };
        let (report, killed) = run_pass(repo.path(), &spec);

        assert!(
            report.reaped.is_empty(),
            "a worktree whose PR is still open must never have its processes reaped"
        );
        assert!(killed.is_empty(), "no signal may be delivered at all");
        assert_eq!(report.preserved, vec![(5110, "PR still open".to_string())]);
    }

    #[test]
    fn test_open_issue_worktree_is_never_reaped() {
        // The same protection one step earlier in the lifecycle: a Builder
        // working an open issue manually has no daemon claim either.
        let repo = make_repo(&[(103, true)]);
        let spec = Spec {
            issue_state: "OPEN".to_string(),
            ..Spec::default()
        };
        let (report, killed) = run_pass(repo.path(), &spec);

        assert!(report.reaped.is_empty());
        assert!(killed.is_empty());
        assert_eq!(report.preserved[0].1, "issue is OPEN");
    }

    #[test]
    fn test_unknown_forge_state_is_never_reaped() {
        // A failed forge probe resolves to UNKNOWN, which is a skip — never a
        // kill (the same posture the removal pass takes).
        for (issue_state, pr_status, expect) in [
            (
                "UNKNOWN",
                PrStatus::Merged {
                    merged_at: "2020-01-01T00:00:00Z".to_string(),
                },
                "issue is UNKNOWN",
            ),
            ("CLOSED", PrStatus::Unknown, "PR status unknown"),
            // A merged PR with an unparseable timestamp is an unknown merge
            // time, not an expired grace period.
            (
                "CLOSED",
                PrStatus::Merged {
                    merged_at: "not-a-timestamp".to_string(),
                },
                "PR status unknown",
            ),
        ] {
            let repo = make_repo(&[(104, true)]);
            let spec = Spec {
                issue_state: issue_state.to_string(),
                pr_status,
                ..Spec::default()
            };
            let (report, killed) = run_pass(repo.path(), &spec);
            assert!(report.reaped.is_empty(), "{expect}");
            assert!(killed.is_empty(), "{expect}");
            assert_eq!(report.preserved[0].1, expect);
        }
    }

    #[test]
    fn test_post_merge_grace_period_protects_the_worktree() {
        // Merged seconds ago: the merging agent may still be finishing inside
        // the worktree, so the same grace the removal pass honors applies.
        let repo = make_repo(&[(105, true)]);
        let merged_at = (Utc::now() - chrono::Duration::seconds(30)).to_rfc3339();
        let spec = Spec {
            pr_status: PrStatus::Merged { merged_at },
            ..Spec::default()
        };
        let (report, killed) = run_pass(repo.path(), &spec);

        assert!(report.reaped.is_empty());
        assert!(killed.is_empty());
        assert!(
            report.preserved[0].1.starts_with("grace period not passed"),
            "unexpected reason: {}",
            report.preserved[0].1
        );
    }

    #[test]
    fn test_closed_issue_without_merged_pr_is_still_reaped() {
        // A closed issue whose PR closed unmerged (or that never had one) is
        // as dead as a merged one — nothing is legitimately working there.
        for pr_status in [PrStatus::ClosedNoMerge, PrStatus::NoPr] {
            let repo = make_repo(&[(106, true)]);
            let spec = Spec {
                pr_status,
                ..Spec::default()
            };
            let (report, killed) = run_pass(repo.path(), &spec);
            assert_eq!(report.reaped.len(), 1);
            assert_eq!(killed, vec![vec![999]]);
        }
    }

    #[test]
    fn test_no_processes_using_worktree_is_a_no_op() {
        let repo = make_repo(&[(107, true)]);
        let spec = Spec {
            root_pids: Vec::new(),
            ..Spec::default()
        };
        let (report, killed) = run_pass(repo.path(), &spec);

        assert_eq!(report.scanned, 1);
        assert!(report.reaped.is_empty());
        assert!(report.preserved.is_empty());
        assert!(killed.is_empty());
    }

    #[test]
    fn test_idle_worktree_costs_no_forge_probe() {
        // The forge gates must not turn a quiet host into a per-tick REST
        // storm: with nothing running inside the worktree there is nothing to
        // decide, so neither probe may be called.
        let repo = make_repo(&[(108, true)]);
        let probed: RefCell<Vec<u32>> = RefCell::new(Vec::new());
        let processes_using = |_: &Path| Vec::new();
        let issue_state = |n: u32| {
            probed.borrow_mut().push(n);
            "CLOSED".to_string()
        };
        let pr_status = |n: u32| {
            probed.borrow_mut().push(n);
            PrStatus::NoPr
        };
        let collect_descendants = |_: u32| Vec::new();
        let kill = |pids: &[u32]| pids.to_vec();

        let report = reap_orphan_processes_with(
            repo.path(),
            &OrphanReapProbes {
                active_issues: &HashSet::new(),
                processes_using: &processes_using,
                issue_state: &issue_state,
                pr_status: &pr_status,
                collect_descendants: &collect_descendants,
                kill: &kill,
                grace_period_secs: DEFAULT_GRACE_PERIOD_SECS,
                now: Utc::now(),
            },
        );

        assert_eq!(report.scanned, 1);
        assert!(probed.borrow().is_empty(), "idle worktree must not hit the forge");
    }

    #[test]
    fn test_missing_worktree_root_is_a_clean_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let (report, killed) = run_pass(tmp.path(), &Spec::default());

        assert_eq!(report, OrphanReapReport::default());
        assert!(killed.is_empty());
    }

    #[test]
    fn test_non_issue_directories_are_ignored() {
        let repo = make_repo(&[(200, true)]);
        fs::create_dir_all(repo.path().join(".loom/worktrees/scratch")).unwrap();
        let spec = Spec {
            root_pids: Vec::new(),
            ..Spec::default()
        };
        let (report, _) = run_pass(repo.path(), &spec);

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
            preserved: vec![(2, "PR still open".to_string())],
        };
        assert_eq!(report.summary(), "scanned=3 reaped_worktrees=1 preserved=1");
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
