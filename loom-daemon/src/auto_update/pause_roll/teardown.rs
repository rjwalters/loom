//! Stopping one agent's whole process tree at H4 (issue #10831; design
//! `docs/design/daemon-roll-pause-resume.md` §5).
//!
//! "Nothing from an agent's tree may outlive H4." An agent's tree is more than
//! its process group: dev servers `setsid` or double-fork, and GNU `timeout`
//! moves its child to a new group (`orphan_process_reaper.rs`'s module doc has
//! the incident). So the teardown has three legs, and always runs the last two:
//!
//! 1. **systemd scope** (Linux, when a scope unit was recorded): the whole
//!    cgroup is stopped, `setsid` descendants included. A recorded unit is
//!    **not** proof the scope exists: `owner.json` names one for every Linux
//!    dispatch, including Codex and Claude launches that never created a
//!    scope. When the stop fails or the unit is not loaded, the legs below do
//!    the work (Judge finding on #10864). The stop never waits on systemd's
//!    stop job (#11051): a blocking `systemctl --user stop` waits out the
//!    scope's own stop timeout, and on the first live roll it timed out at
//!    20 s per item. [`ScopeCtl::stop`] queues the stop with `--no-block`
//!    (systemd sends `SIGTERM` to the cgroup), polls for the scope to empty
//!    for [`SCOPE_GRACE`], then sends `SIGKILL` to the cgroup itself.
//! 2. **Freeze-first tree kill** over the item's seeds, each expanded to its
//!    descendants through the ppid map, which survives `setsid` where the pgid
//!    does not. This reuses [`crate::orphan_process_reaper::reap_tree`]
//!    (`SIGSTOP` parent-first, re-snapshot, `SIGTERM` + `SIGCONT`, `SIGKILL`
//!    the survivors).
//! 3. **Process-group `SIGKILL`** for anything still in the group.
//!
//! # Reach (Judge finding on #10974)
//!
//! The teardown signals nothing outside the agent's own tree:
//!
//! - **Seeds are the recorded identity only**: the recorded pid, and the
//!   members of the recorded process group. A process is never a seed because
//!   its cwd or argv is in the item's worktree, so an operator shell or an
//!   attended session opened there is not touched. (A descendant that was
//!   reparented to pid 1 *and* left the group is reached by the scope leg on
//!   Linux, and otherwise by the orphan reaper's own, agent-shaped pass.)
//! - **Protected pids are removed before descendants are expanded**: this
//!   daemon, its ancestors, its other children, and anything on a foreign
//!   controlling terminal that is not below the agent. A record that names a
//!   protected pid therefore plans nothing, not "everything under it".
//! - **A pid is signalled only while it is still the recorded process**: the
//!   record carries the process's start time ([`TreeSpec::pid_started_at`],
//!   observed when H4 snapshots), and a live pid whose start time differs is a
//!   recycled number. It is not a seed, and its group number is not trusted
//!   either.
//!
//! # Bounded (Judge finding on #10974)
//!
//! The process table comes from `ps` (so the same code works under launchd and
//! systemd), run through [`run_bounded`]: a wedged `ps` is killed at
//! [`PS_TIMEOUT`] and the table is reported unavailable. Without a table there
//! is no tree to plan, so the teardown falls back to the guarded group
//! `SIGKILL` ([`force_kill_group`]), which needs no external command.
//!
//! **Session-exec (containerized) items.** The host side of a session-exec
//! invocation is part of the tree above. `spawn-codex.sh` traps `TERM` and
//! writes its `<stderr>.cancel` marker (`session_exec/host.rs`), so leg 2's
//! `SIGTERM` is what revokes the invocation: the in-container worker cancels
//! the invocation's tree and the session container keeps running. The grace
//! between `SIGTERM` and `SIGKILL` is what gives that trap time to run.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::orphan_process_reaper::{
    ancestors_of, children_map, descendants_of, order_parent_first, parent_map, reap_tree,
    OrphanTree, ProcEntry, ReapHooks,
};
use crate::sweep_registry::reaper::pid_identity;
use crate::sweep_registry::reaper::{group_has_members, send_group_signal, send_signal};

