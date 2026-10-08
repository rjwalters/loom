//! Stopping one agent's whole process tree at H4 (issue #10831; design
//! `docs/design/daemon-roll-pause-resume.md` §5).
//!
//! "Nothing from an agent's tree may outlive H4." An agent's tree is more than
//! its process group: dev servers `setsid` or double-fork, and GNU `timeout`
//! moves its child to a new group (`orphan_process_reaper.rs`'s module doc has
//! the incident). So the teardown has three legs, and always runs the last two:
//!
//! 1. **systemd scope** (Linux, when a scope unit was recorded):
//!    `systemctl --user stop <unit>` kills the whole cgroup, `setsid`
//!    descendants included. A recorded unit is **not** proof the scope exists:
//!    `owner.json` names one for every Linux dispatch, including Codex and
//!    Claude launches that never created a scope. When the stop fails or the
//!    unit is not loaded, the legs below do the work (Judge finding on #10864).
//! 2. **Freeze-first tree kill** over every seed attributed to the item: its
//!    pid, every member of its process group, and every process working in its
//!    worktree (cwd or argv), each expanded to its descendants through the
//!    ppid map, which survives `setsid` where the pgid does not. This reuses
//!    [`crate::orphan_process_reaper::reap_tree`] (`SIGSTOP` parent-first,
//!    re-snapshot, `SIGTERM` + `SIGCONT`, `SIGKILL` the survivors).
//! 3. **Process-group `SIGKILL`** for anything still in the group.
//!
//! **Session-exec (containerized) items.** The host side of a session-exec
//! invocation is part of the tree above. `spawn-codex.sh` traps `TERM` and
//! writes its `<stderr>.cancel` marker (`session_exec/host.rs`), so leg 2's
//! `SIGTERM` is what revokes the invocation: the in-container worker cancels
//! the invocation's tree and the session container keeps running. The grace
//! between `SIGTERM` and `SIGKILL` is what gives that trap time to run.
//!
//! The process table comes from `ps`, not `/proc`, so the same code works under
//! launchd (macOS) and systemd (Linux). The daemon's own pid and its ancestors
//! are never signalled.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::orphan_process_reaper::{
    children_map, descendants_of, order_parent_first, parent_map, reap_tree, references_worktree,
    OrphanTree, ProcEntry, ReapHooks,
};
use crate::sweep_registry::reaper::{group_has_members, send_group_signal, send_signal};

/// How long a stopped tree gets between `SIGTERM` and `SIGKILL`.
pub const TERM_GRACE: Duration = Duration::from_secs(3);
/// Bound on the `systemctl --user stop` call.
const SCOPE_STOP_TIMEOUT: Duration = Duration::from_secs(20);

/// What identifies one item's tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeSpec {
    /// The agent's tracked pid (the spawn script; it leads its group).
    pub pid: Option<u32>,
    /// Its process group.
    pub pgid: Option<u32>,
    /// The systemd scope unit recorded at dispatch, if any. May not exist.
    pub scope_unit: Option<String>,
    /// The item's worktree, for worktree-attributed seeds. `None` for a role
    /// run, which has no worktree of its own.
    pub worktree: Option<PathBuf>,
}

/// What a teardown did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TeardownReport {
    /// `true` when `systemctl --user stop` stopped a real scope.
    pub scope_stopped: bool,
    /// Why the scope leg did not apply, when a unit was recorded but not used.
    pub scope_note: Option<String>,
    /// Pids the tree kill signalled.
    pub pids: Vec<u32>,
    /// Pids still alive after the whole escalation.
    pub survivors: Vec<u32>,
    /// Wall time, milliseconds.
    pub elapsed_ms: u64,
}

/// One row of the process table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    pub cmdline: String,
    pub cwd: Option<PathBuf>,
}

/// Parse `ps -axo pid=,ppid=,pgid=,stat=,command=` output. Zombies (`stat`
/// starting with `Z`) are dropped: a killed process stays in the table until
/// its parent reaps it, and it is not running.
#[must_use]
pub fn parse_ps(out: &str) -> Vec<Proc> {
    out.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let pgid = it.next()?.parse().ok()?;
            if it.next()?.starts_with('Z') {
                return None;
            }
            let cmdline = it.collect::<Vec<_>>().join(" ");
            Some(Proc {
                pid,
                ppid,
                pgid,
                cmdline,
                cwd: None,
            })
        })
        .collect()
}

