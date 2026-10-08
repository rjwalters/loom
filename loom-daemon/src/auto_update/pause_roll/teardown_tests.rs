//! Teardown tests (#10831). The planning tests use a scripted process table.
//! The tests that signal anything only signal processes they spawned.

use super::*;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;

fn t0() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-08T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn p(pid: u32, ppid: u32, pgid: u32, cmd: &str) -> Proc {
    Proc {
        pid,
        ppid,
        pgid,
        tty: None,
        started_at: Some(t0()),
        cmdline: cmd.to_string(),
    }
}

/// A record for pid/group `pid` whose process started at [`t0`].
fn spec(pid: u32) -> TreeSpec {
    TreeSpec {
        pid: Some(pid),
        pid_started_at: Some(t0()),
        recorded_started_at: Some(t0()),
        pgid: Some(pid),
        scope_unit: None,
    }
}

fn set(pids: &[u32]) -> HashSet<u32> {
    pids.iter().copied().collect()
}

/// An executable script at `dir/name`.
fn script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn ps_rows_parse_with_a_terminal_a_start_time_and_spaces_in_the_command() {
    let procs = parse_ps(
        "  10     1    10 Ss   ??       01-02:03:04 /bin/sh -c sleep 5\n bad line\n \
         11 10 10 S+ ttys003 00:07 sleep 5\n 12 10 10 Z ?? 00:01 (perl)\n 13 1 13 Ss ? 1:00:00 x\n",
        t0(),
    );
    assert_eq!(procs.len(), 3, "the zombie row and the bad row are dropped");
    assert_eq!(procs[0].cmdline, "/bin/sh -c sleep 5");
    assert_eq!(procs[0].tty, None, "`??` is no terminal");
    assert_eq!(
        procs[0].started_at,
        Some(t0() - chrono::Duration::seconds(86_400 + 2 * 3600 + 3 * 60 + 4))
    );
    assert_eq!((procs[1].pid, procs[1].ppid, procs[1].pgid), (11, 10, 10));
    assert_eq!(procs[1].tty.as_deref(), Some("ttys003"));
    assert_eq!(procs[1].started_at, Some(t0() - chrono::Duration::seconds(7)));
    assert_eq!(procs[2].tty, None, "`?` is no terminal either");
}

#[test]
fn elapsed_times_parse() {
    assert_eq!(parse_etime("00:07"), Some(7));
    assert_eq!(parse_etime("12:34:56"), Some(12 * 3600 + 34 * 60 + 56));
    assert_eq!(parse_etime("3-00:00:01"), Some(3 * 86_400 + 1));
    for bad in ["", "7", "a:b", "1:2:3:4"] {
        assert_eq!(parse_etime(bad), None, "{bad:?}");
    }
}

/// A pgid-only kill is not enough (`orphan_process_reaper.rs:16-35`): the
/// plan follows the ppid link across a `setsid`. Reach (a): a process is not
/// in the plan because it works in the item's worktree.
#[test]
fn the_plan_covers_the_group_and_a_setsid_child_but_not_worktree_bystanders() {
    let procs = vec![
        p(100, 50, 100, "spawn-claude.sh"),
        p(101, 100, 100, "claude -p"),
        p(102, 101, 102, "timeout 60 ngspice"), // new group
        p(103, 102, 103, "dev-server"),         // setsid'd, still a descendant
        p(200, 50, 200, "other agent"),
        // An operator shell and an editor in the item's worktree.
        p(400, 1, 400, "zsh --cwd /r/.loom/worktrees/issue-7"),
        p(500, 1, 500, "vim /r/.loom/worktrees/issue-7/src/main.rs"),
    ];
    let (plan, leader) = plan_tree(&spec(100), &procs, &HashSet::new(), None);
    assert_eq!(leader, Leader::Verified);
    assert_eq!(set(&plan), set(&[100, 101, 102, 103]));
    // Parent-first: the leader is frozen before its children.
    let pos = |pid| plan.iter().position(|x| *x == pid).unwrap();
    assert!(pos(100) < pos(101) && pos(101) < pos(102) && pos(102) < pos(103));
}