/// How long a stopped tree gets between `SIGTERM` and `SIGKILL`.
pub const TERM_GRACE: Duration = Duration::from_secs(3);
/// Bound on each `systemctl --user` call the scope leg makes.
const SCOPE_CMD_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a scope gets to empty after its stop is queued (systemd sends
/// `SIGTERM`) before the scope leg sends `SIGKILL` to the cgroup.
pub const SCOPE_GRACE: Duration = Duration::from_secs(5);
/// How long the scope leg waits for the scope to empty after its `SIGKILL`.
const SCOPE_KILL_WAIT: Duration = Duration::from_secs(1);
/// Bound on one `ps` call. A healthy `ps` answers in tens of milliseconds.
pub const PS_TIMEOUT: Duration = Duration::from_secs(10);
/// How far a pid's observed start time may drift between two `ps` readings
/// and still be the same process. `etime` has one-second resolution and each
/// reading is taken against a slightly later clock, later still on a loaded
/// host. The margin is safe to be generous with: the recorded process was
/// alive when its start was observed, so a stranger wearing its pid started
/// after that, and would have to have been handed a just-freed pid number
/// within this many seconds of the original's own start to pass.
const IDENTITY_TOLERANCE_SECS: i64 = 30;

/// What identifies one item's tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeSpec {
    /// The agent's tracked pid (the spawn script; it leads its group).
    pub pid: Option<u32>,
    /// When that process started, as the H4 snapshot observed it. With
    /// [`Self::pid`] it is the process's identity: a live pid with a different
    /// start time is a recycled number and is never signalled.
    pub pid_started_at: Option<DateTime<Utc>>,
    /// When the registry recorded the process as started. The fallback
    /// identity evidence when the snapshot could not observe a start time
    /// ([`pid_identity::pid_was_recycled`]'s rule).
    pub recorded_started_at: Option<DateTime<Utc>>,
    /// Its process group.
    pub pgid: Option<u32>,
    /// The systemd scope unit recorded at dispatch, if any. May not exist.
    pub scope_unit: Option<String>,
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
    /// Why nothing, or less than the whole tree, was signalled: the recorded
    /// pid is now another process, or the process table could not be read.
    pub reach_note: Option<String>,
    /// Wall time, milliseconds.
    pub elapsed_ms: u64,
}

/// One row of the process table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub pgid: u32,
    /// The controlling terminal, `None` when the process has none.
    pub tty: Option<String>,
    /// When the process started (the read time minus `etime`).
    pub started_at: Option<DateTime<Utc>>,
    pub cmdline: String,
}

/// Parse a `ps` elapsed time, `[[dd-]hh:]mm:ss`, into seconds.
#[must_use]
pub fn parse_etime(s: &str) -> Option<i64> {
    let (days, clock) = match s.split_once('-') {
        Some((d, rest)) => (d.parse::<i64>().ok()?, rest),
        None => (0, s),
    };
    let mut secs = 0i64;
    let parts: Vec<&str> = clock.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    for part in parts {
        secs = secs
            .checked_mul(60)?
            .checked_add(part.parse::<i64>().ok()?)?;
    }
    days.checked_mul(86_400)?.checked_add(secs)
}

/// Parse `ps -axo pid=,ppid=,pgid=,stat=,tty=,etime=,command=` output read at
/// `now`. Zombies (`stat` starting with `Z`) are dropped: a killed process
/// stays in the table until its parent reaps it, and it is not running.
#[must_use]
pub fn parse_ps(out: &str, now: DateTime<Utc>) -> Vec<Proc> {
    out.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let pgid = it.next()?.parse().ok()?;
            if it.next()?.starts_with('Z') {
                return None;
            }
            let tty = it.next()?;
            let started_at = parse_etime(it.next()?)
                .and_then(|secs| now.checked_sub_signed(chrono::Duration::seconds(secs)));
            let cmdline = it.collect::<Vec<_>>().join(" ");
            Some(Proc {
                pid,
                ppid,
                pgid,
                // macOS prints `??`, Linux `?`, for "no controlling terminal".
                tty: (!tty.starts_with('?') && tty != "-").then(|| tty.to_string()),
                started_at,
                cmdline,
            })
        })
        .collect()
}

