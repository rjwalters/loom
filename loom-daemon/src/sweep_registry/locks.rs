//! Per-issue claim-lock lifecycle (`.loom/locks/issue-<N>/owner.json`) and
//! startup reconstruction of registry state from disk.

use super::*;

/// Typed, matchable error returned by [`SweepRegistry::dispatch`] when the
/// live-claim guard (Issue #4556, step 2.9) refuses a dispatch because a sweep
/// process for this issue is **confirmed still running**.
///
/// ## Why the existing guards were not enough
///
/// `acquire_lock` (step 3) already refuses when `.loom/locks/issue-<N>/`
/// *exists*, and #4463 made every reaper / cancel / watchdog *release* of that
/// lock ownership-checked. Neither covers the incident this guard closes:
/// issue #4275 was dispatched **seven times in 77 minutes** because each
/// re-dispatch path first *convinced itself the sweep was dead* — the
/// reconciler on a dead-looking recorded PID, the mid-build watchdog on a
/// terminal registry entry, the review-stall watchdog on a silent log — and
/// therefore released the lock (or reverted the label) before dispatching, so
/// step 3 had nothing left to collide with. Three further dispatches came from
/// a *second* `loom-daemon` instance on the same host that shared neither the
/// first daemon's memory nor its `.loom/locks/`.
///
/// Step 2.9 asks the strictly stronger question — is a sweep process for this
/// issue confirmed *live* right now? — via [`crate::live_claim::probe`], whose
/// evidence legs (live lock owner, machine-level journal, `/proc` sweep-process
/// scan) survive lock release, label drift, daemon restart, and a second daemon
/// instance.
///
/// Distinct, downcast-matchable type — same rationale as
/// [`OpenPrDispatchError`]: a live-claim refusal is a *deliberate skip*, not a
/// dispatch failure, so the work-finder attributes it to its in-flight skip
/// counter instead of the generic error tally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveClaimDispatchError {
    /// The issue whose dispatch was refused.
    pub issue: u32,
    /// Which signal proved the claim is still live.
    pub evidence: crate::live_claim::LiveClaimEvidence,
}

impl std::fmt::Display for LiveClaimDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "refusing to dispatch issue #{}: it still has a confirmed-live sweep claim — {} \
             (#4556 live-claim guard). A second concurrent sweep would share one worktree with \
             the live one.",
            self.issue, self.evidence
        )
    }
}

impl std::error::Error for LiveClaimDispatchError {}

/// Issue #4256: checkpoint phases at/after Builder completion. A crash whose
/// checkpoint reads one of these means a PR was opened for the issue, so
/// [`SweepRegistry::reap_once`]'s reaper-driven resume is eligible to
/// re-dispatch straight past the #4123 open-PR guard — the checkpoint-resume
/// machinery (#3373) then skips back to the correct phase (typically Judge)
/// rather than redoing the Builder.
///
/// Mirrors `VALID_PHASES` in `defaults/scripts/sweep-checkpoint.sh` (the
/// daemon only *reads* checkpoint phases; the sweep skill is the sole writer
/// and validator, so this is a read-side allowlist, not the canonical
/// source). `curator-done` is excluded (no PR exists yet — an ordinary
/// re-dispatch is exactly right). `merge-done` is excluded too: a merge
/// closes the issue, so the 2.5 closed-issue guard already refuses a
/// re-dispatch there and a resume would be a wasted forge round trip.
pub(crate) const RESUMABLE_CHECKPOINT_PHASES: [&str; 4] = [
    "builder-done",
    "judge-rejected",
    "judge-done",
    "doctor-done",
];

/// Outcome of an ownership-checked lock release (Issue #4463).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockReleaseOutcome {
    /// The lock dir was removed — either its `owner.json` `sweep_id` matched
    /// the releasing sweep, or the owner was unreadable (fail-open), or no lock
    /// existed (idempotent no-op).
    Released,
    /// A *different* sweep owns the lock (a newer sweep re-acquired the claim
    /// after the releasing one died). The lock was left intact; the caller MUST
    /// NOT restore the label or re-dispatch.
    Superseded,
    /// The lock's own recorded owner is **still a live `/loom:sweep <N>`
    /// process** (Issue #4556): the caller's dead-sweep verdict was wrong. The
    /// lock was left intact and, exactly like [`Self::Superseded`], the caller
    /// MUST NOT restore the label or re-dispatch.
    ///
    /// This is the release-side half of the #4556 fix. #4463 stopped a *dying*
    /// sweep's cleanup from clobbering a lock a *newer* sweep had re-acquired,
    /// but it still trusted the caller's claim that the sweep it names is dead.
    /// Issue #4275's storm began with exactly that trust being misplaced: a
    /// false-dead verdict released a live sweep's lock and reverted its label,
    /// re-opening the issue to the work-finder.
    ///
    /// Deliberately narrow to avoid the opposite failure (a permanently wedged
    /// issue): a bare `kill(pid, 0)` would also match an unrelated process that
    /// recycled the PID, so this outcome requires the PID to be live **and** its
    /// argv to target `/loom:sweep <N>`
    /// ([`crate::live_claim::pid_is_sweep_process_for`]). Anything less
    /// positive fails open and releases as before.
    HolderAlive,
}

impl LockReleaseOutcome {
    /// Whether the lock was deliberately **left in place** — the caller's sweep
    /// is not the live owner, so it must skip its label restore and any
    /// re-dispatch ([`Self::Superseded`], #4463; [`Self::HolderAlive`], #4556).
    #[must_use]
    pub fn retained(self) -> bool {
        matches!(self, Self::Superseded | Self::HolderAlive)
    }
}

/// On-disk owner metadata written inside the lock dir. Schema mirrors
/// `defaults/scripts/spawn-loop.sh:299-305`.
///
/// # Schema evolution (Issue #4980)
///
/// `pgid` was added after the fact, so it is `Option<u32>` + `#[serde(default)]`:
/// an `owner.json` written by a pre-#4980 daemon binary (no `pgid` key at all)
/// **must** still deserialize. Failing to parse it would be far worse than
/// missing the field — `reconstruct()`, `lock_owned_by_other()`, and
/// `live_sweep_lock_owner_pid()` all treat an unparseable owner as "no owner",
/// which drops a *live* sweep's lock. The absent-pgid case degrades to
/// single-PID signalling with a log line instead.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct LockOwner {
    pub(crate) issue: u32,
    pub(crate) owner_pid: u32,
    pub(crate) acquired_at: String,
    pub(crate) sweep_id: SweepId,
    /// Process group led by `owner_pid` (Issue #4980). Written by
    /// [`SweepRegistry::record_child_pid_in_lock`] once the child exists;
    /// `None` on a provisional (pre-spawn) record and on any `owner.json`
    /// written before this field existed.
    #[serde(default)]
    pub(crate) pgid: Option<u32>,
}