/// Reach (b): protected pids leave the seeds before descendants are expanded.
/// A corrupt record naming the daemon plans nothing, not the daemon's whole
/// subtree.
#[test]
fn a_record_naming_a_protected_pid_plans_nothing() {
    let procs = vec![
        p(1, 0, 1, "launchd"),
        p(50, 1, 50, "loom-daemon"),
        p(100, 50, 100, "agent a"),
        p(101, 100, 100, "claude"),
        p(200, 50, 200, "agent b"),
    ];
    let protected = protected_pids(&procs, 50, Some(50));
    assert!(protected.is_superset(&set(&[0, 1, 50])));
    for pid in [50, 1] {
        let (plan, _) = plan_tree(&spec(pid), &procs, &protected, None);
        assert!(plan.is_empty(), "pid {pid}: {plan:?}");
    }
}

/// Reach (b): the daemon's other children are protected, so a record whose
/// group is the daemon's own reaches only this agent's tree.
#[test]
fn the_daemons_other_children_are_never_planned() {
    let procs = vec![
        p(1, 0, 1, "launchd"),
        p(50, 1, 50, "loom-daemon"),
        p(60, 50, 50, "gh api (a daemon child in the daemon's group)"),
        p(61, 60, 50, "gh's helper"),
        p(100, 50, 100, "agent a"),
        p(101, 100, 100, "claude"),
        p(200, 50, 200, "agent b"),
        p(201, 200, 200, "codex"),
    ];
    let protected = protected_pids(&procs, 50, Some(100));
    assert!(protected.contains(&60) && protected.contains(&200) && !protected.contains(&100));

    // The right record: this agent only.
    let (plan, _) = plan_tree(&spec(100), &procs, &protected, None);
    assert_eq!(set(&plan), set(&[100, 101]));

    // A corrupt group (the daemon's): the group seed is refused outright, and
    // nothing below another daemon child is reached.
    let corrupt = TreeSpec {
        pgid: Some(50),
        ..spec(100)
    };
    let (plan, _) = plan_tree(&corrupt, &procs, &protected, None);
    assert_eq!(set(&plan), set(&[100, 101]));
}

/// Reach (c): a live pid whose start time is not the recorded one is a
/// recycled number. Neither it nor the group that now wears its number is
/// planned.
#[test]
fn a_recycled_pid_is_never_planned() {
    let mut stranger = p(100, 1, 100, "someone else's server");
    stranger.started_at = Some(t0() + chrono::Duration::seconds(600));
    let procs = vec![stranger, p(101, 100, 100, "its worker")];
    let (plan, leader) = plan_tree(&spec(100), &procs, &HashSet::new(), None);
    assert_eq!(leader, Leader::Stranger);
    assert!(plan.is_empty(), "{plan:?}");

    // Within the tolerance it is the same process.
    let mut same = p(100, 1, 100, "agent");
    same.started_at = Some(t0() + chrono::Duration::seconds(2));
    assert_eq!(leader_state(&spec(100), &[same]), Leader::Verified);

    // A row with no readable start time cannot be verified.
    let mut unknown = p(100, 1, 100, "agent?");
    unknown.started_at = None;
    assert_eq!(leader_state(&spec(100), &[unknown]), Leader::Stranger);

    // No observed start: the registry's start decides, by the #7935 rule.
    let recorded_only = TreeSpec {
        pid_started_at: None,
        ..spec(100)
    };
    let mut late = p(100, 1, 100, "x");
    late.started_at = Some(t0() + chrono::Duration::hours(2));
    assert_eq!(leader_state(&recorded_only, &[late]), Leader::Stranger);
    assert_eq!(leader_state(&recorded_only, &[p(100, 1, 100, "x")]), Leader::Verified);
    // No evidence at all: unverifiable.
    let none = TreeSpec {
        pid_started_at: None,
        recorded_started_at: None,
        ..spec(100)
    };
    assert_eq!(leader_state(&none, &[p(100, 1, 100, "x")]), Leader::Stranger);
}

