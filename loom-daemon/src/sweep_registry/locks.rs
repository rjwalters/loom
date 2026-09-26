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

/// Typed, matchable error returned by [`SweepRegistry::dispatch`] when the
/// local claim lock for the issue already exists (the atomic `mkdir` in
/// `acquire_lock` found it held) — Issue #8907. Before this type the refusal
/// was a bare `anyhow!`, so the work finder could only count it as a generic
/// dispatch error. The Display text is unchanged byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimLockDispatchError {
    /// The issue whose claim lock was already held.
    pub issue: u32,
    /// The lock directory that already existed.
    pub lock: PathBuf,
}

impl std::fmt::Display for ClaimLockDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "lock collision: issue #{} is already claimed (lock at {})",
            self.issue,
            self.lock.display()
        )
    }
}

impl std::error::Error for ClaimLockDispatchError {}

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
    /// Model this sweep was dispatched with (Issue #8056), stamped by
    /// [`SweepRegistry::record_child_pid_in_lock`] alongside the child pid.
    ///
    /// Added for the same reason as `pgid`, and with the same
    /// `Option`+`#[serde(default)]` schema-evolution contract: the value is
    /// known only to the dispatching daemon *instance*, so before this field
    /// existed a daemon restart erased it — [`SweepRegistry::reconstruct`] had
    /// nothing to restore from and set `model: None` on every adopted entry,
    /// which is what nulled `model`/`effort` on the resulting `sweep.outcome`
    /// telemetry record. `None` on a provisional (pre-spawn) record, on a
    /// pre-#8056 `owner.json`, and on a dispatch that requested no explicit
    /// model (the honest "inherited the default" case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    /// Reasoning effort this sweep was dispatched with (Issue #8056), stamped
    /// and restored exactly like [`model`](Self::model). `None` means no
    /// explicit `--effort` was requested — the child inherited the session
    /// default — which is a genuinely unknown level, never a fabricated one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) effort: Option<String>,
}

impl LockOwner {
    /// A provisional owner record: the three fields every lock has from the
    /// moment it is created, with `acquired_at` stamped now and every
    /// after-the-fact field (`pgid`, `model`, `effort`) unset.
    ///
    /// Exists so the optional fields stay in ONE place. Each was added
    /// separately (#4980, #8056) and every construction site had to grow a
    /// `field: None` line for it — which is both churn and a standing
    /// invitation to set one of them to a fabricated value at a site that does
    /// not actually know it. The real stamping happens in
    /// [`SweepRegistry::record_child_pid_in_lock`], once, when the child
    /// exists and the dispatch params are in scope.
    pub(crate) fn new(issue: u32, owner_pid: u32, sweep_id: impl Into<SweepId>) -> Self {
        LockOwner {
            issue,
            owner_pid,
            acquired_at: Utc::now().to_rfc3339(),
            sweep_id: sweep_id.into(),
            pgid: None,
            model: None,
            effort: None,
        }
    }
}