impl SweepRegistry {
    /// Cross-check `.loom/locks/issue-<N>/` against this registry's own
    /// in-memory entries and surface any issue whose lock has a **live**
    /// `owner_pid` but no matching non-terminal (`Pending`/`Running`) registry
    /// entry (Issue #4214).
    ///
    /// This is the structural fix for the "vanish window" incident: the
    /// in-flight union `loom-daemon status` reports is built solely from
    /// in-memory registry entries, so any read-path gap that silently drops an
    /// entry from that union (e.g. a torn/mid-write `workspaces.json`, or a
    /// root-spelling mismatch in the workspace pool causing a registry to be
    /// skipped for a query) makes a demonstrably-alive, locked sweep vanish
    /// from `status` with no trace — exactly the failure a liveness monitor
    /// misreads as "sweep is dead". The lock directory is independent,
    /// filesystem-durable evidence that the in-memory union cannot lose track
    /// of, so cross-checking it here makes that omission structurally
    /// impossible: the caller can render these as "alive, but state
    /// unreconciled" instead of omitting them.
    ///
    /// A **stale** lock (dead `owner_pid`) is deliberately excluded — that
    /// remains [`reconstruct`](Self::reconstruct)'s cleanup remit. Reporting a
    /// dead lock here would misrepresent a genuinely finished/crashed sweep as
    /// still running, which is the opposite of what this diagnostic is for.
    ///
    /// Returns `(issue, owner_pid)` pairs, sorted ascending by issue number.
    #[must_use]
    pub fn unregistered_locked_issues(&self) -> Vec<(u32, u32)> {
        let locks_dir = self.config.locks_dir();
        let mut result = Vec::new();
        let Ok(read_dir) = std::fs::read_dir(&locks_dir) else {
            return result;
        };
        for entry in read_dir {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let Some(issue_str) = name.strip_prefix("issue-") else {
                continue;
            };
            let Ok(issue): Result<u32, _> = issue_str.parse() else {
                continue;
            };
            let owner_path = path.join("owner.json");
            let owner: Option<LockOwner> = std::fs::read_to_string(&owner_path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok());
            let Some(owner) = owner else {
                // No (or unparsable) owner.json: nothing durable to cross-check
                // against — `reconstruct()`'s stale-lock cleanup owns this case.
                continue;
            };
            if !is_pid_alive(owner.owner_pid) {
                // Stale lock (dead owner): the sweep has actually finished or
                // crashed, not "unregistered" — do not report it as alive.
                continue;
            }
            if !self.has_tracked_sweep_for(issue) {
                result.push((issue, owner.owner_pid));
            }
        }
        result.sort_unstable();
        result
    }

    // ------------------------------------------------------------------------
    // Lock primitive (mirrors spawn-loop.sh:293-309)
    // ------------------------------------------------------------------------