/// A leader that already exited leaves its group reachable: the survivors
/// are the agent's own.
#[test]
fn a_dead_leader_still_lets_the_group_be_planned() {
    let procs = vec![p(101, 1, 100, "claude"), p(102, 101, 100, "tool")];
    let (plan, leader) = plan_tree(&spec(100), &procs, &HashSet::new(), None);
    assert_eq!(leader, Leader::Gone);
    assert_eq!(set(&plan), set(&[101, 102]));
}

/// Reach (b): a group member on a terminal that is not the daemon's, and not
/// below the agent, is an attended session. The agent's own descendants are
/// planned whatever terminal they hold.
#[test]
fn an_attended_session_is_never_planned() {
    let mut attended = p(300, 1, 100, "claude (an operator's session)");
    attended.tty = Some("ttys009".to_string());
    let mut own_pty = p(102, 101, 100, "script -q /dev/null make");
    own_pty.tty = Some("ttys020".to_string());
    let mut on_daemon_tty = p(103, 1, 100, "reparented tool");
    on_daemon_tty.tty = Some("ttys001".to_string());
    let procs = vec![
        p(100, 50, 100, "spawn"),
        p(101, 100, 100, "claude"),
        own_pty,
        on_daemon_tty,
        attended,
    ];
    let (plan, _) = plan_tree(&spec(100), &procs, &HashSet::new(), Some("ttys001"));
    assert_eq!(set(&plan), set(&[100, 101, 102, 103]));
}

/// A tree of this test's own: a group leader whose child has left the group
/// with `setsid()`.
struct OwnTree {
    child: std::process::Child,
    setsid_pid: u32,
}

impl OwnTree {
    fn spawn(dir: &std::path::Path) -> Self {
        let pidfile = dir.join("child.pid");
        // The child calls setsid() (new session AND group), records its pid
        // and sleeps; the parent sleeps too.
        let script = format!(
            "perl -e 'use POSIX; my $p = fork(); if ($p == 0) {{ POSIX::setsid(); \
             open(my $f, \">\", \"{}\"); print $f $$; close($f); sleep 300; exit 0 }} sleep 300'",
            pidfile.display()
        );
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&script).process_group(0);
        let child = cmd.spawn().unwrap();
        let setsid_pid = crate::sweep_registry::test_support::read_pid_file(&pidfile, 30_000)
            .expect("the setsid child never started");
        Self { child, setsid_pid }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The record H4 would hold for this tree.
    fn spec(&self) -> TreeSpec {
        let pid = self.pid();
        let started = observe_starts(&[pid]).get(&pid).copied();
        assert!(started.is_some(), "this test's own child must be in the process table");
        TreeSpec {
            pid: Some(pid),
            pid_started_at: started,
            recorded_started_at: Some(Utc::now()),
            pgid: Some(pid),
            scope_unit: None,
        }
    }
}

impl Drop for OwnTree {
    fn drop(&mut self) {
        send_group_signal(self.pid(), libc::SIGKILL);
        send_signal(self.setsid_pid, libc::SIGKILL);
        let _ = self.child.wait();
    }
}

/// The real thing: a tree whose child has left the process group with
/// `setsid` is entirely gone after the teardown, and a recorded scope unit
/// that does not exist falls back to the tree kill.
#[test]
fn a_real_tree_with_a_setsid_child_is_fully_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let mut tree = OwnTree::spawn(dir.path());
    let setsid_pid = tree.setsid_pid;
    assert!(crate::sweep_registry::is_pid_alive(setsid_pid));
    let spec = TreeSpec {
        scope_unit: Some("loom-agent-does-not-exist.scope".to_string()),
        ..tree.spec()
    };

    let report = teardown_tree(&spec, Duration::from_millis(300));

    let _ = tree.child.wait();
    assert!(!report.scope_stopped, "the recorded scope never existed");
    assert!(report.scope_note.is_some());
    assert!(report.reach_note.is_none(), "{report:?}");
    assert!(report.pids.contains(&setsid_pid), "{report:?}");
    assert!(report.survivors.is_empty(), "{report:?}");
    assert!(
        crate::sweep_registry::test_support::wait_until_dead(setsid_pid, 5_000),
        "the setsid'd child outlived the teardown"
    );
    assert!(!pid_running(tree.pid()));
}