/// Run `cmd` to completion or kill it at `timeout`. `Some(stdout)` when it
/// exited (whatever its status), `None` when it could not be spawned or was
/// killed for running long.
///
/// Stdout is drained on its own thread while the child runs. A process table
/// is larger than a pipe buffer, so a wait that only polls the exit status
/// would deadlock against a `ps` blocked on a full pipe.
#[must_use]
pub fn run_bounded(mut cmd: Command, timeout: Duration) -> Option<(bool, Vec<u8>)> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // The child is gone, so the pipe closes and the reader ends.
                // The short wait only covers a grandchild holding the pipe.
                let out = rx.recv_timeout(Duration::from_secs(2)).ok()?;
                return Some((status.success(), out));
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Where the process table comes from. Production uses `ps` on `PATH` with
/// [`PS_TIMEOUT`]; tests point it at a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcSource {
    pub ps: PathBuf,
    pub timeout: Duration,
}

impl ProcSource {
    /// The system `ps`.
    #[must_use]
    pub fn system() -> Self {
        Self {
            ps: PathBuf::from("ps"),
            timeout: PS_TIMEOUT,
        }
    }

    /// The live process table, or `None` when `ps` could not be run or did
    /// not answer within the timeout.
    #[must_use]
    pub fn table(&self) -> Option<Vec<Proc>> {
        let mut cmd = Command::new(&self.ps);
        cmd.args(["-axo", "pid=,ppid=,pgid=,stat=,tty=,etime=,command="]);
        let (_, out) = run_bounded(cmd, self.timeout)?;
        let procs = parse_ps(&String::from_utf8_lossy(&out), Utc::now());
        // An empty table is a failed read: this process is always in it.
        (!procs.is_empty()).then_some(procs)
    }

    /// Whether `pid` is a running process: alive and not a zombie. A child
    /// that exited but has not been reaped still answers `kill(pid, 0)`; H4
    /// holds the sweep reaper off its items, so that is exactly the state an
    /// agent that ended by itself is left in. A `ps` that cannot answer leaves
    /// the liveness probe's verdict (alive).
    #[must_use]
    pub fn pid_running(&self, pid: u32) -> bool {
        if !crate::sweep_registry::is_pid_alive(pid) {
            return false;
        }
        let mut cmd = Command::new(&self.ps);
        cmd.args(["-o", "stat=", "-p", &pid.to_string()]);
        let Some((ok, out)) = run_bounded(cmd, self.timeout) else {
            return true; // cannot tell: the liveness probe said alive
        };
        let stat = String::from_utf8_lossy(&out);
        let stat = stat.trim();
        !(stat.is_empty() && !ok) && !stat.starts_with('Z')
    }
}

/// The live process table from the system `ps`.
#[must_use]
pub fn process_table() -> Option<Vec<Proc>> {
    ProcSource::system().table()
}

/// [`ProcSource::pid_running`] against the system `ps`.
#[must_use]
pub fn pid_running(pid: u32) -> bool {
    ProcSource::system().pid_running(pid)
}

fn as_entries(procs: &[Proc]) -> Vec<ProcEntry> {
    procs
        .iter()
        .map(|p| ProcEntry {
            pid: p.pid,
            ppid: p.ppid,
            cwd: None,
            cmdline: p.cmdline.clone(),
            age_secs: None,
        })
        .collect()
}

/// What the table says about the recorded pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leader {
    /// The pid is live and is the recorded process.
    Verified,
    /// No live process has the pid (it exited, or is a zombie).
    Gone,
    /// A live process has the pid, but it is not the recorded process (its
    /// start time differs), or there is no start time to check it against.
    Stranger,
}

/// Whether `spec`'s recorded pid is still the recorded process in `procs`.
///
/// The recorded start time ([`TreeSpec::pid_started_at`]) must match the
/// row's within [`IDENTITY_TOLERANCE_SECS`]. When the snapshot observed none,
/// the registry's start time decides by [`pid_identity::pid_was_recycled`]'s
/// rule. With no evidence at all, or a row with no readable start time, the
/// pid is unverifiable and is treated as a stranger: an unknown pid is never
/// signalled.
#[must_use]
pub fn leader_state(spec: &TreeSpec, procs: &[Proc]) -> Leader {
    let Some(row) = spec.pid.and_then(|pid| procs.iter().find(|p| p.pid == pid)) else {
        return Leader::Gone;
    };
    let same = match (row.started_at, spec.pid_started_at, spec.recorded_started_at) {
        (Some(now), Some(then), _) => (now - then).num_seconds().abs() <= IDENTITY_TOLERANCE_SECS,
        (Some(now), None, Some(recorded)) => !pid_identity::pid_was_recycled(Some(now), recorded),
        _ => false,
    };
    if same {
        Leader::Verified
    } else {
        Leader::Stranger
    }
}