/// The live process table. `with_cwd` also resolves each process's cwd
/// (`/proc/<pid>/cwd` on Linux, one `lsof` call elsewhere), which is only
/// needed for worktree attribution. Empty when `ps` cannot be run.
#[must_use]
pub fn process_table(with_cwd: bool) -> Vec<Proc> {
    let Ok(out) = Command::new("ps")
        .args(["-axo", "pid=,ppid=,pgid=,stat=,command="])
        .output()
    else {
        return Vec::new();
    };
    let mut procs = parse_ps(&String::from_utf8_lossy(&out.stdout));
    if with_cwd {
        fill_cwds(&mut procs);
    }
    procs
}

#[cfg(target_os = "linux")]
fn fill_cwds(procs: &mut [Proc]) {
    for p in procs {
        p.cwd = std::fs::read_link(format!("/proc/{}/cwd", p.pid)).ok();
    }
}

#[cfg(not(target_os = "linux"))]
fn fill_cwds(procs: &mut [Proc]) {
    // `-F pn`: one `p<pid>` line per process, then `n<path>` for its cwd.
    let Ok(out) = Command::new("lsof")
        .args(["-a", "-d", "cwd", "-n", "-P", "-F", "pn"])
        .output()
    else {
        return;
    };
    let cwds = parse_lsof_cwds(&String::from_utf8_lossy(&out.stdout));
    for p in procs {
        p.cwd = cwds.get(&p.pid).cloned();
    }
}

/// Parse `lsof -a -d cwd -F pn` output into `pid -> cwd`.
#[must_use]
pub fn parse_lsof_cwds(out: &str) -> HashMap<u32, PathBuf> {
    let mut map = HashMap::new();
    let mut pid: Option<u32> = None;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            pid = rest.parse().ok();
        } else if let (Some(rest), Some(p)) = (line.strip_prefix('n'), pid) {
            map.insert(p, PathBuf::from(rest));
        }
    }
    map
}

fn as_entries(procs: &[Proc]) -> Vec<ProcEntry> {
    procs
        .iter()
        .map(|p| ProcEntry {
            pid: p.pid,
            ppid: p.ppid,
            cwd: p.cwd.clone(),
            cmdline: p.cmdline.clone(),
            age_secs: None,
        })
        .collect()
}

/// The pids of `spec`'s tree in `procs`, parent-first: every seed (the pid, the
/// group's members, the worktree's processes) plus its descendants, minus
/// `protected` (this daemon and its ancestors) and pids 0/1.
#[must_use]
pub fn plan_tree(spec: &TreeSpec, procs: &[Proc], protected: &HashSet<u32>) -> Vec<u32> {
    let entries = as_entries(procs);
    let mut seeds: Vec<u32> = Vec::new();
    for (p, e) in procs.iter().zip(&entries) {
        let by_pid = spec.pid == Some(p.pid);
        let by_group = spec.pgid.is_some_and(|g| g > 1 && p.pgid == g);
        let by_worktree = spec
            .worktree
            .as_deref()
            .is_some_and(|w| references_worktree(e, w));
        if by_pid || by_group || by_worktree {
            seeds.push(p.pid);
        }
    }
    let children = children_map(&entries);
    let mut all: HashSet<u32> = seeds.iter().copied().collect();
    all.extend(descendants_of(&seeds, &children));
    all.retain(|pid| *pid > 1 && !protected.contains(pid));
    order_parent_first(&all, &parent_map(&entries))
}

/// This process and every ancestor of it: never signalled.
fn protected_pids(procs: &[Proc]) -> HashSet<u32> {
    let me = std::process::id();
    let parents = parent_map(&as_entries(procs));
    let mut set = crate::orphan_process_reaper::ancestors_of(me, &parents);
    set.insert(me);
    set
}

/// Whether `unit` is a loaded systemd user unit. `false` on any error.
fn scope_is_loaded(unit: &str) -> bool {
    let mut cmd = Command::new("systemctl");
    cmd.args(["--user", "show", unit, "--property=LoadState", "--value"]);
    matches!(
        crate::sweep_registry::reaper::output_with_timeout(cmd, SCOPE_STOP_TIMEOUT),
        Ok(Some(out)) if out.status.success()
            && String::from_utf8_lossy(&out.stdout).trim() == "loaded"
    )
}