/// The pure filesystem-scan half of [`SweepRegistry::unregistered_locked_issues`]
/// (Issue #7526): walks `locks_dir` and cross-checks each live lock's issue
/// against `is_tracked` instead of `&SweepRegistry` directly, so it can run
/// without holding the registry's mutex — see
/// [`crate::sweep_registry::RegistrySnapshot::unregistered_locked_issues`],
/// which calls this on a cloned snapshot outside the lock, and the original
/// instance method above, which still calls it inline (under the lock, as
/// before) for every other caller.
///
/// Returns `(issue, owner_pid)` pairs, sorted ascending by issue number.
#[must_use]
pub(crate) fn scan_unregistered_locked_issues(
    locks_dir: &Path,
    is_tracked: &dyn Fn(u32) -> bool,
) -> Vec<(u32, u32)> {
    let mut result = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(locks_dir) else {
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
        if !pid_identity::owner_pid_alive_since(owner.owner_pid, &owner.acquired_at) {
            // Stale lock (dead owner): the sweep has actually finished or
            // crashed, not "unregistered" — do not report it as alive. The
            // probe is identity-paired (#7935) so a recycled pid number does
            // not resurrect a long-dead owner as a phantom live lock.
            continue;
        }
        if !is_tracked(issue) {
            result.push((issue, owner.owner_pid));
        }
    }
    result.sort_unstable();
    result
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
        scan_unregistered_locked_issues(&self.config.locks_dir(), &|issue| {
            self.has_tracked_sweep_for(issue)
        })
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
                // Provisional in every respect: the child does not exist yet,
                // so `owner_pid` is the DAEMON's pid and `pgid`/`model`/
                // `effort` are unset. `dispatch` rewrites all four via
                // `record_child_pid_in_lock` once the child is running (#3808,
                // #4980, #8056), which is what lets `reconstruct()` recognise
                // a live daemon sweep after a restart — and what keeps the
                // daemon's OWN process group from being recorded here, where a
                // later group-kill would target the daemon itself.
                let owner = LockOwner::new(issue, std::process::id(), sweep_id);
                let owner_json =
                    serde_json::to_string_pretty(&owner).context("serialize lock owner")?;
                std::fs::write(lock.join("owner.json"), owner_json)
                    .context("write lock owner.json")?;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(ClaimLockDispatchError { issue, lock }.into())
            }
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
    ///
    /// Issue #8056 additionally stamps the dispatch's `model`/`effort` for the
    /// same class of reason: they are known only to the dispatching daemon
    /// *instance*, so a restart used to erase them and every post-restart
    /// `sweep.outcome` telemetry record for an adopted sweep carried
    /// `model: null, effort: null` — not because no model was chosen, but
    /// because the choice was never written down. A `None` argument writes
    /// nothing (an unset dispatch param is an honest "inherited the default",
    /// never a fabricated level).
    pub(crate) fn record_child_pid_in_lock(
        &self,
        issue: u32,
        child_pid: u32,
        pgid: Option<u32>,
        model: Option<&str>,
        effort: Option<&str>,
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
        if let Some(model) = model.filter(|m| !m.is_empty()) {
            owner.model = Some(model.to_string());
        }
        if let Some(effort) = effort.filter(|e| !e.is_empty()) {
            owner.effort = Some(effort.to_string());
        }
        let owner_json = serde_json::to_string_pretty(&owner).context("serialize lock owner")?;
        std::fs::write(&owner_path, owner_json)
            .with_context(|| format!("write lock owner {}", owner_path.display()))?;
        Ok(())
    }

    // ------------------------------------------------------------------------
    // PR-set claim locks (Issue #5342)
    // ------------------------------------------------------------------------

    /// Lock dir for one PR-set member: `.loom/locks/pr-<N>/`.
    ///
    /// Deliberately a **separate namespace** from `.loom/locks/issue-<N>/`
    /// (the [`acquire_lock`](Self::acquire_lock) prefix): both
    /// [`reconstruct`](Self::reconstruct) and
    /// [`unregistered_locked_issues`](Self::unregistered_locked_issues) key
    /// strictly on the `issue-` prefix, so a `pr-<N>` dir is silently
    /// invisible to both — a `PrSet` dispatch therefore never gets
    /// misclassified as an `Issue` sweep on daemon restart, and never
    /// pollutes the `unregistered_locked` cross-check with a false positive.
    /// A daemon restart while a `PrSet` sweep is live does **not** currently
    /// re-adopt it (no `PrSet` counterpart to the `Issue` reconstruction
    /// pass exists yet) — its `pr-<N>` locks are orphaned until the next
    /// `loom-clean`/operator cleanup. Tracked as a known follow-up, not
    /// required by #5342's acceptance criteria (live dispatch/cancel/list,
    /// not daemon-restart survival).
    fn pr_lock_dir(&self, pr: u32) -> PathBuf {
        self.config.locks_dir().join(format!("pr-{pr}"))
    }

    /// Acquire the claim lock for one PR-set member (Issue #5342), mirroring
    /// [`acquire_lock`](Self::acquire_lock)'s POSIX-atomic `mkdir` primitive
    /// but under the `pr-<N>` namespace. Called once per PR number in the
    /// set at dispatch time; the caller rolls back every already-acquired
    /// lock on the first failure (see `dispatch_prset_inner`), so a
    /// collision on PR *k* never leaves PRs `0..k` claimed with no live
    /// sweep behind them.
    pub(crate) fn acquire_pr_lock(&self, pr: u32, sweep_id: &str) -> Result<()> {
        let locks_dir = self.config.locks_dir();
        std::fs::create_dir_all(&locks_dir)
            .with_context(|| format!("failed to create locks dir {}", locks_dir.display()))?;
        let lock = self.pr_lock_dir(pr);
        match std::fs::create_dir(&lock) {
            Ok(()) => {
                let owner = LockOwner::new(pr, std::process::id(), sweep_id);
                let owner_json =
                    serde_json::to_string_pretty(&owner).context("serialize PR lock owner")?;
                std::fs::write(lock.join("owner.json"), owner_json)
                    .context("write PR lock owner.json")?;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(anyhow!(
                "lock collision: PR #{pr} is already claimed by another sweep (lock at {})",
                lock.display()
            )),
            Err(e) => {
                Err(anyhow!("failed to acquire lock for PR #{pr} at {}: {e}", lock.display()))
            }
        }
    }

    /// Rewrite a PR-set member lock's `owner.json` with the spawned child's
    /// real PID/pgid, mirroring [`record_child_pid_in_lock`](Self::record_child_pid_in_lock)
    /// (Issue #3808/#4980's rationale applies identically here).
    pub(crate) fn record_child_pid_in_pr_lock(
        &self,
        pr: u32,
        child_pid: u32,
        pgid: Option<u32>,
    ) -> Result<()> {
        let owner_path = self.pr_lock_dir(pr).join("owner.json");
        let existing = std::fs::read_to_string(&owner_path)
            .with_context(|| format!("read PR lock owner {}", owner_path.display()))?;
        let mut owner: LockOwner =
            serde_json::from_str(&existing).context("parse PR lock owner.json")?;
        owner.owner_pid = child_pid;
        if pgid.is_some() {
            owner.pgid = pgid;
        }
        let owner_json = serde_json::to_string_pretty(&owner).context("serialize PR lock owner")?;
        std::fs::write(&owner_path, owner_json)
            .with_context(|| format!("write PR lock owner {}", owner_path.display()))?;
        Ok(())
    }

    /// Ownership-checked release for one PR-set member lock (Issue #5342),
    /// mirroring [`release_lock_owned`](Self::release_lock_owned)'s
    /// `sweep_id`-match ownership check.
    ///
    /// Deliberately **narrower** than `release_lock_owned`: it skips the
    /// #4556 `HolderAlive` live-process re-verification, because that check
    /// needs [`crate::live_claim::pid_is_sweep_process_for`]'s argv pattern
    /// match against `/loom:sweep <N>`, which a PR-set child's
    /// `/loom:sweep --prs <n1> <n2> ...` argv never matches. This is safe
    /// today because no watchdog re-dispatch path re-claims a `PrSet` sweep
    /// the way the mid-build/review-stall watchdogs do for `Issue` (both
    /// filter to `SweepKind::Issue` candidates only) — there is no
    /// false-dead-verdict race for this method to guard against yet. Revisit
    /// if a `PrSet`-aware watchdog is ever added.
    ///
    /// FAIL-OPEN, matching `release_lock_owned`: a missing or unreadable/
    /// unparseable `owner.json` releases rather than wedging the PR.
    #[must_use]
    pub(crate) fn release_pr_lock_owned(&self, pr: u32, sweep_id: &str) -> LockReleaseOutcome {
        let lock = self.pr_lock_dir(pr);
        if !lock.exists() {
            return LockReleaseOutcome::Released;
        }
        let owner_path = lock.join("owner.json");
        let owned_by_other = match std::fs::read_to_string(&owner_path) {
            Ok(contents) => match serde_json::from_str::<LockOwner>(&contents) {
                Ok(owner) if owner.sweep_id != sweep_id => {
                    log::info!(
                        "release_pr_lock: PR #{pr} lock is owned by sweep {} (sweep {sweep_id} \
                         was superseded) — leaving the lock intact (#5342, mirrors #4463)",
                        owner.sweep_id
                    );
                    true
                }
                _ => false,
            },
            Err(_) => false,
        };
        if owned_by_other {
            return LockReleaseOutcome::Superseded;
        }
        if let Err(e) = std::fs::remove_dir_all(&lock) {
            log::warn!("release_pr_lock: failed to remove lock dir {}: {e}", lock.display());
        }
        LockReleaseOutcome::Released
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

        // The watchdog holds this lock itself while it cleans a worktree —
        // there is no sweep child, so `pgid` stays unset (recording OUR OWN
        // group would let a later group-kill target the daemon, #4980) and so
        // do `model`/`effort` (no dispatch happened here, #8056).
        let owner = LockOwner::new(issue, std::process::id(), watchdog_sweep_id.clone());
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
                if !pid_identity::owner_pid_alive_since(owner.owner_pid, &owner.acquired_at) {
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
                    //
                    // Issue #7935: "dead" here now means *identity-paired*
                    // dead — the pid is gone, OR it is alive but belongs to a
                    // process that started long after this lock was acquired,
                    // i.e. an unrelated process that recycled the pid number
                    // while no daemon was running. Admitting that phantom as
                    // `Running` (the pre-#7935 behavior) wedged the entry
                    // non-terminal forever, blocking `restart --drain` and
                    // every `auto_update` roll on the host. In THAT case the
                    // recorded `pgid` (always the leader's own pid) names a
                    // number a stranger now owns, so `reap_orphaned_group`
                    // refuses it — see its own #7935 guard.
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
                        // Issue #8056: restored from the lock, which
                        // `record_child_pid_in_lock` now stamps at dispatch
                        // time. Before that the dispatching daemon instance's
                        // knowledge died with it (#3482/#3716) and every
                        // adopted sweep's `sweep.outcome` record reported a
                        // null model/effort. Still `None` for a pre-#8056
                        // `owner.json` and for a dispatch that requested no
                        // explicit value — never a fabricated one.
                        model: owner.model.clone(),
                        effort: owner.effort.clone(),
                        // depends_on is not recorded in the lock owner (#3729).
                        depends_on: None,
                        // Owning workspace root, stamped for multi-repo
                        // disambiguation (#3929).
                        repo,
                    },
                );
                crate::observability::lifecycle::execution_adopted(
                    &self.config.workspace_root,
                    &owner.sweep_id,
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

    /// Adopt still-running sweeps recorded in the **machine-level sweep
    /// journal** (`~/.loom/sweeps.json`, [`crate::sweep_journal`]) that this
    /// registry's lock-based [`reconstruct`](Self::reconstruct) did not
    /// recover (Issue #6262).
    ///
    /// # Why the lock pass is not sufficient on its own
    ///
    /// [`reconstruct`](Self::reconstruct) is the *primary* restart-survivorship
    /// mechanism and stays that way — it recovers the sweep's `sweep_id`,
    /// `acquired_at`, token attribution, runtime, and process group, none of
    /// which the journal records. But it can only see a sweep whose
    /// `.loom/locks/issue-<N>/owner.json` is still on disk and still parses:
    ///
    /// - A lock dir with a missing/corrupt `owner.json` is deleted as stale
    ///   **without ever asking whether a process is still running** for that
    ///   issue.
    /// - Any path that released the lock early while the child kept running
    ///   (an operator `loom-clean`, a mid-build watchdog takeover, a partially
    ///   completed release) leaves a live sweep with no lock at all.
    ///
    /// In every one of those cases the surviving child is invisible to capacity
    /// accounting, and nothing later re-adopts it:
    /// [`crate::claim_reconciliation`] — the periodic backstop — reconciles the
    /// forge labels of sweeps it can prove are **dead**; it never re-admits a
    /// live one. The journal is the only remaining host-global, pid-keyed
    /// record of "this sweep was dispatched and its process is still up", and
    /// it survives exactly the restart that wipes the in-memory registry.
    ///
    /// # Contract
    ///
    /// - **Union, never replacement.** An issue already tracked by a
    ///   non-terminal entry (which is what the lock pass produces) is skipped,
    ///   so calling this after `reconstruct()` can never double-count a sweep
    ///   against the concurrency budget.
    /// - **Scoped to this workspace.** Only journal entries whose `repo`
    ///   resolves to this registry's `workspace_root` are considered — the
    ///   journal is machine-level and spans every managed repo.
    /// - **Live pids only.** Each candidate is re-probed with the same
    ///   `kill(pid, 0)` liveness check the reaper uses, so a stale journal
    ///   record can never inflate occupancy.
    /// - **Read-only with respect to the filesystem.** It never creates,
    ///   rewrites, or removes a claim lock, never writes the journal, and never
    ///   dispatches — it only seeds in-memory accounting. The reaper's ordinary
    ///   liveness pass retires an adopted entry when its pid exits.
    ///
    /// Returns how many entries were adopted.
    pub fn adopt_live_journal_sweeps(
        &mut self,
        entries: &[crate::sweep_journal::JournalEntry],
    ) -> usize {
        let mut adopted = 0usize;
        for entry in entries {
            if !journal_entry_is_for_workspace(&self.config.workspace_root, &entry.repo) {
                continue;
            }
            if self.has_tracked_sweep_for(entry.issue) {
                continue;
            }
            // Identity-paired (#7935): a journal record whose pid number has
            // been recycled onto an unrelated process must not be adopted as
            // a live sweep — that phantom is un-reapable, since nothing this
            // daemon does can make the stranger exit.
            if !pid_identity::pid_alive_since(entry.pid, entry.started_at) {
                continue;
            }
            let sweep_id = format!("journal-adopted-issue-{}-{}", entry.issue, entry.pid);
            if self.entries.contains_key(&sweep_id) {
                continue;
            }
            let log_path = self.compute_log_path(entry.issue);
            self.entries.insert(
                sweep_id.clone(),
                SweepInfo {
                    sweep_id,
                    kind: SweepKind::Issue(entry.issue),
                    pid: entry.pid,
                    // The journal records no process group. Degrade to
                    // single-pid signalling rather than guessing — the same
                    // conservative choice `reconstruct` makes when the recorded
                    // group cannot be re-verified (#4980).
                    pgid: None,
                    // Not recorded in the journal; the log-derived recovery the
                    // lock pass uses is anchored to a `sweep_id` this entry does
                    // not have (#4173).
                    token_name: "unknown".to_string(),
                    runtime: "unknown".to_string(),
                    runtime_source: None,
                    log_path,
                    idempotency_key: None,
                    started_at: entry.started_at,
                    state: SweepState::Running,
                    latest_phase: None,
                    pr_number: None,
                    model: None,
                    effort: None,
                    depends_on: None,
                    repo: Some(self.config.workspace_root.display().to_string()),
                },
            );
            adopted += 1;
            log::warn!(
                "sweep_registry: adopted surviving sweep for issue #{} (pid {}) from the machine \
                 sweep journal — its claim lock did not survive the restart, so the lock-based \
                 reconstruct() could not see it (#6262)",
                entry.issue,
                entry.pid
            );
        }
        adopted
    }
}

/// Whether a machine-journal entry's `repo` string names `workspace_root`.
///
/// The journal stamps `repo` as `workspace_root.display().to_string()` at
/// dispatch time, so the overwhelmingly common case is an exact string match.
/// The canonicalized comparison is the fallback for a host where the daemon's
/// configured root and the registered workspace root differ only by a symlink
/// (`/tmp` vs `/private/tmp` on macOS is the routine example) — without it a
/// survivor in such a repo would silently fail to be adopted, which is the
/// exact failure mode this whole pass exists to close.
fn journal_entry_is_for_workspace(workspace_root: &Path, repo: &str) -> bool {
    let repo_path = Path::new(repo);
    if workspace_root == repo_path {
        return true;
    }
    match (workspace_root.canonicalize(), repo_path.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::expect_used,
    unused_imports
)]
mod tests;