/// The pids the teardown must never signal: this daemon (`me`), its
/// ancestors, its other children (`leader` is the one child that is this
/// agent), and pids 0 and 1.
#[must_use]
pub fn protected_pids(procs: &[Proc], me: u32, leader: Option<u32>) -> HashSet<u32> {
    let parents = parent_map(&as_entries(procs));
    let mut set = ancestors_of(me, &parents);
    set.insert(me);
    set.extend([0, 1]);
    set.extend(
        procs
            .iter()
            .filter(|p| p.ppid == me && Some(p.pid) != leader)
            .map(|p| p.pid),
    );
    set
}

/// The pids of `spec`'s tree in `procs`, parent-first, and what the table
/// says about the recorded pid.
///
/// Seeds are the recorded pid (only while it is [`Leader::Verified`]) and the
/// members of the recorded group (unless the recorded pid is now a
/// [`Leader::Stranger`], which means the group number was recycled with it).
/// `protected` pids are dropped from the seeds **before** descendants are
/// expanded, and the expansion does not pass through one. A group member on a
/// controlling terminal other than `daemon_tty`'s that is not below the
/// recorded pid is an attended session, not part of the tree, and is dropped
/// with them.
#[must_use]
pub fn plan_tree(
    spec: &TreeSpec,
    procs: &[Proc],
    protected: &HashSet<u32>,
    daemon_tty: Option<&str>,
) -> (Vec<u32>, Leader) {
    let leader = leader_state(spec, procs);
    let entries = as_entries(procs);
    let mut children = children_map(&entries);
    // Never descend through a protected pid.
    children.retain(|parent, _| !protected.contains(parent));
    for kids in children.values_mut() {
        kids.retain(|pid| !protected.contains(pid));
    }

    let by_pid: Vec<u32> = match (spec.pid, leader) {
        (Some(pid), Leader::Verified) if !protected.contains(&pid) => vec![pid],
        _ => Vec::new(),
    };
    let below_leader: HashSet<u32> = descendants_of(&by_pid, &children).into_iter().collect();
    let group = spec
        .pgid
        .filter(|g| *g > 1 && !protected.contains(g) && leader != Leader::Stranger)
        // A group id other than the recorded pid has no identity to check.
        .filter(|g| Some(*g) == spec.pid || !procs.iter().any(|p| p.pid == *g));
    let mut seeds = by_pid.clone();
    for p in procs {
        let in_group = group.is_some_and(|g| p.pgid == g);
        let attended = p.tty.as_deref().is_some_and(|t| Some(t) != daemon_tty)
            && !below_leader.contains(&p.pid)
            && !by_pid.contains(&p.pid);
        if in_group && !protected.contains(&p.pid) && !attended {
            seeds.push(p.pid);
        }
    }
    let mut all: HashSet<u32> = seeds.iter().copied().collect();
    all.extend(descendants_of(&seeds, &children));
    all.retain(|pid| *pid > 1 && !protected.contains(pid));
    (order_parent_first(&all, &parent_map(&entries)), leader)
}

/// How the scope leg reaches systemd. Production uses `systemctl` on `PATH`,
/// on Linux only ([`Self::system`]); tests point it at a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeCtl {
    pub systemctl: PathBuf,
    /// Bound on each `systemctl` call.
    pub timeout: Duration,
    /// How long the scope gets to empty after its stop is queued.
    pub grace: Duration,
    /// How long it gets to empty after the `SIGKILL`.
    pub kill_wait: Duration,
    /// How often the scope's state is polled.
    pub poll: Duration,
}

impl ScopeCtl {
    /// The system `systemctl`, or `None` where there is no systemd.
    #[must_use]
    pub fn system() -> Option<Self> {
        cfg!(target_os = "linux").then(|| Self {
            systemctl: PathBuf::from("systemctl"),
            timeout: SCOPE_CMD_TIMEOUT,
            grace: SCOPE_GRACE,
            kill_wait: SCOPE_KILL_WAIT,
            poll: Duration::from_millis(100),
        })
    }

