//! Identity-paired PID liveness for registry entries the reaper cannot
//! `try_wait()` (Issue #7935) — the Rust counterpart of the shell-side fix in
//! `defaults/scripts/sweep-lease-renew.sh` (#7825 / PR #7934).
//!
//! # The bug this closes
//!
//! [`SweepRegistry::poll_liveness`](super::SweepRegistry::poll_liveness) prefers
//! the retained `Child` handle, whose `try_wait()` is exact. Entries with **no**
//! handle — a [`reconstruct`](crate::sweep_registry::SweepRegistry::reconstruct)-ed
//! lock survivor, an
//! [`adopt_live_journal_sweeps`](crate::sweep_registry::SweepRegistry::adopt_live_journal_sweeps)
//! journal survivor — fall back to
//! [`is_pid_alive`](crate::sweep_registry::is_pid_alive), a bare `kill(pid, 0)`
//! probe that knows nothing about *which* process currently wears that pid
//! number.
//!
//! A bare pid is not a durable handle on a process. The dangerous window is not
//! the 30s between two reaper ticks — it is the arbitrarily long stretch while
//! **no daemon is running at all** (a crash, a restart, an `auto_update` roll,
//! an operator-stopped host). A sweep leader that dies during that stretch can
//! easily have its pid number recycled onto an unrelated process before the next
//! daemon starts; `reconstruct()` then probes the pid, sees "alive", and admits
//! a phantom `SweepState::Running` entry. From that moment:
//!
//! - the entry never transitions terminal, because the stranger process outlives
//!   every probe;
//! - [`count_in_flight_sweeps`](crate::ipc) counts it forever, so
//!   `loom-daemon restart --drain` never drains and every subsequent
//!   `auto_update` roll on the host is blocked;
//! - [`reap_orphaned_group`](super::SweepRegistry::reap_orphaned_group) never
//!   fires for the real, dead leader — it only ever fires at the terminal
//!   transition that now never happens.
//!
//! # The fix
//!
//! Pair the pid with the process's **start time** before believing it, exactly
//! as `pid_start_identity()` does on the shell side. The registry already
//! records the one timestamp needed to make that comparison without any schema
//! change: [`SweepInfo::started_at`](crate::types::SweepInfo::started_at).
//!
//! Every tracked process starts *before* — or within seconds of — its entry's
//! `started_at`:
//!
//! - a live dispatch records `started_at: Utc::now()` **after** the fork
//!   returned (`finish_issue_dispatch`), so the child is strictly older than
//!   its entry;
//! - a reconstructed entry inherits `owner.acquired_at`, written by
//!   `acquire_lock` **immediately before** the spawn, so the child is younger
//!   than its entry by the spawn latency alone (stagger wait + exec + token
//!   capture — seconds, and bounded by [`RECYCLED_PID_SKEW_SECS`] with three
//!   orders of magnitude to spare);
//! - a journal-adopted entry inherits the sweep's own recorded start time.
//!
//! So a live process whose start time is *far later* than its entry's
//! `started_at` cannot be the process that entry is tracking. It is a stranger
//! wearing a recycled pid number, and the entry it is keeping alive is a
//! phantom.
//!
//! # Fail-safe direction (#4691)
//!
//! Every consumer of "dead" uses it to authorize destruction (label restore,
//! lock release, group signalling), so an *unknown* answer must resolve to
//! **alive**, never to dead. An unreadable `/proc`, an unparseable `stat`, a
//! host with no `/proc` at all: all of them return "no opinion" here and leave
//! the pre-#7935 verdict untouched. The check can only ever turn an
//! `is_pid_alive`-alive verdict into dead when it holds *positive* evidence
//! that the pid belongs to a younger process.
//!
//! # Platform support
//!
//! The start-time probe is Linux-only, matching the stance
//! [`crate::orphan_process_reaper`] already takes for its own `/proc` walk (and
//! reusing that module's `stat` parser). On a non-Linux host
//! [`pid_start_wallclock`] returns `None`, so behavior is byte-for-byte
//! pre-#7935. `ps -o lstart=` (what the shell-side fix uses off Linux) is
//! deliberately **not** parsed here: its output is local-time and
//! locale-shaped, and a timezone misparse would push a *healthy* sweep past the
//! skew window and reap it — the one failure mode this module must not have.