    pub(crate) fn acquire_lock(&self, issue: u32, sweep_id: &str) -> Result<()> {
        let locks_dir = self.config.locks_dir();
        std::fs::create_dir_all(&locks_dir)
            .with_context(|| format!("failed to create locks dir {}", locks_dir.display()))?;
        let lock = locks_dir.join(format!("issue-{issue}"));

        // `mkdir` is POSIX-atomic — see spawn-loop.sh:286-292 for rationale.
        match std::fs::create_dir(&lock) {
            Ok(()) => {
                let owner = LockOwner {
                    issue,
                    // Provisional: the child does not exist yet. `dispatch`
                    // rewrites this with the spawned child's PID via
                    // `record_child_pid_in_lock` once the child is running
                    // (Issue #3808), so `reconstruct()` can recognise a live
                    // daemon sweep after a restart.
                    owner_pid: std::process::id(),
                    acquired_at: Utc::now().to_rfc3339(),
                    sweep_id: sweep_id.to_string(),
                    // Provisional too (Issue #4980): the child's process group
                    // does not exist yet, and recording the DAEMON's group here
                    // would be actively dangerous — a later group-kill would
                    // target the daemon itself. `record_child_pid_in_lock`
                    // stamps the real value once the child is running.
                    pgid: None,
                };
                let owner_json =
                    serde_json::to_string_pretty(&owner).context("serialize lock owner")?;
                std::fs::write(lock.join("owner.json"), owner_json)
                    .context("write lock owner.json")?;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(anyhow!(
                "lock collision: issue #{issue} is already claimed (lock at {})",
                lock.display()
            )),
            Err(e) => {
                Err(anyhow!("failed to acquire lock for issue #{issue} at {}: {e}", lock.display()))
            }
        }
    }

    /// Rewrite the lock's `owner.json` so `owner_pid` records the spawned
    /// sweep child's PID rather than the daemon's own PID (Issue #3808).
    ///
    /// `acquire_lock` runs *before* the child is spawned, so it can only
    /// stamp `std::process::id()` (the daemon) provisionally. After a real
    /// daemon restart that PID is gone by definition, which previously made
    /// `reconstruct()`'s lock pass treat every daemon-dispatched sweep as
    /// stale — dropping the lock and (before #3808) synthesizing a spurious
    /// `Crashed` entry even for a child that was still alive. Storing the
    /// child PID lets the lock pass admit a genuinely-live child as `Running`
    /// across a restart. The rest of the owner record is preserved.
    ///
    /// Issue #4980 additionally stamps `pgid` — the process group the child
    /// leads (`process_group(0)`, #3800). The pid alone is not enough to tear
    /// down a sweep after the spawning daemon is gone: the OS can only report a
    /// *live* process's group, so once the wrapper dies its surviving
    /// descendants become unreachable-by-group unless the value was persisted
    /// while it was alive. `None` leaves the field untouched (unknown group).
    pub(crate) fn record_child_pid_in_lock(
        &self,
        issue: u32,
        child_pid: u32,
        pgid: Option<u32>,
    ) -> Result<()> {
        let owner_path = self
            .config
            .locks_dir()
            .join(format!("issue-{issue}"))
            .join("owner.json");
        let existing = std::fs::read_to_string(&owner_path)
            .with_context(|| format!("read lock owner {}", owner_path.display()))?;
        let mut owner: LockOwner =
            serde_json::from_str(&existing).context("parse lock owner.json")?;
        owner.owner_pid = child_pid;
        if pgid.is_some() {
            owner.pgid = pgid;
        }
        let owner_json = serde_json::to_string_pretty(&owner).context("serialize lock owner")?;
        std::fs::write(&owner_path, owner_json)
            .with_context(|| format!("write lock owner {}", owner_path.display()))?;
        Ok(())
    }

    /// Release the lock dir for an issue (idempotent).
    ///
    /// UNCONDITIONAL: removes `.loom/locks/issue-<N>` regardless of which sweep
    /// owns it. Prefer [`release_lock_owned`](Self::release_lock_owned) from any
    /// reaper / cancel / re-dispatch path so a newer sweep's live claim is not
    /// clobbered (Issue #4463) — this remains for callers that intentionally
    /// want an owner-blind removal.
    pub fn release_lock(&self, issue: u32) -> Result<()> {
        let lock = self.config.locks_dir().join(format!("issue-{issue}"));
        if lock.exists() {
            std::fs::remove_dir_all(&lock)
                .with_context(|| format!("failed to remove lock dir {}", lock.display()))?;
        }
        Ok(())
    }

    /// Ownership-checked lock release (Issue #4463).
    ///
    /// Removes `.loom/locks/issue-<N>` **only when** its `owner.json`
    /// `sweep_id` matches `sweep_id` — the sweep being reaped / cancelled /
    /// re-dispatched. When a *different* sweep owns the lock (a newer sweep
    /// re-acquired the claim after this one died — the double-dispatch incident
    /// this guards against), the lock is left intact and
    /// [`LockReleaseOutcome::Superseded`] is returned so the caller skips any
    /// label restore and any re-dispatch: the newer sweep is the live owner and
    /// runs its own lifecycle.
    ///
    /// Issue #4556 adds a second refusal: even when the owner **is** this
    /// sweep, the lock is left intact and
    /// [`LockReleaseOutcome::HolderAlive`] returned if the recorded
    /// `owner_pid` is still a live `/loom:sweep <N>` process. #4463 trusted the
    /// caller's assertion that the sweep it names is dead; #4275's
    /// seven-dispatch storm started with exactly that assertion being wrong (a
    /// false-dead verdict released a live sweep's lock and reverted its label).
    ///
    /// FAIL-OPEN: a missing / unreadable / corrupt / unparseable `owner.json`
    /// falls back to the legacy unconditional removal — the release only refuses
    /// on a *positively-read, conflicting* owner or a *positively-confirmed*
    /// live sweep process, so a garbage lock file can never wedge an issue
    /// permanently. A non-existent lock dir is a no-op
    /// ([`LockReleaseOutcome::Released`], idempotent).
    #[must_use]
    pub fn release_lock_owned(&self, issue: u32, sweep_id: &str) -> LockReleaseOutcome {
        let lock = self.config.locks_dir().join(format!("issue-{issue}"));
        if !lock.exists() {
            return LockReleaseOutcome::Released;
        }
        if self.lock_owned_by_other(issue, sweep_id) {
            return LockReleaseOutcome::Superseded;
        }
        if let Some(pid) = self.live_sweep_lock_owner_pid(issue) {
            log::warn!(
                "release_lock: issue #{issue} lock is owned by sweep {sweep_id}, which the \
                 caller believes is dead — but pid {pid} is STILL a live `/loom:sweep {issue}` \
                 process. Leaving the lock intact and skipping any label restore / re-dispatch \
                 (#4556 false-dead verdict guard)."
            );
            return LockReleaseOutcome::HolderAlive;
        }
        if let Err(e) = std::fs::remove_dir_all(&lock) {
            log::warn!("release_lock: failed to remove lock dir {}: {e}", lock.display());
        }
        LockReleaseOutcome::Released
    }

    /// The lock's recorded `owner_pid` when it is **positively confirmed** to
    /// still be a live `/loom:sweep <issue>` process (Issue #4556).
    ///
    /// Fail-open in both directions that matter: a missing / unparseable
    /// `owner.json` yields `None`, and so does a live PID whose argv does *not*
    /// name this issue's sweep (a recycled PID). Only a positive match refuses a
    /// release, so this can never wedge an issue.
    pub(crate) fn live_sweep_lock_owner_pid(&self, issue: u32) -> Option<u32> {
        let owner_path = self
            .config
            .locks_dir()
            .join(format!("issue-{issue}"))
            .join("owner.json");
        let raw = std::fs::read_to_string(&owner_path).ok()?;
        let owner: LockOwner = serde_json::from_str(&raw).ok()?;
        crate::live_claim::pid_is_sweep_process_for(owner.owner_pid, issue)
            .then_some(owner.owner_pid)
    }

    /// Read-only ownership probe (Issue #4463): `true` iff the issue lock's
    /// `owner.json` records a `sweep_id` *different* from `sweep_id` — i.e. a
    /// newer sweep re-acquired the claim after the querying sweep died. Unlike
    /// [`release_lock_owned`](Self::release_lock_owned) this never mutates the
    /// filesystem, so a caller can gate destructive work (worktree cleanup,
    /// re-dispatch) on it without prematurely freeing the lock.
    ///
    /// FAIL-OPEN: a missing lock dir, or an unreadable / unparseable
    /// `owner.json`, resolves to `false` (not-conflicting) so a garbage owner
    /// file can never wedge an issue.
    pub(crate) fn lock_owned_by_other(&self, issue: u32, sweep_id: &str) -> bool {
        let owner_path = self
            .config
            .locks_dir()
            .join(format!("issue-{issue}"))
            .join("owner.json");
        // Only report a conflict on a POSITIVELY-read differing owner.
        match std::fs::read_to_string(&owner_path) {
            Ok(contents) => match serde_json::from_str::<LockOwner>(&contents) {
                Ok(owner) if owner.sweep_id != sweep_id => {
                    log::info!(
                        "release_lock: issue #{issue} lock is owned by sweep {} \
                         (sweep {sweep_id} was superseded) — leaving the lock intact and \
                         skipping any re-dispatch (#4463)",
                        owner.sweep_id
                    );
                    true
                }
                _ => false,
            },
            Err(_) => false,
        }
    }

    /// Take **exclusive ownership** of issue `N`'s claim lock before the
    /// mid-build watchdog does anything destructive to its worktree
    /// (Issue #4564).
    ///
    /// #4463 gated the watchdog's `clean_worktree` on the read-only
    /// [`lock_owned_by_other`](Self::lock_owned_by_other) probe. That narrowed
    /// but did not close a probe→clean TOCTOU: the lock could be free at probe
    /// time and be acquired by a cross-instance sweep microseconds later, and
    /// the watchdog would then `git reset --hard` a worktree a *newly live*
    /// sweep had just claimed — the #4449 data-loss shape all over again.
    /// Holding the lock across the clean removes the window: a peer that races
    /// in can no longer acquire the claim at all, and a peer that got there
    /// first is detected here so the clean is skipped entirely.
    ///
    /// Returns the watchdog's own `sweep_id` — the lock's new owner, to be
    /// handed to [`release_lock_owned`](Self::release_lock_owned) once the
    /// clean is done — or `None` when the claim belongs to someone else. On
    /// `None` the caller MUST touch nothing and MUST NOT consume the issue's
    /// single recovery retry (the claim may be free again on a later tick).
    ///
    /// Two paths, neither of which ever leaves the lock momentarily free
    /// (which would itself re-open the race it is closing):
    ///
    /// - **No lock dir** — [`acquire_lock`](Self::acquire_lock)'s POSIX-atomic
    ///   `mkdir`, the same primitive [`dispatch`](Self::dispatch) uses, so a
    ///   racing peer loses the `mkdir` and exactly one of the two proceeds.
    /// - **Lock dir present** — refuse when `owner.json` positively names a
    ///   *different* sweep; otherwise (the dead sweep's own stale lock, or a
    ///   fail-open unreadable/corrupt owner) take it over **in place** by
    ///   rewriting `owner.json`. The directory is deliberately never removed
    ///   and re-created: a release→re-acquire pair would expose exactly the
    ///   `mkdir`-sized window this method exists to eliminate.
    ///
    /// FAIL-CLOSED on the takeover write: if `owner.json` cannot be rewritten
    /// we do not own the lock, so we return `None` rather than clean a
    /// worktree we cannot fence.
    pub(crate) fn claim_lock_for_midbuild(
        &self,
        issue: u32,
        dead_sweep_id: &str,
    ) -> Option<String> {
        let watchdog_sweep_id = format!("midbuild-watchdog-{dead_sweep_id}");
        let lock = self.config.locks_dir().join(format!("issue-{issue}"));

        if !lock.exists() {
            return match self.acquire_lock(issue, &watchdog_sweep_id) {
                Ok(()) => Some(watchdog_sweep_id),
                Err(e) => {
                    log::info!(
                        "midbuild-watchdog: issue #{issue} claim lock was acquired by another \
                         sweep while recovering dead {dead_sweep_id} — not cleaning the worktree \
                         and not re-dispatching ({e}) (#4564)."
                    );
                    None
                }
            };
        }

        // The lock dir exists. Only a POSITIVELY-read differing owner refuses
        // (fail-open, as #4463 established) — anything else is the dead sweep's
        // own leftover claim, which this watchdog is entitled to take over.
        if self.lock_owned_by_other(issue, dead_sweep_id) {
            log::info!(
                "midbuild-watchdog: issue #{issue} lock now owned by a newer sweep \
                 (superseding dead {dead_sweep_id}) — not cleaning the worktree and not \
                 re-dispatching (#4463)."
            );
            return None;
        }

        let owner = LockOwner {
            issue,
            owner_pid: std::process::id(),
            acquired_at: Utc::now().to_rfc3339(),
            sweep_id: watchdog_sweep_id.clone(),
            // The watchdog holds this lock itself while it cleans a worktree —
            // there is no sweep child, and recording OUR OWN group would let a
            // later group-kill target the daemon (#4980).
            pgid: None,
        };
        let takeover = serde_json::to_string_pretty(&owner)
            .context("serialize midbuild-watchdog lock owner")
            .and_then(|json| {
                std::fs::write(lock.join("owner.json"), json).context("write lock owner.json")
            });
        match takeover {
            Ok(()) => Some(watchdog_sweep_id),
            Err(e) => {
                log::warn!(
                    "midbuild-watchdog: could not take over issue #{issue}'s stale claim lock at \
                     {} ({e}) — refusing to clean the worktree without holding the lock (#4564).",
                    lock.display()
                );
                None
            }
        }
    }

    /// Confirmed-live claim probe for `issue` (Issue #4556) — the evidence
    /// behind the dispatch-time live-claim guard (step 2.9) and reusable by any
    /// caller that must distinguish "a lock file exists" from "a sweep process
    /// is running".
    ///
    /// Read-only: unlike [`release_lock_owned`](Self::release_lock_owned) it
    /// never touches the filesystem, so it is safe to consult *before* a
    /// release, a label revert, or a re-dispatch. Delegates to
    /// [`crate::live_claim::probe_excluding`], scoped to this registry's
    /// workspace root and its configured journal path (tests point that at a
    /// tempdir, so the probe never reads the real `~/.loom/sweeps.json`).
    ///
    /// Issue #5236: passes this daemon's own pid as the exclusion whenever
    /// this registry has no tracked (non-terminal) entry for `issue` — the
    /// only way a lock's `owner_pid` can legitimately equal `std::process::id()`
    /// is `acquire_lock`'s provisional placeholder before the spawned child's
    /// real pid is recorded (`record_child_pid_in_lock`, #3808). If that
    /// rewrite never ran and this registry also has no tracked entry for the
    /// issue, the lock is stale by construction — a leaked placeholder from a
    /// `spawn_child` failure, not a confirmed-live claim — so the daemon's own
    /// (necessarily still-alive) pid must not count as evidence against
    /// itself. A registry that DOES have a tracked entry for the issue keeps
    /// the exclusion off, so an actual in-flight dispatch's transient
    /// pre-rewrite window is never misread as stale.
    #[must_use]
    pub fn live_claim_evidence(&self, issue: u32) -> Option<crate::live_claim::LiveClaimEvidence> {
        let own_untracked_pid = (!self.has_tracked_sweep_for(issue)).then_some(std::process::id());
        crate::live_claim::probe_excluding(
            &self.config.workspace_root,
            self.config.journal_path.as_deref(),
            issue,
            own_untracked_pid,
        )
    }

    /// Whether this registry has a non-terminal (`Pending`/`Running`) entry
    /// tracking a sweep for `issue` — shared by [`Self::live_claim_evidence`]
    /// (#5236) and [`Self::unregistered_locked_issues`] (#4214), both of
    /// which need the same "does our own bookkeeping know about this issue"
    /// question.
    #[must_use]
    fn has_tracked_sweep_for(&self, issue: u32) -> bool {
        self.entries.values().any(|info| {
            !info.state.is_terminal() && matches!(info.kind, SweepKind::Issue(i) if i == issue)
        })
    }

    // ------------------------------------------------------------------------
    // Reconstruction
    // ------------------------------------------------------------------------

    /// Reconstruct registry entries on daemon startup by combining:
    ///
    /// 1. Live lock dirs under `.loom/locks/issue-<N>/` (the lock's
    ///    `owner.json` records the dispatching daemon's PID and sweep ID).
    /// 2. Sweep checkpoints under `.loom/sweep-checkpoint/issue-<N>.json`
    ///    (#3373) — these survive crashes and signal that a sweep was in
    ///    flight even if the lock is gone.
    ///
    /// This is best-effort: locks whose `owner_pid` is dead are released
    /// (they're stale); locks whose owner is live are admitted as `Running`.
    ///
    /// # Daemon ownership of checkpoints (Issue #3808)
    ///
    /// `.loom/sweep-checkpoint/` is written by the shared `/loom:sweep` skill
    /// regardless of how the run was launched — an in-session (subagent-path)
    /// sweep writes checkpoints there just like a daemon-dispatched detached
    /// child does. A checkpoint file alone therefore does **not** imply the
    /// daemon owns the sweep. The daemon-ownership signal is the **lock**: only
    /// `dispatch` writes `.loom/locks/issue-<N>/`, and in-session sweeps never
    /// touch it. So the checkpoint pass synthesizes a `Crashed` recovery entry
    /// only for issues that had a daemon-owned lock whose owner PID is now dead
    /// (a genuine daemon-owned sweep whose process is gone). Checkpoints with
    /// no lock — in-session `/loom:sweep` runs the daemon never dispatched —
    /// are skipped, so the daemon no longer ingests phantom entries for sweeps
    /// it does not own. Genuine daemon-crash recovery is preserved because the
    /// lock survives a daemon crash (it is only removed on clean release).
    #[allow(clippy::too_many_lines)]
    pub fn reconstruct(&mut self) -> Result<usize> {
        let locks_dir = self.config.locks_dir();
        let mut admitted = 0usize;
        // Issues that had a daemon-owned lock whose owner PID is now dead.
        // These are the only issues whose checkpoints the checkpoint pass may
        // recover as `Crashed` (Issue #3808) — the lock is the daemon-ownership
        // signal that a bare checkpoint file lacks.
        let mut daemon_owned_dead: HashSet<u32> = HashSet::new();

        if locks_dir.exists() {
            for entry in std::fs::read_dir(&locks_dir)? {
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        log::warn!("read_dir error in {}: {e}", locks_dir.display());
                        continue;
                    }
                };
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                let Some(issue_str) = name.strip_prefix("issue-") else {
                    continue;
                };
                let Ok(issue): Result<u32, _> = issue_str.parse() else {
                    continue;
                };
                let owner_path = path.join("owner.json");
                let owner: Option<LockOwner> = std::fs::read_to_string(&owner_path)
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok());
                let Some(owner) = owner else {
                    // No owner.json — treat as stale, remove.
                    let _ = std::fs::remove_dir_all(&path);
                    continue;
                };
                if !is_pid_alive(owner.owner_pid) {
                    // Stale lock: the daemon-dispatched child's PID (recorded
                    // by `record_child_pid_in_lock`, #3808) is dead. This lock
                    // is the daemon's own crash-surviving evidence that it
                    // dispatched this issue, so record the issue — the
                    // checkpoint pass may recover it as `Crashed` — then drop
                    // the stale lock and continue.
                    //
                    // Issue #4980 crash-path reap: a dead *leader* does not mean
                    // a dead *tree*. The 2026-08-03 incident was exactly this
                    // shape — the tracked wrapper was gone while the `claude`
                    // agent it had spawned kept running (and relaunched its
                    // workload) against an issue whose claim had already been
                    // returned to the queue. The persisted `pgid` is the only
                    // remaining handle on those survivors (the OS cannot report
                    // a dead pid's group), so reap the group before dropping the
                    // lock that records it.
                    if let Some(pgid) = owner.pgid {
                        self.reap_orphaned_group(&owner.sweep_id, Some(issue), pgid);
                    }
                    daemon_owned_dead.insert(issue);
                    let _ = std::fs::remove_dir_all(&path);
                    continue;
                }
                let log_path = self.compute_log_path(issue);
                let started_at = chrono::DateTime::parse_from_rfc3339(&owner.acquired_at)
                    .map_or_else(|_| Utc::now(), |t| t.with_timezone(&Utc));
                let repo = Some(self.config.workspace_root.display().to_string());
                // Issue #4173: the lock owner.json does not record the token,
                // but the per-sweep log (which survives the restart) captured
                // the OAuth account at dispatch. Re-run the same parser, anchored
                // to owner.sweep_id, to restore attribution before falling back
                // to `unknown`. Degrades gracefully — adoption never fails here.
                let token_name = recover_adopted_token_name(&log_path, &owner.sweep_id);
                let runtime = recover_adopted_runtime(&log_path, &owner.sweep_id);
                // Issue #4980: carry the persisted process group onto the
                // reconstructed entry so a post-restart cancel still tears down
                // the WHOLE tree instead of degrading to a single-PID kill that
                // orphans the `claude` agent and its descendants. Re-verified
                // against the live owner rather than trusted blindly: the owner
                // is alive here (checked above), so the OS can confirm the
                // recorded group is still the one it leads. A disagreement means
                // the record is stale (PID recycled between daemons), and
                // group-killing a stranger's group is exactly the blast radius
                // this must never have — drop to `None` and degrade.
                let pgid = owner.pgid.filter(|&recorded| {
                    let actual = process_group_of(owner.owner_pid);
                    if actual == Some(recorded) {
                        true
                    } else {
                        log::warn!(
                            "reconstruct: issue #{issue} lock records pgid {recorded} for owner \
                             pid {} but the OS reports {actual:?} — ignoring the recorded group \
                             and degrading to single-PID signalling (#4980)",
                            owner.owner_pid
                        );
                        false
                    }
                });
                self.entries.insert(
                    owner.sweep_id.clone(),
                    SweepInfo {
                        sweep_id: owner.sweep_id.clone(),
                        kind: SweepKind::Issue(issue),
                        pid: owner.owner_pid,
                        pgid,
                        token_name,
                        runtime,
                        runtime_source: None,
                        log_path,
                        idempotency_key: None,
                        started_at,
                        state: SweepState::Running,
                        latest_phase: None,
                        pr_number: None,
                        // Lock owner.json does not record the model; the
                        // dispatching daemon instance is gone (#3482).
                        model: None,
                        // Effort is likewise unrecoverable from the lock (#3716).
                        effort: None,
                        // depends_on is not recorded in the lock owner (#3729).
                        depends_on: None,
                        // Owning workspace root, stamped for multi-repo
                        // disambiguation (#3929).
                        repo,
                    },
                );
                admitted += 1;
            }
        }

        // Checkpoints for daemon-owned sweeps whose process is gone -> Crashed
        // entries (so list_sweeps shows them; the next dispatch resumes via the
        // sweep skill). Gated on daemon ownership (Issue #3808): a checkpoint
        // is only recovered when a daemon-owned lock existed for its issue.
        let checkpoint_dir = self.config.checkpoint_dir();
        if checkpoint_dir.exists() {
            for entry in std::fs::read_dir(&checkpoint_dir)? {
                let Ok(entry) = entry else { continue };
                let path = entry.path();
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                let Some(rest) = name.strip_prefix("issue-") else {
                    continue;
                };
                let Some(issue_str) = rest.strip_suffix(".json") else {
                    continue;
                };
                let Ok(issue): Result<u32, _> = issue_str.parse() else {
                    continue;
                };
                // Skip if we already have a Running entry for this issue.
                let already_running = self.entries.values().any(|info| {
                    matches!(info.state, SweepState::Running | SweepState::Pending)
                        && matches!(info.kind, SweepKind::Issue(n) if n == issue)
                });
                if already_running {
                    continue;
                }
                // Issue #3808: only recover a checkpoint when the daemon has
                // independent evidence it dispatched this issue — a daemon-owned
                // lock existed for it (captured in the lock pass above). A bare
                // checkpoint file does NOT imply daemon ownership because the
                // shared /loom:sweep skill writes `.loom/sweep-checkpoint/`
                // regardless of launch mechanism. In-session sweeps never write
                // a lock, so their checkpoints are skipped here — no phantom
                // daemon registry entry.
                if !daemon_owned_dead.contains(&issue) {
                    continue;
                }
                let sweep_id = format!("sweep-issue-{issue}-recovered-{}", Utc::now().timestamp());
                let phase = read_checkpoint_phase(&path);
                let log_path = self.compute_log_path(issue);
                let repo = Some(self.config.workspace_root.display().to_string());
                self.entries.insert(
                    sweep_id.clone(),
                    SweepInfo {
                        sweep_id,
                        kind: SweepKind::Issue(issue),
                        pid: 0, // unknown — owner is gone
                        // Likewise unknown (#4980): the lock that recorded the
                        // process group was already removed as stale by the pass
                        // above, and its group (if any survivors remained) was
                        // reaped there.
                        pgid: None,
                        // Issue #4173: a checkpoint-only entry has no lock
                        // `sweep_id` anchor to recover the token against (the
                        // lock was already removed as stale above), so this
                        // path legitimately stays `unknown`.
                        token_name: "unknown".to_string(),
                        runtime: "unknown".to_string(),
                        runtime_source: None,
                        log_path,
                        idempotency_key: None,
                        started_at: Utc::now(),
                        state: SweepState::Crashed { at: Utc::now() },
                        latest_phase: phase,
                        pr_number: None,
                        model: None,      // not recoverable from a checkpoint-only entry
                        effort: None,     // not recoverable from a checkpoint-only entry
                        depends_on: None, // not recoverable from a checkpoint-only entry
                        // Owning workspace root, stamped for multi-repo
                        // disambiguation (#3929).
                        repo,
                    },
                );
                admitted += 1;
            }
        }

        Ok(admitted)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod tests {
    use super::*;
    use crate::sweep_registry::test_support::*;
    use serial_test::serial;
    use std::os::unix::fs::PermissionsExt;
    use std::time::SystemTime;
    use tempfile::tempdir;

    #[test]
    fn reconstruct_admits_live_lock_owners() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        // Write a lock dir with our own PID as the owner (guaranteed alive).
        let locks = registry.config.locks_dir();
        std::fs::create_dir_all(&locks).unwrap();
        let lock = locks.join("issue-77");
        std::fs::create_dir(&lock).unwrap();
        let owner = LockOwner {
            pgid: None,
            issue: 77,
            owner_pid: std::process::id(),
            acquired_at: Utc::now().to_rfc3339(),
            sweep_id: "sweep-issue-77-reconstruct".to_string(),
        };
        std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap())
            .unwrap();

        let admitted = registry.reconstruct().unwrap();
        assert!(admitted >= 1);
        let info = registry.get("sweep-issue-77-reconstruct").unwrap();
        assert_eq!(info.pid, std::process::id());
        assert!(matches!(info.state, SweepState::Running));
    }

    /// Issue #4214: a live-locked issue with **no** matching registry entry at
    /// all (no `reconstruct()` has run, no dispatch entry exists) must surface
    /// via `unregistered_locked_issues` — this is the "vanish window" case: the
    /// lock (filesystem-durable) proves the sweep is alive even though nothing
    /// in memory currently reflects it.
    #[test]
    fn unregistered_locked_issues_surfaces_live_lock_with_no_entry() {
        let dir = tempdir().unwrap();
        let (registry, _record_log) = fixture_registry(dir.path());

        let locks = registry.config.locks_dir();
        std::fs::create_dir_all(&locks).unwrap();
        let lock = locks.join("issue-4201");
        std::fs::create_dir(&lock).unwrap();
        let owner = LockOwner {
            pgid: None,
            issue: 4201,
            owner_pid: std::process::id(), // guaranteed alive
            acquired_at: Utc::now().to_rfc3339(),
            sweep_id: "sweep-issue-4201-1785221507".to_string(),
        };
        std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap())
            .unwrap();

        let unregistered = registry.unregistered_locked_issues();
        assert_eq!(
            unregistered,
            vec![(4201, std::process::id())],
            "a live-locked issue with no in-memory entry must surface as unregistered_locked"
        );
    }

    /// Issue #4214: once the registry has admitted the lock's sweep as a
    /// non-terminal entry (e.g. via `reconstruct()`, or a normal `dispatch()`),
    /// the same lock must NOT be reported as unregistered — it is registered.
    #[test]
    fn unregistered_locked_issues_excludes_registered_live_entry() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let locks = registry.config.locks_dir();
        std::fs::create_dir_all(&locks).unwrap();
        let lock = locks.join("issue-4202");
        std::fs::create_dir(&lock).unwrap();
        let owner = LockOwner {
            pgid: None,
            issue: 4202,
            owner_pid: std::process::id(),
            acquired_at: Utc::now().to_rfc3339(),
            sweep_id: "sweep-issue-4202-registered".to_string(),
        };
        std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap())
            .unwrap();

        // Reconstruct admits the lock's sweep as a `Running` entry.
        let admitted = registry.reconstruct().unwrap();
        assert!(admitted >= 1);

        let unregistered = registry.unregistered_locked_issues();
        assert!(
            unregistered.is_empty(),
            "a lock whose sweep is already a registered non-terminal entry must not be \
             reported as unregistered_locked; got: {unregistered:?}"
        );
    }

    /// Issue #4214: a **stale** lock (dead `owner_pid`) must NOT be reported as
    /// `unregistered_locked` — that lock is `reconstruct()`'s cleanup remit, not
    /// evidence the sweep is still alive.
    #[test]
    fn unregistered_locked_issues_excludes_stale_dead_pid_lock() {
        let dir = tempdir().unwrap();
        let (registry, _record_log) = fixture_registry(dir.path());

        let locks = registry.config.locks_dir();
        std::fs::create_dir_all(&locks).unwrap();
        let lock = locks.join("issue-4203");
        std::fs::create_dir(&lock).unwrap();
        let owner = LockOwner {
            pgid: None,
            issue: 4203,
            owner_pid: 2_147_483_640, // dead
            acquired_at: Utc::now().to_rfc3339(),
            sweep_id: "sweep-issue-4203-stale".to_string(),
        };
        std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap())
            .unwrap();

        let unregistered = registry.unregistered_locked_issues();
        assert!(
            unregistered.is_empty(),
            "a stale (dead-pid) lock must never surface as unregistered_locked; got: \
             {unregistered:?}"
        );
    }

    #[test]
    fn reconstruct_drops_stale_locks() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let locks = registry.config.locks_dir();
        std::fs::create_dir_all(&locks).unwrap();
        let lock = locks.join("issue-78");
        std::fs::create_dir(&lock).unwrap();
        let owner = LockOwner {
            pgid: None,
            issue: 78,
            owner_pid: 2_147_483_640, // dead
            acquired_at: Utc::now().to_rfc3339(),
            sweep_id: "sweep-issue-78-stale".to_string(),
        };
        std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap())
            .unwrap();

        let _ = registry.reconstruct().unwrap();
        assert!(!lock.exists(), "stale lock should be removed");
        assert!(registry.get("sweep-issue-78-stale").is_none());
    }

    /// Issue #3808: a checkpoint with no corresponding daemon-owned lock is an
    /// in-session `/loom:sweep` run the daemon never dispatched. `reconstruct`
    /// must NOT synthesize a phantom `Crashed` entry for it. (Replaces the old
    /// `reconstruct_admits_orphan_checkpoints_as_crashed`, which locked in the
    /// pre-#3808 overly-broad behavior.)
    #[test]
    fn reconstruct_skips_in_session_checkpoints_without_lock() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let cp_dir = registry.config.checkpoint_dir();
        std::fs::create_dir_all(&cp_dir).unwrap();
        // In-session checkpoint: no lock dir was ever written for it.
        std::fs::write(cp_dir.join("issue-91.json"), r#"{"phase":"judge","issue":91}"#).unwrap();

        let admitted = registry.reconstruct().unwrap();
        assert_eq!(admitted, 0, "in-session checkpoint must not be recovered");
        let crashed = registry.list(Some(&SweepState::Crashed { at: Utc::now() }));
        assert!(crashed.is_empty(), "no phantom Crashed entry for issue 91");
        assert!(registry.list(None).is_empty(), "registry must be empty");
    }

    /// Issue #3808: genuine daemon-crash recovery is preserved. A checkpoint
    /// whose issue had a daemon-owned lock with a now-dead owner PID (the
    /// daemon dispatched it, then crashed along with its child) IS recovered as
    /// a `Crashed` entry so the next dispatch resumes it.
    #[test]
    fn reconstruct_recovers_daemon_owned_checkpoint() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        // Daemon-owned lock with a dead owner PID (crashed daemon + child).
        let locks = registry.config.locks_dir();
        std::fs::create_dir_all(&locks).unwrap();
        let lock = locks.join("issue-91");
        std::fs::create_dir(&lock).unwrap();
        let owner = LockOwner {
            pgid: None,
            issue: 91,
            owner_pid: 2_147_483_640, // dead
            acquired_at: Utc::now().to_rfc3339(),
            sweep_id: "sweep-issue-91-daemon".to_string(),
        };
        std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap())
            .unwrap();

        // Matching checkpoint written by the (now-gone) daemon-dispatched child.
        let cp_dir = registry.config.checkpoint_dir();
        std::fs::create_dir_all(&cp_dir).unwrap();
        std::fs::write(cp_dir.join("issue-91.json"), r#"{"phase":"judge","issue":91}"#).unwrap();

        let admitted = registry.reconstruct().unwrap();
        assert!(admitted >= 1, "daemon-owned checkpoint must be recovered");
        let crashed = registry.list(Some(&SweepState::Crashed { at: Utc::now() }));
        assert_eq!(crashed.len(), 1);
        assert_eq!(crashed[0].latest_phase.as_deref(), Some("judge"));
        // The stale daemon lock is cleaned up as part of recovery.
        assert!(!lock.exists(), "stale daemon lock should be removed");
    }

    // --- ownership-checked lock release (Issue #4463) ---

    /// Core invariant: `release_lock_owned` must NOT delete a lock whose
    /// `owner.json` records a DIFFERENT `sweep_id` — a newer sweep re-acquired
    /// the claim after the releasing (older) sweep died. The lock survives and
    /// `Superseded` is returned so the caller skips any re-dispatch. This is the
    /// exact double-dispatch mechanism from the incident: reaping an old dead
    /// sweep must never free a live sweep's claim.
    #[test]
    fn release_lock_owned_preserves_lock_owned_by_different_sweep() {
        let dir = tempdir().unwrap();
        let (registry, _record_log) = fixture_registry(dir.path());

        // Newer sweep B owns the lock; older sweep A tries to release it.
        let lock = write_lock_owner(&registry, 4463, "sweep-issue-4463-newer", std::process::id());

        let outcome = registry.release_lock_owned(4463, "sweep-issue-4463-older-dead");
        assert_eq!(outcome, LockReleaseOutcome::Superseded);
        assert!(
            lock.exists(),
            "the newer sweep's live lock must survive an older sweep's release"
        );
        let owner: LockOwner =
            serde_json::from_str(&std::fs::read_to_string(lock.join("owner.json")).unwrap())
                .unwrap();
        assert_eq!(owner.sweep_id, "sweep-issue-4463-newer", "owner.json must be left untouched");
    }

    /// A sweep releasing its OWN lock (matching `sweep_id`) removes it —
    /// unchanged from the legacy unconditional release for the common case.
    #[test]
    fn release_lock_owned_removes_matching_owner() {
        let dir = tempdir().unwrap();
        let (registry, _record_log) = fixture_registry(dir.path());

        let lock = write_lock_owner(&registry, 4464, "sweep-issue-4464-mine", std::process::id());

        let outcome = registry.release_lock_owned(4464, "sweep-issue-4464-mine");
        assert_eq!(outcome, LockReleaseOutcome::Released);
        assert!(!lock.exists(), "a sweep must be able to release its own lock");
    }

    /// FAIL-OPEN: a corrupt / unparseable `owner.json` falls back to the legacy
    /// unconditional removal — a garbage lock file must never wedge an issue.
    #[test]
    fn release_lock_owned_fails_open_on_corrupt_owner() {
        let dir = tempdir().unwrap();
        let (registry, _record_log) = fixture_registry(dir.path());

        let locks = registry.config.locks_dir();
        std::fs::create_dir_all(&locks).unwrap();
        let lock = locks.join("issue-4465");
        std::fs::create_dir_all(&lock).unwrap();
        std::fs::write(lock.join("owner.json"), b"{ this is not valid json").unwrap();

        let outcome = registry.release_lock_owned(4465, "sweep-issue-4465-whoever");
        assert_eq!(outcome, LockReleaseOutcome::Released);
        assert!(!lock.exists(), "a corrupt owner.json must not wedge the lock (fail-open)");
    }

    /// FAIL-OPEN: a lock dir with a MISSING `owner.json` releases too (legacy
    /// spawn-loop locks predating the owner record, or a partially-written one).
    #[test]
    fn release_lock_owned_fails_open_on_missing_owner_json() {
        let dir = tempdir().unwrap();
        let (registry, _record_log) = fixture_registry(dir.path());

        let locks = registry.config.locks_dir();
        std::fs::create_dir_all(&locks).unwrap();
        let lock = locks.join("issue-4466");
        std::fs::create_dir_all(&lock).unwrap();
        // No owner.json written.

        let outcome = registry.release_lock_owned(4466, "sweep-issue-4466-whoever");
        assert_eq!(outcome, LockReleaseOutcome::Released);
        assert!(!lock.exists(), "a lock with no owner.json must release (fail-open)");
    }

    /// A non-existent lock is an idempotent no-op (`Released`), never a spurious
    /// `Superseded`.
    #[test]
    fn release_lock_owned_noop_when_no_lock() {
        let dir = tempdir().unwrap();
        let (registry, _record_log) = fixture_registry(dir.path());
        let outcome = registry.release_lock_owned(4467, "sweep-issue-4467-none");
        assert_eq!(outcome, LockReleaseOutcome::Released);
    }

    /// Release-side half of the fix: a lock whose owner is this very sweep, but
    /// whose PID is still a live `/loom:sweep <N>` process, is NOT released —
    /// the caller's dead-sweep verdict was wrong (`HolderAlive`), so the label
    /// restore and re-dispatch it would have triggered are both skipped.
    #[cfg(target_os = "linux")]
    #[test]
    fn release_lock_owned_refuses_when_the_owner_is_a_live_sweep_process() {
        let dir = tempdir().unwrap();
        let (registry, _record_log) = fixture_registry(dir.path());
        let sweep = FakeSweep::spawn(4564);
        let lock = write_lock_owner(&registry, 4564, "sweep-issue-4564-mine", sweep.pid());

        let outcome = registry.release_lock_owned(4564, "sweep-issue-4564-mine");

        assert_eq!(outcome, LockReleaseOutcome::HolderAlive);
        assert!(outcome.retained(), "HolderAlive must suppress the label restore / re-dispatch");
        assert!(lock.exists(), "a live sweep's lock must survive a false-dead release");
    }

    /// The inverse: a live PID that is *not* a sweep for this issue (a recycled
    /// PID) must NOT block the release. Without this the guard could wedge an
    /// issue permanently.
    #[test]
    fn release_lock_owned_releases_when_a_live_pid_is_not_this_issues_sweep() {
        let dir = tempdir().unwrap();
        let (registry, _record_log) = fixture_registry(dir.path());
        // Our own PID: live, but its argv is the test binary, not `/loom:sweep`.
        let lock = write_lock_owner(&registry, 4565, "sweep-issue-4565-mine", std::process::id());

        let outcome = registry.release_lock_owned(4565, "sweep-issue-4565-mine");
        assert_eq!(outcome, LockReleaseOutcome::Released);
        assert!(!outcome.retained());
        assert!(!lock.exists(), "a recycled PID must not wedge the lock");
    }

    #[test]
    fn lock_release_outcome_retained_covers_both_refusals() {
        assert!(!LockReleaseOutcome::Released.retained());
        assert!(LockReleaseOutcome::Superseded.retained());
        assert!(LockReleaseOutcome::HolderAlive.retained());
    }

    /// Issue #4173: adoption recovers the real OAuth account from the surviving
    /// per-sweep log (the `using OAuth account '<name>'` line anchored to the
    /// lock's `sweep_id`), so `status` shows the account, not `unknown`.
    #[test]
    fn reconstruct_recovers_token_name_from_log() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let locks = registry.config.locks_dir();
        std::fs::create_dir_all(&locks).unwrap();
        let lock = locks.join("issue-401");
        std::fs::create_dir(&lock).unwrap();
        let sweep_id = "sweep-issue-401-adopt";
        let owner = LockOwner {
            pgid: None,
            issue: 401,
            owner_pid: std::process::id(), // alive → admitted as Running
            acquired_at: Utc::now().to_rfc3339(),
            sweep_id: sweep_id.to_string(),
        };
        std::fs::write(lock.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap())
            .unwrap();

        // The per-sweep log survived the restart with the dispatch header and
        // the account-selection line the wrapper wrote.
        let log_path = registry.compute_log_path(401);
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        std::fs::write(
            &log_path,
            format!(
                "sweep_id={sweep_id} issue=401 ====\n\
                 spawn-claude: using OAuth account 'agent1-2amlogic' (mode=ranking)\n\
                 ...build output...\n"
            ),
        )
        .unwrap();

        let admitted = registry.reconstruct().unwrap();
        assert!(admitted >= 1);
        let info = registry.get(sweep_id).unwrap();
        assert_eq!(
            info.token_name, "agent1-2amlogic",
            "adopted sweep must recover its account from the log (#4173)"
        );
    }

    /// Issue #4173: a missing log, or a log without the selection line, degrades
    /// gracefully to `unknown` — adoption never fails on token capture.
    #[test]
    fn reconstruct_token_recovery_degrades_to_unknown() {
        let dir = tempdir().unwrap();
        let (mut registry, _record_log) = fixture_registry(dir.path());

        let locks = registry.config.locks_dir();
        std::fs::create_dir_all(&locks).unwrap();

        // Case A: log present but WITHOUT a selection line.
        let lock_a = locks.join("issue-402");
        std::fs::create_dir(&lock_a).unwrap();
        let owner_a = LockOwner {
            pgid: None,
            issue: 402,
            owner_pid: std::process::id(),
            acquired_at: Utc::now().to_rfc3339(),
            sweep_id: "sweep-issue-402-noline".to_string(),
        };
        std::fs::write(lock_a.join("owner.json"), serde_json::to_string_pretty(&owner_a).unwrap())
            .unwrap();
        let log_a = registry.compute_log_path(402);
        std::fs::create_dir_all(log_a.parent().unwrap()).unwrap();
        std::fs::write(&log_a, "sweep_id=sweep-issue-402-noline issue=402 ====\nno selection\n")
            .unwrap();

        // Case B: no log file at all (rotated/truncated away).
        let lock_b = locks.join("issue-403");
        std::fs::create_dir(&lock_b).unwrap();
        let owner_b = LockOwner {
            pgid: None,
            issue: 403,
            owner_pid: std::process::id(),
            acquired_at: Utc::now().to_rfc3339(),
            sweep_id: "sweep-issue-403-nolog".to_string(),
        };
        std::fs::write(lock_b.join("owner.json"), serde_json::to_string_pretty(&owner_b).unwrap())
            .unwrap();

        let admitted = registry.reconstruct().unwrap();
        assert!(admitted >= 2, "both sweeps admitted despite unrecoverable token");
        assert_eq!(
            registry.get("sweep-issue-402-noline").unwrap().token_name,
            UNKNOWN_TOKEN_NAME,
            "no selection line → unknown"
        );
        assert_eq!(
            registry.get("sweep-issue-403-nolog").unwrap().token_name,
            UNKNOWN_TOKEN_NAME,
            "missing log → unknown"
        );
    }
}