    /// Run `systemctl --user <args>`, bounded by `timeout`. `Some(ok, stdout)`
    /// when it exited, `None` when it could not run or was killed.
    fn run(&self, args: &[&str], timeout: Duration) -> Option<(bool, String)> {
        let mut cmd = Command::new(&self.systemctl);
        cmd.arg("--user").args(args);
        run_bounded(cmd, timeout)
            .map(|(ok, out)| (ok, String::from_utf8_lossy(&out).trim().to_string()))
    }

    fn show(&self, unit: &str, property: &str, timeout: Duration) -> Option<String> {
        let prop = format!("--property={property}");
        match self.run(&["show", unit, &prop, "--value"], timeout) {
            Some((true, value)) => Some(value),
            _ => None,
        }
    }

    /// Whether the scope has emptied: inactive or failed. A scope that has
    /// been garbage-collected reads `inactive` too.
    fn wait_empty(&self, unit: &str, within: Duration) -> bool {
        let until = Instant::now() + within;
        loop {
            // Each probe is bounded by what is left of the wait, so a wedged
            // `systemctl` cannot stretch it.
            let left = until
                .saturating_duration_since(Instant::now())
                .min(self.timeout)
                .max(Duration::from_millis(200));
            let state = self.show(unit, "ActiveState", left);
            if matches!(state.as_deref(), Some("inactive" | "failed")) {
                return true;
            }
            if Instant::now() >= until {
                return false;
            }
            std::thread::sleep(self.poll);
        }
    }

    /// Leg 1: stop the recorded scope if it really exists, without waiting on
    /// systemd's stop job. `Ok(())` when the scope emptied, `Err(why)` when
    /// the caller must rely on the fallback legs. Bounded: about
    /// `3 * timeout + grace + kill_wait` with every call wedged, and
    /// `grace + kill_wait` with a scope that ignores `SIGTERM`.
    ///
    /// # Errors
    /// When the unit is not loaded, or did not empty in time.
    pub fn stop(&self, unit: &str) -> Result<(), String> {
        if self.show(unit, "LoadState", self.timeout).as_deref() != Some("loaded") {
            return Err(format!("scope unit {unit} is not loaded (it was never created)"));
        }
        // systemd sends SIGTERM to every process in the cgroup. `--no-block`
        // returns once the job is queued.
        let queued =
            matches!(self.run(&["stop", "--no-block", unit], self.timeout), Some((true, _)));
        if queued && self.wait_empty(unit, self.grace) {
            return Ok(());
        }
        // The grace is over: SIGKILL the whole cgroup ourselves.
        let killed =
            matches!(self.run(&["kill", "--signal=SIGKILL", unit], self.timeout), Some((true, _)));
        if killed && self.wait_empty(unit, self.kill_wait) {
            return Ok(());
        }
        Err(format!(
            "scope unit {unit} did not empty within {}ms of its stop (stop {}, SIGKILL {})",
            (self.grace + self.kill_wait).as_millis(),
            if queued { "queued" } else { "failed" },
            if killed { "sent" } else { "failed" }
        ))
    }
}

/// This process's own process group.
fn own_pgid() -> u32 {
    // SAFETY: `getpgrp` takes no arguments, cannot fail and touches no memory.
    u32::try_from(unsafe { libc::getpgrp() }).unwrap_or(0)
}

/// `SIGKILL` `spec`'s process group without reading the process table: the
/// last resort when `ps` cannot answer, and what the H4 deadline uses to
/// finish a pause whose teardown is stuck. Runs no external command, so it
/// cannot hang.
///
/// Guarded as far as that allows: never group 0 or 1, never this daemon's own
/// group, and (where a start time is derivable without `ps`, i.e. Linux) never
/// a group whose leader pid now belongs to a process started well after the
/// recorded one. Returns `true` when the signal was sent.
pub fn force_kill_group(spec: &TreeSpec) -> bool {
    let Some(pgid) = spec.pgid.or(spec.pid).filter(|g| *g > 1) else {
        return false;
    };
    if pgid == own_pgid() || pgid == std::process::id() {
        return false;
    }
    let recorded = spec.pid_started_at.or(spec.recorded_started_at);
    if let Some(recorded) = recorded {
        if crate::live_claim::pid_is_live_process(pgid)
            && pid_identity::pid_was_recycled(pid_identity::pid_start_wallclock(pgid), recorded)
        {
            return false;
        }
    }
    group_has_members(pgid) && send_group_signal(pgid, libc::SIGKILL)
}

