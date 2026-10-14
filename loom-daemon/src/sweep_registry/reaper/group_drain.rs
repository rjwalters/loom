//! Crash-path process-group reaping, and the gate that keeps a dead leader's
//! claim held until its process group has drained (Issues #4980, #11076).
//!
//! # One owner of a sweep's lifetime (#11076)
//!
//! The tracked pid is only the *leader* of a sweep's process group. When it
//! dies, the rest of the group can keep running — in the 2026-10-09 incident a
//! systemd scope stop killed the leader while the resilient wrapper went on to
//! start `Attempt 2/5` of the same session in the same worktree. The reaper
//! used to SIGTERM that group, defer the SIGKILL, and then release the claim
//! (`released without a pull request`) on the very same tick, so for a while
//! the daemon had given the issue back to the queue while a session it no
//! longer tracked was still writing to it: two owners of one session.
//!
//! [`SweepRegistry::await_group_drain`] makes the daemon the single owner. A
//! dead leader whose group still has members is NOT a dead sweep yet: the
//! group is SIGTERM'd (then SIGKILL'd after [`ORPHAN_GROUP_REAP_GRACE`], the
//! existing #4980 escalation), and the entry stays `Running` — claim, lock and
//! label untouched — until a later tick confirms the group is empty. Only then
//! does the reaper run its ordinary terminal transition. The wait is bounded by
//! [`ORPHAN_GROUP_RELEASE_CAP`] so a member the kernel will not let go of (an
//! uninterruptible sleep, an `EPERM` member) can never wedge the entry forever,
//! and it never blocks: every step is a deadline checked on a later tick.
use super::*;

/// Grace between the crash-path reaper's group SIGTERM and its SIGKILL
/// escalation (Issue #4980).
///
/// The escalation is deliberately deferred to a later reaper tick rather than
/// slept through inline: [`SweepRegistry::reap_once`] also runs on the
/// `ListSweeps` / `GetSweepStatus` read path (via `reap_liveness`) while holding
/// the registry mutex, and blocking there for a grace window is the exact
/// 2026-07-26 wedge [`REAP_GH_TIMEOUT`] exists to prevent. Five seconds means
/// the next ordinary tick (30s) is always past the deadline.
pub(crate) const ORPHAN_GROUP_REAP_GRACE: Duration = Duration::from_secs(5);

/// Upper bound on how long a dead leader's terminal transition (and therefore
/// its claim release) may wait for the process group to drain (Issue #11076).
///
/// Comfortably past SIGTERM + [`ORPHAN_GROUP_REAP_GRACE`] + SIGKILL + one more
/// ordinary 30s tick, so it only ever fires for a group that survived SIGKILL —
/// which no running user-space code can do. Releasing then is the lesser evil
/// against an entry wedged `Running` forever.
pub(crate) const ORPHAN_GROUP_RELEASE_CAP: Duration = Duration::from_secs(120);

/// A crash-path group reap awaiting SIGKILL escalation and/or drain (Issues
/// #4980, #11076).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingGroupReap {
    /// The process group that was SIGTERM'd.
    pub(crate) pgid: u32,
    /// When SIGKILL becomes due if the group still has members.
    pub(crate) escalate_at: Instant,
    /// Whether the SIGKILL escalation has already been sent.
    pub(crate) killed: bool,
    /// The leader's exit code as observed on the tick that found it dead.
    /// `poll_liveness` consumes the `Child` handle on that tick, so later ticks
    /// can only report `None`; the gate hands this back once the group drains.
    pub(crate) exit_code: Option<i32>,
    /// Past this instant the gate releases even with members left
    /// ([`ORPHAN_GROUP_RELEASE_CAP`]).
    pub(crate) release_deadline: Instant,
}

/// Verdict of [`SweepRegistry::await_group_drain`] (Issue #11076).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GroupDrain {
    /// The group still has live members: keep the entry live, release nothing.
    Draining,
    /// The group is empty (or cannot be safely addressed): run the terminal
    /// transition with this exit code.
    Drained(Option<i32>),
}

