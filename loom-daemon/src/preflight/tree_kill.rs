//! Whole-tree teardown of a timed-out pre-flight gate (Issue #10955).
//!
//! # The measured bug
//!
//! Pre-flight used to SIGKILL the gate shell's process group and nothing
//! else. `build-gate.sh` runs every stage through `bounded_run`, which uses
//! GNU `timeout` when it is installed, and GNU `timeout` moves itself and its
//! command into a **new** process group. So on a timeout `timeout`,
//! `cargo-nextest` and every test under them survived, were reparented to
//! init/launchd and kept running from the Builder's worktree. Each timeout
//! added load, which caused more timeouts. The SIGKILL also skipped the gate's
//! own EXIT trap, which is what releases its build slot.
//!
//! # What this does
//!
//! 1. The gate is spawned as the leader of a **new session** ([`own_session`]).
//!    A process keeps its session id when it changes process group and when
//!    its parent dies, so "session id == the gate's pid" still identifies a
//!    gate process after both.
//! 2. On a kill the process table is snapshotted *before* any signal (a dead
//!    shell takes the parent links with it) and every gate process is
//!    collected: the gate's session, the process groups and sessions of
//!    anything already collected, and every descendant by parent pid (which
//!    reaches a child that called `setsid` itself).
//! 3. SIGTERM each one, then re-snapshot every [`POLL`] for up to the grace so
//!    the gate's EXIT trap can run and late forks are picked up.
//! 4. SIGKILL whatever is left, re-snapshotting until nothing is.
//!
//! The same code runs on macOS and Linux: the snapshot is POSIX `ps -A -o`
//! plus `getsid(2)`, neither of which needs `/proc`.
//!
//! # What may be signalled
//!
//! Only a process tied to the gate this call spawned, by session, process
//! group or parent chain. The caller's own pid, process group and session are
//! never collected, and neither is pid 0 or 1. Signals go to individual pids
//! from a snapshot at most one [`POLL`] old; the one group signal targets the
//! gate's own group while the gate is still unreaped, so its id cannot have
//! been reused.
//!
//! # What it cannot reach
//!
//! A process that left the gate's session (`setsid`) **and** lost its parent
//! before the timeout has no remaining link to the gate, so it is not found.
//! If `ps` is unavailable the teardown degrades to signalling the gate's own
//! process group (the pre-#10955 reach), still with the SIGTERM grace.

use std::process::{Child, Command};
use std::time::Duration;

/// How long gate processes get between SIGTERM and SIGKILL. `build-gate.sh`'s
/// EXIT trap releases the build slot and then reaps its own group with a 2 s
/// pause, so this must comfortably exceed 2 s.
pub(super) const TERM_GRACE: Duration = Duration::from_secs(5);