/// Stop `spec`'s whole tree. Never fails: every leg is best-effort and the
/// report says what is left.
#[must_use]
pub fn teardown_tree(spec: &TreeSpec, grace: Duration) -> TeardownReport {
    teardown_tree_with(spec, grace, &ProcSource::system())
}

/// [`teardown_tree`] with an explicit process-table source.
#[must_use]
pub fn teardown_tree_with(spec: &TreeSpec, grace: Duration, source: &ProcSource) -> TeardownReport {
    teardown_tree_full(spec, grace, source, ScopeCtl::system().as_ref())
}

/// [`teardown_tree_with`] with an explicit scope controller (`None`: no
/// systemd, so the scope leg does not apply).
#[must_use]
pub fn teardown_tree_full(
    spec: &TreeSpec,
    grace: Duration,
    source: &ProcSource,
    scope: Option<&ScopeCtl>,
) -> TeardownReport {
    let started = Instant::now();
    let mut report = TeardownReport::default();
    let finish = |mut report: TeardownReport| {
        report.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        report
    };

    if let Some(unit) = spec.scope_unit.as_deref() {
        let stopped = scope
            .map_or_else(|| Err("no systemd on this platform".to_string()), |ctl| ctl.stop(unit));
        match stopped {
            Ok(()) => report.scope_stopped = true,
            Err(why) => report.scope_note = Some(why),
        }
    }

    let Some(procs) = source.table() else {
        // No table, so no tree and no identity check: the group is all that
        // can still be reached.
        let killed = force_kill_group(spec);
        report.reach_note = Some(format!(
            "the process table could not be read within {}s; {}",
            source.timeout.as_secs(),
            if killed {
                "sent SIGKILL to the process group only"
            } else {
                "nothing was signalled"
            }
        ));
        return finish(report);
    };
    let me = std::process::id();
    let protected = protected_pids(&procs, me, spec.pid);
    let daemon_tty = procs
        .iter()
        .find(|p| p.pid == me)
        .and_then(|p| p.tty.clone());
    let (pids, leader) = plan_tree(spec, &procs, &protected, daemon_tty.as_deref());
    if leader == Leader::Stranger {
        report.reach_note = Some(format!(
            "pid {:?} is no longer the recorded process (its start time differs or cannot be \
             checked): the pid number was recycled, so neither it nor its group was signalled",
            spec.pid
        ));
    }
    if !pids.is_empty() {
        let tree = OrphanTree {
            issue: 0,
            worktree: PathBuf::new(),
            seeds: pids.clone(),
            pids: pids.clone(),
            details: Vec::new(),
            protected_pids: Vec::new(),
        };
        // The post-freeze re-snapshot must come from the same table source, so
        // a child forked between the scan and the freeze is caught on macOS
        // too (the orphan reaper's own snapshot is `/proc`-only).
        let snapshot = || as_entries(&source.table().unwrap_or_default());
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
            let live: HashSet<u32> = source
                .table()
                .unwrap_or_default()
                .iter()
                .map(|p| p.pid)
                .collect();
            report.survivors = outcome
                .survivors
                .into_iter()
                .filter(|pid| live.contains(pid))
                .collect();
        }
    }

    // Leg 3: whatever is still in the group, unless the group number went
    // with a recycled pid.
    if leader != Leader::Stranger {
        if let Some(pgid) = spec
            .pgid
            .filter(|g| *g > 1 && !protected.contains(g) && *g != own_pgid())
        {
            if group_has_members(pgid) {
                send_group_signal(pgid, libc::SIGKILL);
            }
        }
    }
    finish(report)
}

/// The start time of each of `pids` in one table read, for the identity a
/// [`TreeSpec`] carries. Empty when the table cannot be read.
#[must_use]
pub fn observe_starts(pids: &[u32]) -> HashMap<u32, DateTime<Utc>> {
    let Some(procs) = process_table() else {
        return HashMap::new();
    };
    procs
        .iter()
        .filter(|p| pids.contains(&p.pid))
        .filter_map(|p| Some((p.pid, p.started_at?)))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "teardown_tests.rs"]
mod tests;