impl SweepRegistry {
    /// Terminate the surviving process group of a sweep whose **leader is
    /// already dead** (Issue #4980) — the crash path.
    ///
    /// A dead wrapper does not imply a dead tree. In the 2026-08-03 incident the
    /// tracked pid was gone while the `claude` agent it had spawned kept running
    /// against an issue whose claim had already been returned to the queue: a
    /// zombie agent, invisible to the registry (`in_flight: 0`), burning CPU and
    /// mutating a repo it no longer held. `signal_sweep` cannot help here — the
    /// OS refuses to report a dead pid's group — which is exactly why the pgid is
    /// persisted while the leader is alive.
    ///
    /// Sends SIGTERM now and registers a deferred SIGKILL escalation
    /// ([`ORPHAN_GROUP_REAP_GRACE`]) picked up by a later
    /// [`reap_once`](Self::reap_once) tick, so no caller ever blocks on a grace
    /// window while holding the registry mutex. A no-op (returning `false`) when
    /// the group is already empty — the overwhelmingly common case, where the
    /// leader's death took its whole tree with it — and, since #7935, when the
    /// group id has demonstrably been recycled onto a stranger (see
    /// [`pid_identity::pgid_number_was_recycled`]).
    pub(crate) fn reap_orphaned_group(
        &mut self,
        sweep_id: &str,
        issue: Option<u32>,
        pgid: u32,
    ) -> bool {
        if !group_addressable(sweep_id, pgid) || !group_has_members(pgid) {
            return false;
        }
        let scope = issue.map_or_else(String::new, |n| format!(" (issue #{n})"));
        log::warn!(
            "reap_orphaned_group: sweep {sweep_id}{scope} has a DEAD leader but its process \
             group {pgid} still has members — an orphaned agent/subtree (e.g. a wrapper retry) \
             still running this sweep's work. Sending SIGTERM to the group; escalating to \
             SIGKILL in {}s if it survives (#4980); the claim stays held until the group is \
             empty (#11076).",
            ORPHAN_GROUP_REAP_GRACE.as_secs()
        );
        send_group_signal(pgid, 15);
        let now = Instant::now();
        self.pending_group_reaps.insert(
            sweep_id.to_string(),
            PendingGroupReap {
                pgid,
                escalate_at: now + ORPHAN_GROUP_REAP_GRACE,
                killed: false,
                exit_code: None,
                release_deadline: now + ORPHAN_GROUP_RELEASE_CAP,
            },
        );
        true
    }

    /// Whether a **dead** leader's recorded `pgid` still names a populated,
    /// safely addressable group (#11076): non-zero, not our own, not recycled
    /// onto a stranger (#7935/#4980), and with live members. `reconstruct`
    /// uses it to admit such an entry as `Running` instead of dropping its
    /// lock, so [`Self::await_group_drain`] owns the release.
    pub(crate) fn group_still_populated(&self, sweep_id: &str, pgid: u32) -> bool {
        group_addressable(sweep_id, pgid) && group_has_members(pgid)
    }

    /// SIGKILL any orphaned group that survived its crash-path SIGTERM past
    /// [`ORPHAN_GROUP_REAP_GRACE`] (Issue #4980). Called at the top of every
    /// [`reap_once`](Self::reap_once) tick, mirroring how
    /// `retry_pending_quarantine_releases` drains its own deferred work.
    /// Cheap early-return when nothing is pending.
    ///
    /// #11076: a pending reap whose sweep entry is still live is *gated* — its
    /// terminal transition is waiting on [`Self::await_group_drain`], which
    /// owns removing it (and needs its stored exit code). Only ungated reaps
    /// (e.g. from `reconstruct`) are dropped here.
    pub(crate) fn escalate_pending_group_reaps(&mut self) {
        if self.pending_group_reaps.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut done: Vec<SweepId> = Vec::new();
        for (sweep_id, pending) in &mut self.pending_group_reaps {
            let gated = self.entries.get(sweep_id).is_some_and(|info| {
                matches!(info.state, SweepState::Running | SweepState::Pending)
            });
            if !group_has_members(pending.pgid) {
                // The SIGTERM worked (or the group drained on its own).
                if !gated {
                    done.push(sweep_id.clone());
                }
                continue;
            }
            if pending.killed || now < pending.escalate_at {
                continue;
            }
            log::warn!(
                "reap_orphaned_group: process group {} for sweep {sweep_id} survived SIGTERM — \
                 escalating to SIGKILL (#4980)",
                pending.pgid
            );
            send_group_signal(pending.pgid, 9);
            pending.killed = true;
            if !gated {
                done.push(sweep_id.clone());
            }
        }
        for sweep_id in done {
            self.pending_group_reaps.remove(&sweep_id);
        }
    }