/// Leg 1: stop the recorded scope if it really exists. `Ok(())` when it was
/// stopped, `Err(why)` when the caller must rely on the fallback legs.
fn stop_scope(unit: &str) -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("no systemd on this platform".to_string());
    }
    if !scope_is_loaded(unit) {
        return Err(format!("scope unit {unit} is not loaded (it was never created)"));
    }
    let mut cmd = Command::new("systemctl");
    cmd.args(["--user", "stop", unit]);
    match crate::sweep_registry::reaper::output_with_timeout(cmd, SCOPE_STOP_TIMEOUT) {
        Ok(Some(out)) if out.status.success() => Ok(()),
        Ok(Some(out)) => Err(format!(
            "systemctl --user stop {unit} exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Ok(None) => Err(format!("systemctl --user stop {unit} timed out")),
        Err(e) => Err(format!("systemctl --user stop {unit}: {e}")),
    }
}

/// Stop `spec`'s whole tree. Never fails: every leg is best-effort and the
/// report says what is left.
#[must_use]
pub fn teardown_tree(spec: &TreeSpec, grace: Duration) -> TeardownReport {
    let started = Instant::now();
    let mut report = TeardownReport::default();

    if let Some(unit) = spec.scope_unit.as_deref() {
        match stop_scope(unit) {
            Ok(()) => report.scope_stopped = true,
            Err(why) => report.scope_note = Some(why),
        }
    }

    let with_cwd = spec.worktree.is_some();
    let procs = process_table(with_cwd);
    let protected = protected_pids(&procs);
    let pids = plan_tree(spec, &procs, &protected);
    if !pids.is_empty() {
        let tree = OrphanTree {
            issue: 0,
            worktree: spec.worktree.clone().unwrap_or_default(),
            seeds: pids.clone(),
            pids: pids.clone(),
            details: Vec::new(),
            protected_pids: Vec::new(),
        };
        // The post-freeze re-snapshot must come from the same table source, so
        // a child forked between the scan and the freeze is caught on macOS
        // too (the orphan reaper's own snapshot is `/proc`-only).
        let snapshot = || as_entries(&process_table(false));
        let signal = |pid: u32, sig: i32| !protected.contains(&pid) && send_signal(pid, sig);
        let hooks = ReapHooks {
            signal: &signal,
            snapshot: &snapshot,
            is_alive: &crate::sweep_registry::is_pid_alive,
            sleep: &std::thread::sleep,
        };
        let outcome = reap_tree(&tree, grace, &hooks);
        report.pids = pids;
        report.pids.extend(outcome.late_arrivals.iter().copied());
        // `kill(pid, 0)` still succeeds on a zombie, and the tree's leader is
        // this daemon's own unreaped child. Only a pid still in the (zombie-
        // free) table really survived.
        if !outcome.survivors.is_empty() {
            let live: HashSet<u32> = process_table(false).iter().map(|p| p.pid).collect();
            report.survivors = outcome
                .survivors
                .into_iter()
                .filter(|pid| live.contains(pid))
                .collect();
        }
    }

    // Leg 3: whatever is still in the group.
    if let Some(pgid) = spec.pgid.filter(|g| *g > 1 && !protected.contains(g)) {
        if group_has_members(pgid) {
            send_group_signal(pgid, libc::SIGKILL);
        }
    }
    report.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    report
}

/// Whether `pid` is a running process: alive and not a zombie. A child that
/// exited but has not been reaped still answers `kill(pid, 0)`; H4 holds the
/// sweep reaper off its items, so that is exactly the state an agent that
/// ended by itself is left in.
#[must_use]
pub fn pid_running(pid: u32) -> bool {
    if !crate::sweep_registry::is_pid_alive(pid) {
        return false;
    }
    let Ok(out) = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
    else {
        return true; // cannot tell: the liveness probe said alive
    };
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !(stat.is_empty() && !out.status.success()) && !stat.starts_with('Z')
}

/// Whether anything of `spec`'s tree is still alive: its pid, its group, or a
/// process working in its worktree.
#[cfg(test)]
#[must_use]
pub fn tree_alive(spec: &TreeSpec) -> bool {
    if spec.pid.is_some_and(pid_running) {
        return true;
    }
    let procs = process_table(false);
    if spec
        .pgid
        .is_some_and(|g| g > 1 && procs.iter().any(|p| p.pgid == g))
    {
        return true;
    }
    spec.worktree.as_deref().is_some_and(worktree_has_processes)
}

#[cfg(test)]
fn worktree_has_processes(worktree: &std::path::Path) -> bool {
    let procs = process_table(true);
    let protected = protected_pids(&procs);
    as_entries(&procs)
        .iter()
        .any(|e| !protected.contains(&e.pid) && references_worktree(e, worktree))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn p(pid: u32, ppid: u32, pgid: u32, cmd: &str) -> Proc {
        Proc {
            pid,
            ppid,
            pgid,
            cmdline: cmd.to_string(),
            cwd: None,
        }
    }

    #[test]
    fn ps_rows_parse_with_spaces_in_the_command() {
        let procs = parse_ps(
            "  10     1    10 Ss /bin/sh -c sleep 5\n bad line\n 11 10 10 S+ sleep 5\n 12 10 10 Z (perl)\n",
        );
        assert_eq!(procs.len(), 2, "the zombie row is dropped");
        assert_eq!(procs[0].cmdline, "/bin/sh -c sleep 5");
        assert_eq!((procs[1].pid, procs[1].ppid, procs[1].pgid), (11, 10, 10));
    }

    #[test]
    fn lsof_cwd_rows_parse() {
        let map = parse_lsof_cwds("p10\nfcwd\nn/w/issue-1\np11\nn/elsewhere\n");
        assert_eq!(map[&10], PathBuf::from("/w/issue-1"));
        assert_eq!(map[&11], PathBuf::from("/elsewhere"));
    }

    /// A pgid-only kill is not enough (`orphan_process_reaper.rs:16-35`): the
    /// plan follows the ppid link across a `setsid`, and picks up a reparented
    /// process by its worktree.
    #[test]
    fn the_plan_covers_the_group_a_setsid_child_and_a_worktree_orphan() {
        let wt = PathBuf::from("/r/.loom/worktrees/issue-7");
        let mut orphan = p(400, 1, 400, "vite --port 5173");
        orphan.cwd = Some(wt.join("app"));
        let procs = vec![
            p(100, 50, 100, "spawn-claude.sh"),
            p(101, 100, 100, "claude -p"),
            p(102, 101, 102, "timeout 60 ngspice"), // new group
            p(103, 102, 103, "dev-server"),         // setsid'd, still a descendant
            p(200, 50, 200, "other agent"),
            orphan,
            p(500, 1, 500, "/usr/bin/tool --cwd /r/.loom/worktrees/issue-70"),
        ];
        let spec = TreeSpec {
            pid: Some(100),
            pgid: Some(100),
            scope_unit: None,
            worktree: Some(wt),
        };
        let plan = plan_tree(&spec, &procs, &HashSet::new());
        let set: HashSet<u32> = plan.iter().copied().collect();
        assert_eq!(set, HashSet::from([100, 101, 102, 103, 400]));
        // Parent-first: the leader is frozen before its children.
        let pos = |pid| plan.iter().position(|x| *x == pid).unwrap();
        assert!(pos(100) < pos(101) && pos(101) < pos(102) && pos(102) < pos(103));
    }

    #[test]
    fn the_daemon_and_its_ancestors_are_never_planned() {
        let procs = vec![
            p(1, 0, 1, "launchd"),
            p(50, 1, 50, "loom-daemon"),
            p(100, 50, 100, "a"),
        ];
        let spec = TreeSpec {
            pid: Some(50), // a corrupt record naming the daemon itself
            pgid: None,
            scope_unit: None,
            worktree: None,
        };
        let plan = plan_tree(&spec, &procs, &HashSet::from([50, 1]));
        assert_eq!(plan, vec![100], "descendants only; never the protected pids");
    }

    /// The real thing: a tree whose child has left the process group with
    /// `setsid` is entirely gone after the teardown, and a recorded scope unit
    /// that does not exist falls back to the tree kill.
    #[test]
    fn a_real_tree_with_a_setsid_child_is_fully_stopped() {
        use std::os::unix::process::CommandExt;
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("child.pid");
        // The child calls setsid() (new session AND group), records its pid
        // and sleeps; the parent sleeps too.
        let script = format!(
            "perl -e 'use POSIX; my $p = fork(); if ($p == 0) {{ POSIX::setsid(); \
             open(my $f, \">\", \"{}\"); print $f $$; close($f); sleep 300; exit 0 }} sleep 300'",
            pidfile.display()
        );
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&script).process_group(0);
        let mut child = cmd.spawn().unwrap();
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(20);
        let setsid_pid: u32 = loop {
            if let Some(n) = std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                break n;
            }
            assert!(Instant::now() < deadline, "the setsid child never started");
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(crate::sweep_registry::is_pid_alive(setsid_pid));

        let spec = TreeSpec {
            pid: Some(pid),
            pgid: Some(pid),
            scope_unit: Some("loom-agent-does-not-exist.scope".to_string()),
            worktree: None,
        };
        let report = teardown_tree(&spec, Duration::from_millis(300));
        let _ = child.wait();
        assert!(!report.scope_stopped, "the recorded scope never existed");
        assert!(report.scope_note.is_some());
        assert!(report.pids.contains(&setsid_pid), "{report:?}");
        assert!(report.survivors.is_empty(), "{report:?}");
        let gone = Instant::now() + Duration::from_secs(5);
        while crate::sweep_registry::is_pid_alive(setsid_pid) && Instant::now() < gone {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !crate::sweep_registry::is_pid_alive(setsid_pid),
            "the setsid'd child outlived the teardown"
        );
        assert!(!tree_alive(&spec));
    }
}