use chrono::{DateTime, Duration as ChronoDuration, Utc};

/// How far *after* an entry's `started_at` a tracked process may have started
/// and still be believed to be that entry's own process.
///
/// Sized for the worst legitimate gap on the widest path (a reconstructed
/// entry, whose `started_at` is the lock's `acquired_at`, written before the
/// dispatch stagger wait, the spawn, and the token-name capture poll) with a
/// very large margin: that gap is seconds in practice. It is deliberately NOT
/// tight — the failure this catches is a pid recycled across a daemon-down
/// window measured in hours or days, so a 15-minute skew costs nothing in
/// detection power while making a false "dead" verdict on a healthy sweep
/// essentially impossible.
pub(crate) const RECYCLED_PID_SKEW_SECS: i64 = 15 * 60;

/// Identity-paired liveness for a tracked pid whose entry's `started_at` is
/// known: is `pid` alive **and** still the same process the entry recorded?
///
/// Returns `false` (dead) when the pid is gone, when it is a zombie (exited,
/// awaiting reaping — it holds nothing and does nothing, the same stance
/// [`crate::live_claim::pid_is_live_process`] takes), or when its start time
/// proves it is a younger process wearing a recycled pid number.
pub(crate) fn pid_alive_since(pid: u32, entry_started_at: DateTime<Utc>) -> bool {
    if !crate::live_claim::pid_is_live_process(pid) {
        return false;
    }
    let proc_start = pid_start_wallclock(pid);
    if !pid_was_recycled(proc_start, entry_started_at) {
        return true;
    }
    log::warn!(
        "pid_identity: pid {pid} is alive but started at {} — later than the {}s skew window \
         after its registry entry's started_at ({}). The tracked process is gone and this pid \
         number has been RECYCLED onto an unrelated process; reporting it DEAD so the entry can \
         transition terminal instead of blocking `restart --drain` forever (#7935).",
        proc_start.map_or_else(|| "?".to_string(), |t| t.to_rfc3339()),
        RECYCLED_PID_SKEW_SECS,
        entry_started_at.to_rfc3339(),
    );
    false
}

/// [`pid_alive_since`] for callers whose entry may have no known `started_at`
/// (`None` ⇒ the pre-#7935 liveness probe, unchanged).
pub(crate) fn tracked_pid_alive(pid: u32, entry_started_at: Option<DateTime<Utc>>) -> bool {
    entry_started_at.map_or_else(
        || crate::live_claim::pid_is_live_process(pid),
        |started_at| pid_alive_since(pid, started_at),
    )
}

/// [`pid_alive_since`] for a claim lock's `owner.json` (Issue #7935), whose
/// `acquired_at` is an RFC3339 string stamped by `acquire_lock` immediately
/// **before** the sweep child is spawned — so the child it records is at most
/// one spawn's worth younger than it.
///
/// This is the *admission* gate: `reconstruct()` re-admits a lock whose
/// `owner_pid` is alive as a `Running` entry, and a recycled pid number is
/// exactly how a phantom entry gets born in the first place (the daemon-down
/// window is unbounded — a crash, a restart, an `auto_update` roll). An
/// unparseable `acquired_at` is "no opinion": the probe degrades to the bare
/// liveness check rather than guessing, mirroring how the caller degrades the
/// same field to `Utc::now()`.
pub(crate) fn owner_pid_alive_since(pid: u32, acquired_at: &str) -> bool {
    let started_at = DateTime::parse_from_rfc3339(acquired_at)
        .ok()
        .map(|t| t.with_timezone(&Utc));
    tracked_pid_alive(pid, started_at)
}