    /// Gate a dead leader's terminal transition on its process group having
    /// drained (Issue #11076) — see the module docs.
    ///
    /// `exit_code` is what `poll_liveness` reported on THIS tick; the first
    /// tick's value is remembered across deferrals and returned with
    /// [`GroupDrain::Drained`]. A sweep with no recorded pgid, or whose pgid is
    /// zero / our own / recycled onto a stranger, is `Drained` immediately —
    /// exactly the pre-#11076 behaviour, since nothing safe can be waited on.
    pub(crate) fn await_group_drain(
        &mut self,
        sweep_id: &str,
        issue: Option<u32>,
        pgid: Option<u32>,
        exit_code: Option<i32>,
    ) -> GroupDrain {
        let pending = self.pending_group_reaps.get(sweep_id).copied();
        let code = exit_code.or(pending.and_then(|p| p.exit_code));
        let live_members =
            pgid.is_some_and(|pgid| group_addressable(sweep_id, pgid) && group_has_members(pgid));
        if !live_members {
            self.pending_group_reaps.remove(sweep_id);
            return GroupDrain::Drained(code);
        }
        let Some(pending) = pending else {
            let pgid = pgid.unwrap_or_default();
            if !self.reap_orphaned_group(sweep_id, issue, pgid) {
                return GroupDrain::Drained(code);
            }
            if let Some(p) = self.pending_group_reaps.get_mut(sweep_id) {
                p.exit_code = code;
            }
            return GroupDrain::Draining;
        };
        if Instant::now() >= pending.release_deadline {
            log::error!(
                "await_group_drain: process group {} for sweep {sweep_id} still has members {}s \
                 after SIGTERM{} — releasing the claim anyway so the entry cannot wedge (#11076)",
                pending.pgid,
                ORPHAN_GROUP_RELEASE_CAP.as_secs(),
                if pending.killed { " and SIGKILL" } else { "" }
            );
            self.pending_group_reaps.remove(sweep_id);
            return GroupDrain::Drained(code);
        }
        GroupDrain::Draining
    }
}

/// Whether `pgid` may be signalled on behalf of `sweep_id` at all: never zero,
/// never this process's own group (#4980), never a number recycled onto a live
/// stranger (#7935). Logs the refusal.
fn group_addressable(sweep_id: &str, pgid: u32) -> bool {
    if pgid == 0 || Some(pgid) == current_process_group() {
        log::error!(
            "reap_orphaned_group: refusing to signal process group {pgid} for sweep \
             {sweep_id} — it is zero or THIS process's own group (#4980)"
        );
        return false;
    }
    // Issue #7935. The recorded pgid is always the dead leader's own pid, so a
    // LIVE process wearing that number means the kernel reallocated it — which
    // it can only do once the group had no members left. The group that answers
    // to this number now is somebody else's, and signalling it would kill an
    // innocent tree.
    if pid_identity::pgid_number_was_recycled(pgid) {
        log::error!(
            "reap_orphaned_group: refusing to signal process group {pgid} for sweep \
             {sweep_id} — a live process currently OWNS pid {pgid}, so this group id was \
             recycled after the sweep's own group drained. The members it names today are \
             an unrelated process tree (#7935)."
        );
        return false;
    }
    true
}

#[cfg(test)]
#[path = "group_drain_tests.rs"]
mod tests;