/// Make `cmd` the leader of a new session (and so of a new process group whose
/// id is its pid), so the gate's processes stay identifiable by session id.
#[cfg(unix)]
pub(super) fn own_session(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: `setsid` is async-signal-safe, which is all that may run between
    // fork and exec. It cannot fail here for the documented reason (the caller
    // is a process-group leader): the forked child is never one.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
pub(super) fn own_session(_cmd: &mut Command) {}

/// Terminate `child` and every process tied to it, then reap it. Returns how
/// many gate processes were still alive after SIGKILL (`0` when the snapshot
/// was unavailable and the answer is unknown).
#[cfg(unix)]
pub(super) fn kill_tree(child: &mut Child, grace: Duration) -> usize {
    let survivors = match libc::pid_t::try_from(child.id()) {
        Ok(root) if root > 1 => unix::terminate(root, child, grace),
        _ => 0,
    };
    let _ = child.kill();
    let _ = child.wait();
    survivors
}

#[cfg(not(unix))]
pub(super) fn kill_tree(child: &mut Child, _grace: Duration) -> usize {
    let _ = child.kill();
    let _ = child.wait();
    0
}

#[cfg(unix)]
mod unix {
    use std::collections::HashSet;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use libc::pid_t;

    /// Re-snapshot cadence while waiting out the grace.
    const POLL: Duration = Duration::from_millis(100);
    /// How long to keep re-snapshotting after SIGKILL before giving up.
    const KILL_WAIT: Duration = Duration::from_secs(3);
    /// Budget for one `ps` run; a wedged `ps` must not wedge the teardown.
    const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(5);

    /// One row of the process table.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct Proc {
        pub pid: pid_t,
        pub ppid: pid_t,
        pub pgid: pid_t,
        /// `getsid(pid)`; `None` when it could not be read (already gone).
        pub sid: Option<pid_t>,
        /// Exited but not yet reaped: nothing left to signal.
        pub zombie: bool,
    }

    /// Parse `ps -A -o pid=,ppid=,pgid=,stat=` (the session id is filled in by
    /// the caller). Unparseable rows are dropped.
    pub(super) fn parse_ps(out: &str) -> Vec<Proc> {
        out.lines()
            .filter_map(|l| {
                let mut f = l.split_whitespace();
                let pid = f.next()?.parse().ok()?;
                let ppid = f.next()?.parse().ok()?;
                let pgid = f.next()?.parse().ok()?;
                let zombie = f.next().is_some_and(|s| s.starts_with('Z'));
                Some(Proc {
                    pid,
                    ppid,
                    pgid,
                    sid: None,
                    zombie,
                })
            })
            .collect()
    }

    /// The live process table, or `None` when `ps` could not be run.
    fn snapshot() -> Option<Vec<Proc>> {
        let mut cmd = Command::new("ps");
        cmd.args(["-A", "-o", "pid=,ppid=,pgid=,stat="])
            .stdin(Stdio::null());
        let out = crate::proc_exec::run_bounded(cmd, SNAPSHOT_TIMEOUT)
            .ok()?
            .output()?;
        if !out.status.success() {
            return None;
        }
        let mut procs = parse_ps(&String::from_utf8_lossy(&out.stdout));
        for p in &mut procs {
            // SAFETY: plain syscall that only reads; -1 (ESRCH: the process
            // exited since `ps` ran) is mapped to `None`.
            let sid = unsafe { libc::getsid(p.pid) };
            p.sid = (sid > 0).then_some(sid);
        }
        Some(procs)
    }

    /// The set of processes tied to one gate, carried across snapshots.
    #[derive(Debug)]
    pub(super) struct Tree {
        root: pid_t,
        /// The caller's own pid / process group / session: never collected.
        own: [pid_t; 3],
        pids: HashSet<pid_t>,
        groups: HashSet<pid_t>,
        sessions: HashSet<pid_t>,
    }

    impl Tree {
        pub(super) fn new(root: pid_t, own: [pid_t; 3]) -> Self {
            Self {
                root,
                own,
                pids: HashSet::from([root]),
                groups: HashSet::from([root]),
                sessions: HashSet::from([root]),
            }
        }

        fn may_signal(&self, p: &Proc) -> bool {
            !p.zombie && p.pid > 1 && p.pid != self.own[0]
        }

        /// Fold one snapshot in and return the live gate processes in it.
        ///
        /// A process is collected when it was collected last time and is still
        /// there, shares a collected process group or session, or descends
        /// from a collected process. The carried sets are then rebuilt from
        /// this snapshot only, so an id that has emptied out is forgotten
        /// rather than trusted after the kernel may have reused it.
        pub(super) fn absorb(&mut self, procs: &[Proc]) -> Vec<pid_t> {
            let mut live: HashSet<pid_t> = procs
                .iter()
                .filter(|p| {
                    self.may_signal(p)
                        && (self.pids.contains(&p.pid)
                            || self.groups.contains(&p.pgid)
                            || p.sid.is_some_and(|s| self.sessions.contains(&s)))
                })
                .map(|p| p.pid)
                .collect();
            loop {
                let before = live.len();
                for p in procs {
                    if self.may_signal(p) && (p.ppid == self.root || live.contains(&p.ppid)) {
                        live.insert(p.pid);
                    }
                }
                if live.len() == before {
                    break;
                }
            }
            self.pids.clone_from(&live);
            self.groups = HashSet::from([self.root]);
            self.sessions = HashSet::from([self.root]);
            for p in procs.iter().filter(|p| live.contains(&p.pid)) {
                if p.pgid > 1 && p.pgid != self.own[1] {
                    self.groups.insert(p.pgid);
                }
                if let Some(sid) = p.sid.filter(|&s| s > 1 && s != self.own[2]) {
                    self.sessions.insert(sid);
                }
            }
            let mut live: Vec<pid_t> = live.into_iter().collect();
            live.sort_unstable();
            live
        }
    }

    fn signal(pid: pid_t, sig: libc::c_int) {
        // SAFETY: plain syscall on one positive pid taken from the gate's
        // tree; ESRCH (already gone) is expected and ignored.
        unsafe {
            libc::kill(pid, sig);
        }
    }

    fn signal_root_group(root: pid_t, sig: libc::c_int) {
        // SAFETY: `root` is a child this module's caller spawned as a session
        // leader, so it leads a group created for it, and it has not been
        // reaped yet, so its id cannot have been reused.
        unsafe {
            libc::killpg(root, sig);
        }
    }

    /// SIGTERM the gate's tree, wait out `grace`, SIGKILL the rest. Does not
    /// reap `child` on the snapshot path (the caller does), which is what
    /// keeps the root's pid, group and session ids from being reused meanwhile.
    pub(super) fn terminate(root: pid_t, child: &mut Child, grace: Duration) -> usize {
        // SAFETY: plain syscalls that only read the caller's own ids.
        let own = unsafe { [libc::getpid(), libc::getpgrp(), libc::getsid(0)] };
        let mut tree = Tree::new(root, own);
        // Each process gets exactly ONE SIGTERM: bash dies at once on a second
        // TERM that lands while it is running its EXIT trap, which would skip
        // the very cleanup the grace exists for.
        let mut termed: HashSet<pid_t> = HashSet::new();
        let mut group_termed = false;
        let deadline = Instant::now() + grace;
        loop {
            // Snapshot BEFORE the first signal: once the shell dies its
            // children are reparented, and a child in its own session is then
            // tied to the gate by nothing at all.
            let procs = snapshot();
            let live = procs.as_ref().map(|procs| tree.absorb(procs));
            if !group_termed {
                // The gate's own group in one signal, which also reaches a
                // fork that raced the snapshot (and is all there is when the
                // snapshot is unavailable). Its members seen in the snapshot
                // are not signalled again below.
                signal_root_group(root, libc::SIGTERM);
                group_termed = true;
                for p in procs.iter().flatten().filter(|p| p.pgid == root) {
                    termed.insert(p.pid);
                }
            }
            for pid in live.iter().flatten() {
                if termed.insert(*pid) {
                    signal(*pid, libc::SIGTERM);
                }
            }
            match live {
                Some(live) if live.is_empty() => break,
                Some(_) => {}
                // No process table: all that can be watched is the shell.
                None if matches!(child.try_wait(), Ok(Some(_))) => break,
                None => {}
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(POLL);
        }
        // Immediately after the last look at the shell, so a reap on the
        // no-snapshot path above leaves no window for its id to be reused.
        signal_root_group(root, libc::SIGKILL);
        let deadline = Instant::now() + KILL_WAIT;
        loop {
            let Some(procs) = snapshot() else {
                return 0;
            };
            let live = tree.absorb(&procs);
            if live.is_empty() {
                return 0;
            }
            for pid in &live {
                signal(*pid, libc::SIGKILL);
            }
            if Instant::now() >= deadline {
                return live.len();
            }
            std::thread::sleep(POLL);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::unix::{parse_ps, Proc, Tree};
    use super::*;
    use std::path::Path;
    use std::process::Stdio;
    use std::time::Instant;

    const ROOT: libc::pid_t = 500;
    /// The caller: pid 90, process group 80, session 70.
    const OWN: [libc::pid_t; 3] = [90, 80, 70];

    fn p(pid: i32, ppid: i32, pgid: i32, sid: i32) -> Proc {
        Proc {
            pid,
            ppid,
            pgid,
            sid: Some(sid),
            zombie: false,
        }
    }

    #[test]
    fn parses_ps_rows_and_marks_zombies() {
        let rows = parse_ps("    1     0     1 Ss  \n  42    1   42 Z\njunk\n 7 1 7\n");
        assert_eq!(rows.len(), 3);
        assert_eq!((rows[0].pid, rows[0].ppid, rows[0].pgid), (1, 0, 1));
        assert!(!rows[0].zombie && rows[1].zombie && !rows[2].zombie);
    }

    /// The measured shape: GNU `timeout` (501) in its own group under the gate
    /// shell, nextest (502) under it, a test (503) in yet another group.
    #[test]
    fn collects_descendants_in_other_process_groups() {
        let procs = [
            p(ROOT, 90, ROOT, ROOT),
            p(501, ROOT, 501, ROOT),
            p(502, 501, 501, ROOT),
            p(503, 502, 503, ROOT),
            p(600, 1, 600, 600),
        ];
        let mut t = Tree::new(ROOT, OWN);
        assert_eq!(t.absorb(&procs), vec![ROOT, 501, 502, 503]);
    }

    #[test]
    fn collects_an_orphan_by_session_after_the_shell_died() {
        // The shell is a zombie; its children were reparented to pid 1 and sit
        // in their own group. Only the session id still ties them to the gate.
        let mut shell = p(ROOT, 90, ROOT, ROOT);
        shell.zombie = true;
        let procs = [shell, p(501, 1, 501, ROOT), p(502, 501, 501, ROOT)];
        let mut t = Tree::new(ROOT, OWN);
        assert_eq!(t.absorb(&procs), vec![501, 502]);
    }

    #[test]
    fn follows_a_child_into_a_new_session_and_keeps_it_once_orphaned() {
        // 501 called setsid: found by parent pid only.
        let mut t = Tree::new(ROOT, OWN);
        let first = [p(ROOT, 90, ROOT, ROOT), p(501, ROOT, 501, 501)];
        assert_eq!(t.absorb(&first), vec![ROOT, 501]);
        // The shell is gone: 501 is carried over, and its own child (502,
        // forked since) is found through 501's session.
        let second = [p(501, 1, 501, 501), p(502, 1, 502, 501)];
        assert_eq!(t.absorb(&second), vec![501, 502]);
    }

    #[test]
    fn never_collects_the_caller_or_its_group_or_session() {
        // A gate child that somehow reports the caller's group and session
        // must not drag the caller's other processes in.
        let procs = [
            p(ROOT, 90, ROOT, ROOT),
            p(501, ROOT, 80, 70),
            p(90, 1, 80, 70),
            p(91, 90, 80, 70),
            p(1, 0, 1, 1),
        ];
        let mut t = Tree::new(ROOT, OWN);
        assert_eq!(t.absorb(&procs), vec![ROOT, 501]);
        assert_eq!(t.absorb(&procs), vec![ROOT, 501], "stable on a second pass");
    }

    #[test]
    fn an_emptied_group_is_forgotten() {
        let mut t = Tree::new(ROOT, OWN);
        assert_eq!(t.absorb(&[p(ROOT, 90, ROOT, ROOT), p(501, ROOT, 501, ROOT)]), vec![ROOT, 501]);
        assert!(t.absorb(&[]).is_empty());
        // Pid 501 and group 501 now belong to an unrelated process.
        assert!(t
            .absorb(&[p(501, 1, 501, 600), p(777, 501, 501, 600)])
            .is_empty());
    }

    // ---- real processes -------------------------------------------------
    //
    // Every process below is spawned by the test itself. The tests send no
    // signal of their own (liveness is read with `ps`), and `kill_tree`
    // signals nothing outside the tree of the child handed to it.

    fn spawn_gate(dir: &Path, script: &str, env: &[(&str, &str)]) -> Child {
        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg(script)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        own_session(&mut cmd);
        cmd.spawn().unwrap()
    }

    /// Wait for the gate's descendant to record its pid in `dir/<name>`.
    fn recorded_pid(dir: &Path, name: &str) -> libc::pid_t {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(pid) = std::fs::read_to_string(dir.join(name))
                .ok()
                .and_then(|s| s.trim().parse::<libc::pid_t>().ok())
            {
                return pid;
            }
            assert!(Instant::now() < deadline, "{name} was never written");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Whether `pid` is still running. Read-only (`ps`), and a zombie counts
    /// as gone: it has exited, and whether anything reaps it is up to the
    /// host's init, not to the teardown.
    fn running(pid: libc::pid_t) -> bool {
        let out = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&out.stdout).trim().to_string();
        !stat.is_empty() && !stat.starts_with('Z')
    }

    /// `pid` is gone, allowing a moment for a just-signalled process to exit.
    fn assert_gone(pid: libc::pid_t, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while running(pid) {
            assert!(Instant::now() < deadline, "{what} (pid {pid}) outlived the teardown");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn pgid_of(pid: libc::pid_t) -> libc::pid_t {
        // SAFETY: plain syscall that only reads.
        unsafe { libc::getpgid(pid) }
    }

    fn sid_of(pid: libc::pid_t) -> libc::pid_t {
        // SAFETY: plain syscall that only reads.
        unsafe { libc::getsid(pid) }
    }

    const PERL_NEW_GROUP: &str = r#"perl -e 'setpgrp(0,0); open(F,">","pid.tmp"); print F $$; close F; rename("pid.tmp","pid"); sleep 300'"#;
    const PERL_NEW_SESSION: &str = r#"perl -MPOSIX -e 'POSIX::setsid(); open(F,">","pid.tmp"); print F $$; close F; rename("pid.tmp","pid"); sleep 300'"#;

    #[test]
    fn a_grandchild_in_another_process_group_is_gone() {
        let d = tempfile::tempdir().unwrap();
        let mut gate = spawn_gate(d.path(), &format!("{PERL_NEW_GROUP} & wait"), &[]);
        let root = libc::pid_t::try_from(gate.id()).unwrap();
        let pid = recorded_pid(d.path(), "pid");
        assert_eq!(pgid_of(pid), pid, "the grandchild leads its own group");
        assert_ne!(pgid_of(pid), root, "…which is not the gate's group");
        assert_eq!(kill_tree(&mut gate, TERM_GRACE), 0);
        assert_gone(pid, "grandchild in another process group");
    }

    #[test]
    fn a_grandchild_in_another_session_is_gone() {
        let d = tempfile::tempdir().unwrap();
        let mut gate = spawn_gate(d.path(), &format!("{PERL_NEW_SESSION} & wait"), &[]);
        let root = libc::pid_t::try_from(gate.id()).unwrap();
        let pid = recorded_pid(d.path(), "pid");
        assert_eq!(sid_of(pid), pid, "the grandchild leads its own session");
        assert_ne!(sid_of(pid), root);
        assert_eq!(kill_tree(&mut gate, TERM_GRACE), 0);
        assert_gone(pid, "grandchild in another session");
    }

    #[test]
    fn an_orphaned_grandchild_in_another_process_group_is_gone() {
        // The subshell that forks it exits at once, so by the time of the kill
        // it has no parent link to the gate and a group of its own.
        let d = tempfile::tempdir().unwrap();
        let mut gate = spawn_gate(d.path(), &format!("({PERL_NEW_GROUP} &); sleep 300"), &[]);
        let pid = recorded_pid(d.path(), "pid");
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(pgid_of(pid), pid);
        assert_eq!(kill_tree(&mut gate, TERM_GRACE), 0);
        assert_gone(pid, "orphaned grandchild");
    }

    #[test]
    fn a_grandchild_that_ignores_sigterm_is_killed_after_the_grace() {
        let d = tempfile::tempdir().unwrap();
        let script = r#"perl -e '$SIG{TERM}="IGNORE"; setpgrp(0,0); open(F,">","pid.tmp"); print F $$; close F; rename("pid.tmp","pid"); sleep 300' & wait"#;
        let mut gate = spawn_gate(d.path(), script, &[]);
        let pid = recorded_pid(d.path(), "pid");
        let t = Instant::now();
        assert_eq!(kill_tree(&mut gate, Duration::from_secs(1)), 0);
        assert!(t.elapsed() >= Duration::from_secs(1), "the grace was honoured");
        assert_gone(pid, "SIGTERM-ignoring grandchild");
    }

    #[test]
    fn the_exit_trap_runs_before_the_kill() {
        let d = tempfile::tempdir().unwrap();
        let script = "trap 'echo cleaned > trap-marker' EXIT; echo $$ > pid; sleep 300";
        let mut gate = spawn_gate(d.path(), script, &[]);
        recorded_pid(d.path(), "pid");
        assert_eq!(kill_tree(&mut gate, TERM_GRACE), 0);
        assert!(d.path().join("trap-marker").exists(), "the EXIT trap did not run");
    }

    /// `build-gate.sh`'s real stage runner: under GNU `timeout` (when it is
    /// installed) the stage sits in a process group of its own.
    fn bounded_run_stage_is_gone(force_portable: bool) {
        let lib =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts/lib/bounded-run.sh");
        assert!(lib.exists(), "{}", lib.display());
        let d = tempfile::tempdir().unwrap();
        let script = r#". "$BOUNDED_RUN_LIB"; bounded_run 300 sh -c 'echo $$ > pid.tmp; mv pid.tmp pid; sleep 300 & wait'"#;
        let lib = lib.display().to_string();
        let mut env = vec![("BOUNDED_RUN_LIB", lib.as_str())];
        if force_portable {
            env.push(("LOOM_FORCE_PORTABLE_TIMEOUT", "1"));
        }
        let mut gate = spawn_gate(d.path(), script, &env);
        let pid = recorded_pid(d.path(), "pid");
        assert_eq!(kill_tree(&mut gate, TERM_GRACE), 0);
        assert_gone(pid, "bounded_run stage");
    }

    #[test]
    fn a_bounded_run_stage_is_gone() {
        bounded_run_stage_is_gone(false);
    }

    #[test]
    fn a_bounded_run_stage_is_gone_on_the_portable_timeout_path() {
        bounded_run_stage_is_gone(true);
    }
}