/// Pure decision core: does `proc_start` prove the pid was recycled?
///
/// `None` (start time not derivable on this host / for this pid) is "no
/// opinion" and always answers `false` — see the module doc's fail-safe note.
/// A process that started *before* its entry, or within
/// [`RECYCLED_PID_SKEW_SECS`] after it, is the entry's own process.
pub(crate) fn pid_was_recycled(
    proc_start: Option<DateTime<Utc>>,
    entry_started_at: DateTime<Utc>,
) -> bool {
    let Some(proc_start) = proc_start else {
        return false;
    };
    proc_start > entry_started_at + ChronoDuration::seconds(RECYCLED_PID_SKEW_SECS)
}

/// Does a **live** process currently wear `pgid` as its own pid — i.e. has the
/// recorded group id been recycled onto a stranger (Issue #7935)?
///
/// [`reap_orphaned_group`](super::SweepRegistry::reap_orphaned_group) is the
/// crash path: it runs only for a sweep whose **leader is already dead**, and
/// the `pgid` it is handed is always that leader's own pid — `spawned_leader_pgid`
/// records `Some(pid)` exclusively when `getpgid(pid) == pid`, and `None`
/// otherwise. So the group id and the dead leader's pid are the same number.
///
/// A pid number cannot be handed to a new process while it is still in use as a
/// process group id with live members: the kernel frees a `struct pid` only once
/// no task references it under *any* type, `PIDTYPE_PGID` included (every
/// process-group member holds such a reference). Therefore a live, non-zombie
/// process whose pid equals `pgid` proves two things at once — the sweep's group
/// drained completely (otherwise the number could never have been reallocated),
/// and the group answering to that number *today* belongs to an unrelated
/// process tree.
///
/// Signalling it would SIGTERM-then-SIGKILL an innocent bystander. This is not
/// hypothetical once the identity pairing above starts reporting recycled pids
/// as dead: that verdict is precisely what routes a recycled number into the
/// crash-path group reap, so the two changes must land together.
///
/// A **zombie** leader answers `false` (not recycled) on purpose: a zombie is
/// still the original process, its pid is not reusable, and its group may well
/// still hold the live survivors #4980 exists to kill.
pub(crate) fn pgid_number_was_recycled(pgid: u32) -> bool {
    crate::live_claim::pid_is_live_process(pgid)
}

/// Wall-clock instant at which `pid` started, when derivable.
///
/// Linux: `/proc/<pid>/stat` field 22 (`starttime`, clock ticks since boot,
/// parsed by [`crate::orphan_process_reaper::parse_stat`] so the `comm`-with-
/// parentheses trap is handled in exactly one place) converted against
/// `/proc/stat`'s `btime` (boot time, epoch seconds). Both reads are cheap and
/// world-readable; any failure returns `None`.
#[cfg(target_os = "linux")]
pub(crate) fn pid_start_wallclock(pid: u32) -> Option<DateTime<Utc>> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_ppid, start_ticks) = crate::orphan_process_reaper::parse_stat(&stat)?;
    let hz = crate::orphan_process_reaper::clock_ticks_per_sec();
    if hz <= 0.0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let since_boot_ms = ((start_ticks as f64 / hz) * 1000.0) as i64;
    boot_time()?.checked_add_signed(ChronoDuration::milliseconds(since_boot_ms))
}

/// Host boot time from `/proc/stat`'s `btime` line (epoch seconds).
#[cfg(target_os = "linux")]
fn boot_time() -> Option<DateTime<Utc>> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let btime: i64 = stat
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    DateTime::from_timestamp(btime, 0)
}