/// Reach (c), for real: a record whose start time is not the live process's
/// signals nothing. The process (this test's own child) keeps running.
#[test]
fn a_real_process_wearing_a_recycled_pid_is_not_signalled() {
    let dir = tempfile::tempdir().unwrap();
    let tree = OwnTree::spawn(dir.path());
    let stale = TreeSpec {
        // The record is of a process that started an hour before this one.
        pid_started_at: Some(Utc::now() - chrono::Duration::hours(1)),
        ..tree.spec()
    };

    let report = teardown_tree(&stale, Duration::from_millis(100));

    assert!(report.pids.is_empty(), "{report:?}");
    assert!(report.reach_note.as_deref().unwrap().contains("recycled"), "{report:?}");
    assert!(pid_running(tree.pid()), "the stranger must not be signalled");
    assert!(crate::sweep_registry::is_pid_alive(tree.setsid_pid));
}

/// #10974 item 5: a process-table command that hangs is killed at its
/// timeout, and the teardown still ends: it falls back to the group
/// `SIGKILL`, which needs no external command.
#[test]
fn a_hanging_process_table_command_does_not_hang_the_teardown() {
    let dir = tempfile::tempdir().unwrap();
    let mut tree = OwnTree::spawn(dir.path());
    let spec = tree.spec();
    let source = ProcSource {
        ps: script(dir.path(), "hanging-ps", "exec sleep 300"),
        timeout: Duration::from_millis(300),
    };

    let started = Instant::now();
    assert!(source.table().is_none(), "a hung `ps` is no table");
    assert!(source.pid_running(tree.pid()), "cannot tell: the liveness probe decides");
    let report = teardown_tree_with(&spec, Duration::from_millis(100), &source);
    let took = started.elapsed();

    assert!(took < Duration::from_secs(10), "the hung `ps` blocked for {took:?}");
    let note = report
        .reach_note
        .expect("the report says the table was unavailable");
    assert!(note.contains("could not be read") && note.contains("SIGKILL"), "{note}");
    assert!(
        crate::sweep_registry::test_support::wait_for_condition(5_000, || {
            matches!(tree.child.try_wait(), Ok(Some(_)))
        }),
        "the group leader survived the fallback group SIGKILL"
    );
}

/// A table bigger than a pipe buffer is read whole: the wait drains stdout
/// instead of deadlocking against a `ps` blocked on a full pipe.
#[test]
fn a_large_process_table_is_drained_not_deadlocked() {
    let dir = tempfile::tempdir().unwrap();
    let big = script(
        dir.path(),
        "big-ps",
        "i=0; while [ $i -lt 6000 ]; do echo \"$((i + 10)) 1 $((i + 10)) S ?? 00:01 \
         /a/long/command/line/to/fill/the/pipe/buffer --flag value\"; i=$((i + 1)); done",
    );
    let source = ProcSource {
        ps: big,
        timeout: Duration::from_secs(20),
    };
    assert_eq!(source.table().expect("the table is read").len(), 6000);
}

#[test]
fn the_forced_group_kill_refuses_this_process_and_the_null_groups() {
    for pgid in [0, 1, std::process::id(), own_pgid()] {
        let spec = TreeSpec {
            pid: Some(pgid),
            pgid: Some(pgid),
            ..TreeSpec::default()
        };
        assert!(!force_kill_group(&spec), "group {pgid}");
    }
    assert!(!force_kill_group(&TreeSpec::default()));
}