/// No portable start-time probe off Linux — see the module doc's platform note.
/// `None` keeps the pre-#7935 verdict exactly as it was.
#[cfg(not(target_os = "linux"))]
pub(crate) fn pid_start_wallclock(_pid: u32) -> Option<DateTime<Utc>> {
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::sweep_registry::test_support::{
        fixture_registry, insert_running_with_pid_at, wait_until_alive, FIXTURE_CHILD_WAIT_MS,
    };
    use tempfile::tempdir;

    /// Spawn a harmless long-lived process that no registry owns a `Child`
    /// handle for — the stand-in for "some unrelated process now wears the
    /// pid number our dead sweep leader used to have".
    fn spawn_stand_in() -> std::process::Child {
        let child = std::process::Command::new("sleep")
            .arg("60")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn stand-in process");
        assert!(
            wait_until_alive(child.id(), FIXTURE_CHILD_WAIT_MS),
            "the stand-in process must come up"
        );
        child
    }

    fn non_terminal(registry: &crate::sweep_registry::SweepRegistry) -> usize {
        registry
            .list(None)
            .into_iter()
            .filter(|info| !info.state.is_terminal())
            .count()
    }

    // ---- pure decision core ------------------------------------------------

    /// The fail-safe direction (#4691): an underivable start time must never
    /// be able to turn an alive pid into a dead one.
    #[test]
    fn unknown_start_time_is_never_recycled() {
        assert!(!pid_was_recycled(None, Utc::now() - ChronoDuration::days(30)));
    }

    #[test]
    fn a_process_older_than_its_entry_is_not_recycled() {
        let started_at = Utc::now();
        // The ordinary live-dispatch shape: the child forked BEFORE the entry
        // recorded `started_at`.
        assert!(!pid_was_recycled(Some(started_at - ChronoDuration::seconds(5)), started_at));
    }

    #[test]
    fn a_process_inside_the_skew_window_is_not_recycled() {
        let started_at = Utc::now();
        // The reconstruct shape: `acquired_at` is stamped just BEFORE the
        // spawn, so the child is legitimately a little younger than its entry.
        assert!(!pid_was_recycled(
            Some(started_at + ChronoDuration::seconds(RECYCLED_PID_SKEW_SECS - 1)),
            started_at
        ));
        // Exactly at the boundary is still believed (the comparison is strict).
        assert!(!pid_was_recycled(
            Some(started_at + ChronoDuration::seconds(RECYCLED_PID_SKEW_SECS)),
            started_at
        ));
    }

    /// The #7935 shape: the entry is hours old, the process wearing its pid
    /// started minutes ago. That process cannot be the one the entry tracks.
    #[test]
    fn a_process_younger_than_the_skew_window_is_recycled() {
        let started_at = Utc::now() - ChronoDuration::hours(6);
        assert!(pid_was_recycled(
            Some(started_at + ChronoDuration::seconds(RECYCLED_PID_SKEW_SECS + 1)),
            started_at
        ));
        assert!(pid_was_recycled(Some(Utc::now()), started_at));
    }

    // ---- the /proc-backed probe -------------------------------------------

    #[cfg(target_os = "linux")]
    #[test]
    fn own_process_start_time_is_in_the_past_and_plausible() {
        let start = pid_start_wallclock(std::process::id())
            .expect("this process's own start time must be derivable on Linux");
        let now = Utc::now();
        assert!(start <= now, "a process cannot have started in the future");
        assert!(
            start > now - ChronoDuration::days(365),
            "start time {start} is implausibly far in the past — btime/HZ conversion is wrong"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_freshly_spawned_child_reads_as_started_just_now() {
        let before = Utc::now() - ChronoDuration::seconds(5);
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn fixture child");
        let start = pid_start_wallclock(child.id()).expect("child start time");
        let after = Utc::now() + ChronoDuration::seconds(5);
        assert!(
            start > before && start < after,
            "a child spawned just now should read as started just now, got {start}"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn a_dead_pid_is_dead_whatever_its_entry_says() {
        // ~i32::MAX: above every plausible `pid_max`, so it can never exist.
        assert!(!pid_alive_since(2_147_483_640, Utc::now()));
        assert!(!tracked_pid_alive(2_147_483_640, None));
    }

    /// A genuinely-live tracked process must keep reading as alive — the
    /// regression this whole module must not introduce.
    #[test]
    fn a_live_child_whose_entry_is_fresh_reads_as_alive() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn fixture child");
        assert!(
            pid_alive_since(child.id(), Utc::now()),
            "a live child must not be reported dead"
        );
        assert!(tracked_pid_alive(child.id(), Some(Utc::now())));
        let _ = child.kill();
        let _ = child.wait();
    }

    /// The recycled-pid simulation: a live pid number paired with an entry
    /// that is far older than the process wearing it. Pre-#7935 this read as
    /// alive forever; it must now read as dead.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_live_pid_younger_than_its_entry_reads_as_dead() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn fixture child");
        let pid = child.id();
        assert!(
            crate::live_claim::pid_is_live_process(pid),
            "precondition: the stand-in process is alive, so only the identity \
             pairing can make this verdict dead"
        );
        assert!(
            !pid_alive_since(pid, Utc::now() - ChronoDuration::hours(6)),
            "a process that started long AFTER its entry did is a recycled pid"
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    // ---- registry level: the wedge itself ---------------------------------

    /// The #7935 acceptance criterion. A handle-less entry (the
    /// `reconstruct()`/journal-adopted shape) whose recorded pid has been
    /// recycled onto an unrelated live process must transition to a terminal
    /// state on the next reaper tick — so `count_in_flight_sweeps` drops to
    /// zero and `restart --drain` / `auto_update` stop being blocked by a
    /// phantom. Before this fix `reap_once` observed `is_pid_alive == true`
    /// and left the entry `Running` forever.
    #[cfg(target_os = "linux")]
    #[test]
    fn reap_once_terminates_an_entry_whose_pid_was_recycled() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let mut stand_in = spawn_stand_in();
        let pid = stand_in.id();

        // The entry is hours old; the process wearing its pid started just
        // now. No `Child` handle is ever inserted, so `poll_liveness` takes
        // exactly the fallback path this issue is about.
        let sweep_id = insert_running_with_pid_at(
            &mut registry,
            7935,
            1,
            pid,
            Utc::now() - ChronoDuration::hours(6),
        );
        assert_eq!(non_terminal(&registry), 1, "precondition: one in-flight sweep");

        let changed = registry.reap_once();

        assert!(changed >= 1, "the reaper must observe the recycled-pid death");
        assert!(
            registry
                .get(&sweep_id)
                .expect("entry still tracked")
                .state
                .is_terminal(),
            "a recycled pid must not keep its entry non-terminal forever (#7935)"
        );
        assert_eq!(
            non_terminal(&registry),
            0,
            "in-flight count must drain so `restart --drain` can complete"
        );

        let _ = stand_in.kill();
        let _ = stand_in.wait();
    }

    /// The regression guard for the fix: a handle-less entry whose process is
    /// genuinely still its own (start time consistent with `started_at`) must
    /// stay `Running` through a reaper tick.
    #[test]
    fn reap_once_leaves_a_genuinely_live_handleless_entry_running() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let mut stand_in = spawn_stand_in();
        let pid = stand_in.id();

        let sweep_id = insert_running_with_pid_at(&mut registry, 7936, 1, pid, Utc::now());

        registry.reap_once();

        assert!(
            !registry
                .get(&sweep_id)
                .expect("entry still tracked")
                .state
                .is_terminal(),
            "a live tracked process must never be reaped by the identity pairing"
        );
        assert_eq!(non_terminal(&registry), 1);

        let _ = stand_in.kill();
        let _ = stand_in.wait();
    }

    /// And a genuinely-dead handle-less entry still transitions terminal —
    /// the pre-#7935 behavior the identity pairing must not disturb.
    #[test]
    fn reap_once_still_terminates_a_genuinely_dead_handleless_entry() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let mut stand_in = spawn_stand_in();
        let pid = stand_in.id();
        let _ = stand_in.kill();
        let _ = stand_in.wait(); // reaped by us: the pid is gone, not a zombie

        let sweep_id = insert_running_with_pid_at(&mut registry, 7937, 1, pid, Utc::now());

        let changed = registry.reap_once();

        assert!(changed >= 1, "a dead pid is still observed as dead");
        assert!(registry
            .get(&sweep_id)
            .expect("entry still tracked")
            .state
            .is_terminal(),);
    }

    // ---- the recycled pgid the fix must NOT signal -------------------------

    /// Spawn a stranger that leads its own process group — `pgid == pid`, one
    /// live member. That is byte-for-byte the shape a recycled sweep pgid takes
    /// on this host, because `spawned_leader_pgid` only ever records a pgid
    /// that equals the leader's own pid.
    #[cfg(unix)]
    fn spawn_group_leading_stranger() -> std::process::Child {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("60")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        cmd.process_group(0);
        let child = cmd.spawn().expect("spawn group-leading stranger");
        assert!(wait_until_alive(child.id(), FIXTURE_CHILD_WAIT_MS), "the stranger must come up");
        child
    }

    #[test]
    fn a_dead_pid_number_is_not_a_recycled_group_id() {
        // Nothing wears this number, so the crash-path group reap must stay
        // enabled — the #4980 behavior the guard must not disturb.
        assert!(!pgid_number_was_recycled(2_147_483_640));
    }

    /// The bystander-safety half of #7935. Once `poll_liveness` reports a
    /// recycled pid as dead, the entry's terminal transition routes the very
    /// same (recycled) number into `reap_orphaned_group` as a pgid. Signalling
    /// it would SIGTERM-then-SIGKILL an unrelated process tree, so the reap
    /// must refuse — even though `group_has_members` says the group is live.
    #[cfg(unix)]
    #[test]
    fn reap_orphaned_group_refuses_a_group_id_recycled_onto_a_live_process() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let mut stranger = spawn_group_leading_stranger();
        let pgid = stranger.id();

        assert!(
            super::super::group_has_members(pgid),
            "precondition: the stranger's group is non-empty, so ONLY the #7935 \
             guard can stop the reap"
        );

        let reaped = registry.reap_orphaned_group("sweep-recycled-pgid", Some(7935), pgid);

        assert!(
            !reaped,
            "a group id whose number is worn by a live process was recycled — \
             signalling it kills a bystander"
        );
        assert!(
            registry.pending_group_reaps.is_empty(),
            "a refused reap must not arm a deferred SIGKILL escalation either"
        );
        assert!(
            crate::live_claim::pid_is_live_process(pgid),
            "the bystander must still be running — no signal was delivered"
        );

        let _ = stranger.kill();
        let _ = stranger.wait();
    }

    /// End-to-end: the recycled-pid entry goes terminal (the drain unblocks)
    /// *and* the stranger wearing the pid survives the tick untouched. These
    /// two properties are exactly the pair that must hold together.
    #[cfg(all(unix, target_os = "linux"))]
    #[test]
    fn a_recycled_pid_entry_drains_without_signalling_the_bystander() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());
        let mut stranger = spawn_group_leading_stranger();
        let pid = stranger.id();

        let sweep_id = insert_running_with_pid_at(
            &mut registry,
            7938,
            1,
            pid,
            Utc::now() - ChronoDuration::hours(6),
        );
        // The registry records `pgid == pid` for every sweep it dispatches
        // (`spawned_leader_pgid`), so reproduce that here: it is what makes the
        // crash-path group reap aim at the recycled number.
        registry
            .entries
            .get_mut(&sweep_id)
            .expect("entry just inserted")
            .pgid = Some(pid);

        registry.reap_once();

        assert!(
            registry
                .get(&sweep_id)
                .expect("entry still tracked")
                .state
                .is_terminal(),
            "the phantom entry must drain (#7935)"
        );
        assert_eq!(non_terminal(&registry), 0);
        assert!(
            crate::live_claim::pid_is_live_process(pid),
            "the unrelated process wearing the recycled pid must be untouched"
        );

        let _ = stranger.kill();
        let _ = stranger.wait();
    }
}
