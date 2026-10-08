//! Resume dispatch for a sweep a daemon roll paused (issue #10832; design
//! `docs/design/daemon-roll-pause-resume.md` §4 "Daemon-dispatched sweep", §7
//! H5 step 4).
//!
//! H4 stopped the agent at a safe point and left its claim lock, journal entry
//! and checkpoint in place. This is the other half: relaunch the **same
//! runtime session** in the same workspace, keeping the claim.
//!
//! It is deliberately not [`SweepRegistry::dispatch`]. That path decides
//! whether an issue may be *claimed*: the open-PR, backoff, live-claim,
//! peer-claim and lease-order guards, the claim lock, the label flip, a new
//! lease record. A resume claims nothing. The claim is already this host's, so
//! it checks that the claim is **still** this host's and otherwise leaves the
//! forge alone:
//!
//! | | fresh dispatch | roll resume |
//! |---|---|---|
//! | claim lock | `acquire_lock` (must not exist) | rebound to the new sweep id (must exist, owned by the paused run) |
//! | `loom:building` | flipped on | already on; never touched |
//! | lease record | new comment | the same record, refreshed, then renewed by a new loop |
//! | session | pinned fresh | the saved one (`--resume` / `exec resume`) |
//! | registry entry | `Running` | `Running`, lock records `resume_of` |
//! | `agent_started_at` | now | carried, so the 5-minute rule does not restart |
//!
//! Everything a refusal can say is one of design §9's closed reasons; the
//! caller (H5) requeues the item with it.
//!
//! The three steps mirror the dispatch split (#6592) so the registry mutex is
//! never held across the account-selection poll:
//! [`begin_roll_resume`](SweepRegistry::begin_roll_resume) →
//! [`poll_and_classify_spawned_child`](super::poll_and_classify_spawned_child)
//! → [`finish_roll_resume`](SweepRegistry::finish_roll_resume).

use super::locks::LockOwner;
use super::resume_handle::RollResumeLaunch;
use super::*;

/// Requeue reasons a resume can produce (design §9).
pub(crate) const REASON_LEASE_LOST: &str = "lease-lost";
pub(crate) const REASON_ISSUE_CLOSED: &str = "issue-closed";
pub(crate) const REASON_ISSUE_PARKED: &str = "issue-parked";
pub(crate) const REASON_WORKTREE_CHANGED: &str = "worktree-changed";
pub(crate) const REASON_RESUME_FAILED: &str = "session-resume-failed";
/// Prefix of `guard-refused:<step>`.
pub(crate) const REASON_GUARD_REFUSED: &str = "guard-refused";

/// Why a resume was refused: a closed-set reason and what was observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RollResumeRefusal {
    pub(crate) reason: String,
    pub(crate) detail: String,
}

impl RollResumeRefusal {
    pub(crate) fn new(reason: &str, detail: impl Into<String>) -> Self {
        Self {
            reason: reason.to_string(),
            detail: detail.into(),
        }
    }

    /// `guard-refused:<step>`.
    pub(crate) fn guard(step: &str, detail: impl Into<String>) -> Self {
        Self::new(&format!("{REASON_GUARD_REFUSED}:{step}"), detail)
    }
}

impl std::fmt::Display for RollResumeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason, self.detail)
    }
}

/// The worktree a paused sweep was working in, as the manifest recorded it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WorktreeExpectation {
    pub(crate) path: PathBuf,
    pub(crate) branch: Option<String>,
    pub(crate) head: Option<String>,
}

/// One paused sweep to resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RollResumeSpec {
    pub(crate) issue: u32,
    /// The paused run's sweep id (the manifest item id).
    pub(crate) old_sweep_id: String,
    pub(crate) launch: RollResumeLaunch,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    /// `None` when the sweep had not created its worktree yet.
    pub(crate) worktree: Option<WorktreeExpectation>,
}

impl RollResumeSpec {
    /// The sweep id the claim's lease record is published under.
    pub(crate) fn lease_sweep_id(&self) -> &str {
        self.launch
            .lease_sweep_id
            .as_deref()
            .unwrap_or(&self.old_sweep_id)
    }
}

/// A resumed child that has been spawned and not yet polled.
#[derive(Debug)]
pub(crate) struct PreparedRollResume {
    pub(crate) child: Child,
    pub(crate) header_anchor: String,
    pub(crate) log_path: PathBuf,
    pub(crate) issue: u32,
    /// The resumed run's own sweep id.
    pub(crate) sweep_id: SweepId,
    pub(crate) lease_sweep_id: String,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) admission: crate::runtime_preference::DispatchAdmission,
}

/// A resumed sweep that is now a tracked `Running` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResumedSweep {
    pub(crate) sweep_id: SweepId,
    pub(crate) pid: u32,
    pub(crate) log_path: PathBuf,
    pub(crate) header_anchor: String,
}

/// The state of a resumed sweep's child process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeChild {
    Running,
    /// It exited, with this code (`None` when a signal ended it).
    Exited(Option<i32>),
    /// The registry does not track it (any more).
    Untracked,
}

/// The part of a per-issue log written by the dispatch whose header carries
/// `header_anchor`.
#[must_use]
pub(crate) fn dispatch_region<'a>(log: &'a str, header_anchor: &str) -> &'a str {
    log.rfind(header_anchor).map_or(log, |at| &log[at..])
}

/// The sandbox mode `spawn-codex.sh` reported for the dispatch that wrote
/// `header_anchor`, from its `sandbox=<mode> source=…` log line. `None` when
/// the launch logged none (a Claude launch, or a script that words it
/// differently).
#[must_use]
pub(crate) fn launched_sandbox(log: &str, header_anchor: &str) -> Option<String> {
    dispatch_region(log, header_anchor)
        .lines()
        .filter_map(|line| line.split("spawn-codex: sandbox=").nth(1))
        .filter_map(|rest| rest.split_whitespace().next())
        .next_back()
        .map(str::to_string)
}

/// `Some(why)` when the resumed launch reports a sandbox mode other than the
/// one recorded for the session at its original launch. A resumed session
/// must never run under a different sandbox than it was started with.
#[must_use]
pub(crate) fn sandbox_mismatch(
    log_path: &Path,
    header_anchor: &str,
    recorded: Option<&str>,
) -> Option<String> {
    let recorded = recorded?;
    let log = std::fs::read(log_path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let launched = launched_sandbox(&log, header_anchor)?;
    (launched != recorded).then(|| {
        format!(
            "the resumed launch reports sandbox `{launched}` but the session was started under \
             `{recorded}`"
        )
    })
}

impl SweepRegistry {
    fn roll_lock_owner(&self, issue: u32) -> Option<LockOwner> {
        let path = self
            .config
            .locks_dir()
            .join(format!("issue-{issue}"))
            .join("owner.json");
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    }

    /// Whether `owner` is the paused run `old_sweep_id`, or a resume of it.
    fn owner_is_run(owner: &LockOwner, old_sweep_id: &str) -> bool {
        owner.sweep_id == old_sweep_id
            || owner
                .resume_handle
                .as_ref()
                .and_then(|h| h.resume_of.as_deref())
                == Some(old_sweep_id)
    }

    /// The sweep id and pid of a live process already resuming `old_sweep_id`
    /// (design §7 H5 failure edges: a crash during H5 must not relaunch an
    /// item whose relaunch had already started). Identity-paired: the pid must
    /// have started no earlier than the lock was rebound.
    pub(crate) fn live_roll_resume_of(&self, issue: u32, old_sweep_id: &str) -> Option<SweepId> {
        let owner = self.roll_lock_owner(issue)?;
        let resumes_it = owner
            .resume_handle
            .as_ref()
            .and_then(|h| h.resume_of.as_deref())
            == Some(old_sweep_id);
        (resumes_it
            && owner.owner_pid != std::process::id()
            && reaper::pid_identity::owner_pid_alive_since(owner.owner_pid, &owner.acquired_at))
        .then_some(owner.sweep_id)
    }

    /// The §4 cross-checks for one paused sweep: the claim is still this
    /// host's, the issue is still open and not parked, and the worktree is
    /// where and what it was. Read-only.
    ///
    /// # Errors
    /// The first check that fails, as a design §9 reason.
    pub(crate) fn roll_resume_checks(
        &self,
        spec: &RollResumeSpec,
    ) -> std::result::Result<(), RollResumeRefusal> {
        let issue = spec.issue;
        // The claim lock is the local half of the claim. Gone, or another
        // run's, means something released or re-took the claim meanwhile.
        match self.roll_lock_owner(issue) {
            Some(owner) if Self::owner_is_run(&owner, &spec.old_sweep_id) => {}
            Some(owner) => {
                return Err(RollResumeRefusal::guard(
                    "claim-lock",
                    format!("the claim lock for #{issue} is now owned by sweep {}", owner.sweep_id),
                ))
            }
            None => {
                return Err(RollResumeRefusal::guard(
                    "claim-lock",
                    format!("the claim lock for #{issue} is gone"),
                ))
            }
        }
        if !self.config.skip_label_flip {
            self.roll_resume_forge_checks(spec)?;
        }
        let Some(expected) = &spec.worktree else {
            return Ok(());
        };
        let changed = |what: String| Err(RollResumeRefusal::new(REASON_WORKTREE_CHANGED, what));
        if !expected.path.is_dir() {
            return changed(format!("{} is missing", expected.path.display()));
        }
        if let Some(branch) = &expected.branch {
            let now =
                roll_requeue::git_line(&expected.path, &["rev-parse", "--abbrev-ref", "HEAD"]);
            if now.as_deref() != Some(branch.as_str()) {
                return changed(format!(
                    "{} is on {} (was {branch})",
                    expected.path.display(),
                    now.as_deref().unwrap_or("an unreadable branch")
                ));
            }
        }
        if let Some(head) = &expected.head {
            let now = roll_requeue::git_line(&expected.path, &["rev-parse", "HEAD"]);
            if now.as_deref() != Some(head.as_str()) {
                return changed(format!(
                    "{} is at {} (was {head})",
                    expected.path.display(),
                    now.as_deref().unwrap_or("an unreadable HEAD")
                ));
            }
        }
        Ok(())
    }

    /// The forge half of [`Self::roll_resume_checks`]: lease, state, park.
    ///
    /// An unanswered read is not a refusal. The manifest's age check already
    /// bounds the window to one lease TTL from a refresh this host made, so a
    /// forge hiccup here must not requeue work that nothing else could have
    /// taken. Only a positive answer refuses.
    fn roll_resume_forge_checks(
        &self,
        spec: &RollResumeSpec,
    ) -> std::result::Result<(), RollResumeRefusal> {
        let issue = spec.issue;
        if let LeaseOwnerProbeResult::Found { host, sweep_id, .. } =
            self.freshest_lease_owner(issue)
        {
            let ours = self.published_host_id();
            if host != ours {
                return Err(RollResumeRefusal::new(
                    REASON_LEASE_LOST,
                    format!("the freshest lease on #{issue} is host {host} (sweep {sweep_id})"),
                ));
            }
            if sweep_id != spec.lease_sweep_id() && sweep_id != spec.old_sweep_id {
                return Err(RollResumeRefusal::new(
                    REASON_LEASE_LOST,
                    format!(
                        "the freshest lease on #{issue} is another sweep on this host ({sweep_id})"
                    ),
                ));
            }
        }
        if self.guard_closed_or_pr(issue) == Some(true) {
            return Err(RollResumeRefusal::new(
                REASON_ISSUE_CLOSED,
                format!("#{issue} closed while its agent was paused"),
            ));
        }
        if let Some(labels) = self.guard_issue_labels(issue) {
            if let Some(label) = self.first_park_label_in(issue, &labels) {
                return Err(RollResumeRefusal::new(
                    REASON_ISSUE_PARKED,
                    format!("#{issue} gained `{label}` while its agent was paused"),
                ));
            }
            if !labels.iter().any(|l| l == "loom:building") {
                return Err(RollResumeRefusal::guard(
                    "claim-label",
                    format!("#{issue} no longer carries `loom:building`"),
                ));
            }
        }
        Ok(())
    }

    /// Whether the claim on `issue` is still the paused run's to give back
    /// (design §8: "every H5 action re-checks the live state first"). A binary
    /// that could not read the manifest may have recovered the claim through
    /// ordinary restart recovery in the meantime, and another host may have
    /// taken the issue since: a requeue must then write nothing, or it would
    /// take `loom:building` off someone else's live claim.
    ///
    /// # Errors
    /// Why the claim is no longer this run's.
    pub(crate) fn roll_claim_still_ours(
        &self,
        issue: u32,
        old_sweep_id: &str,
    ) -> std::result::Result<(), String> {
        match self.roll_lock_owner(issue) {
            Some(owner) if Self::owner_is_run(&owner, old_sweep_id) => {}
            Some(owner) => {
                return Err(format!("the claim lock is now owned by sweep {}", owner.sweep_id))
            }
            None => return Err("the claim lock is gone (already recovered)".to_string()),
        }
        if self.config.skip_label_flip {
            return Ok(());
        }
        if let LeaseOwnerProbeResult::Found { host, sweep_id, .. } =
            self.freshest_lease_owner(issue)
        {
            if host != self.published_host_id() {
                return Err(format!("the freshest lease is host {host} (sweep {sweep_id})"));
            }
        }
        Ok(())
    }

    /// Refresh the lease record of a paused sweep once, before anything is
    /// relaunched, so no other host can take the claim while H5 works through
    /// the manifest. Returns what [`roll_requeue::refresh_lease_once`] needs,
    /// captured under the registry lock so the refresh itself runs without it.
    pub(crate) fn roll_resume_lease_identity(&self) -> Option<(PathBuf, String)> {
        (!self.config.skip_label_flip).then(|| self.lease_refresh_identity())
    }

    /// Step 1 of a resume: admit the recorded runtime, rebind the claim lock
    /// to a new sweep id, and spawn the child in resume mode. No forge write.
    ///
    /// # Errors
    /// With a design §9 reason. The lock is left as it was found, so the
    /// caller's requeue releases it.
    pub(crate) fn begin_roll_resume(
        &mut self,
        spec: &RollResumeSpec,
    ) -> std::result::Result<PreparedRollResume, RollResumeRefusal> {
        let issue = spec.issue;
        let kind = SweepKind::Issue(issue);
        let Some(owner) = self
            .roll_lock_owner(issue)
            .filter(|o| Self::owner_is_run(o, &spec.old_sweep_id))
        else {
            return Err(RollResumeRefusal::guard(
                "claim-lock",
                format!("the claim lock for #{issue} is not the paused run's any more"),
            ));
        };
        // The saved session only resumes on the runtime that owns it.
        let admission = if self.config.skip_label_flip {
            // Hermetic fixtures install no runtime manifests.
            crate::runtime_preference::DispatchAdmission::none()
        } else {
            match crate::runtime_admission::resolve_and_admit(
                &self.config.workspace_root,
                "sweep-lifecycle",
                Some(&spec.launch.runtime),
            ) {
                Ok(admitted) => crate::runtime_preference::DispatchAdmission {
                    admitted: Some(admitted),
                    backstop: None,
                },
                Err(rejection) => {
                    return Err(RollResumeRefusal::guard(
                        "runtime-admission",
                        format!(
                            "runtime {} is no longer admitted for a sweep: {}",
                            spec.launch.runtime, rejection.reason
                        ),
                    ))
                }
            }
        };
        let sweep_id = format!("{}-r{}", generate_sweep_id(&kind), spec.launch.resume_count);
        // Rebind the lock: same claim, new run. `acquired_at` is restamped so
        // the pid identity check pairs the lock with the process about to be
        // spawned, and the dead run's pid and group are dropped.
        let lock_path = self
            .config
            .locks_dir()
            .join(format!("issue-{issue}"))
            .join("owner.json");
        let rebound = LockOwner {
            owner_pid: std::process::id(),
            acquired_at: Utc::now().to_rfc3339(),
            sweep_id: sweep_id.clone(),
            pgid: None,
            ..owner.clone()
        };
        let write = |o: &LockOwner| {
            serde_json::to_string_pretty(o)
                .map_err(std::io::Error::other)
                .and_then(|body| std::fs::write(&lock_path, body))
        };
        if let Err(e) = write(&rebound) {
            return Err(RollResumeRefusal::guard(
                "claim-lock",
                format!("could not rebind {}: {e}", lock_path.display()),
            ));
        }
        self.apply_dispatch_stagger();
        let log_path = self.compute_log_path(issue);
        let model = spec.model.as_deref().filter(|m| !m.is_empty());
        let effort = spec.effort.as_deref().filter(|e| !e.is_empty());
        let spawned = self.spawn_child_process_for(
            &kind,
            &log_path,
            &sweep_id,
            model,
            effort,
            None,
            admission.admitted.as_ref(),
            None,
            Some(&spec.launch),
        );
        match spawned {
            Ok((child, header_anchor)) => Ok(PreparedRollResume {
                child,
                header_anchor,
                log_path,
                issue,
                sweep_id,
                lease_sweep_id: spec.lease_sweep_id().to_string(),
                model: model.map(str::to_string),
                effort: effort.map(str::to_string),
                admission,
            }),
            Err(e) => {
                // Put the lock back as the paused run's, so the requeue that
                // follows releases the claim it expects to find.
                let _ = write(&owner);
                Err(RollResumeRefusal::new(
                    REASON_RESUME_FAILED,
                    format!("could not spawn the resume: {e:#}"),
                ))
            }
        }
    }

    /// Step 3 of a resume, after the unlocked poll: record the running child
    /// exactly as [`Self::finish_issue_dispatch`] does, and start its lease
    /// renewal loop on the claim's existing lease record.
    ///
    /// # Errors
    /// `session-resume-failed` when the child already died in its preflight.
    pub(crate) fn finish_roll_resume(
        &mut self,
        prepared: PreparedRollResume,
        token_name: String,
        runtime: String,
        immediate_preflight_death: Option<&'static str>,
    ) -> std::result::Result<ResumedSweep, RollResumeRefusal> {
        let PreparedRollResume {
            mut child,
            header_anchor,
            log_path,
            issue,
            sweep_id,
            lease_sweep_id,
            model,
            effort,
            mut admission,
        } = prepared;
        if let Some(class) = immediate_preflight_death {
            let _ = child.wait();
            return Err(RollResumeRefusal::new(
                REASON_RESUME_FAILED,
                format!("the resumed launch died in its preflight ({class})"),
            ));
        }
        let pid = child.id();
        crate::runtime_preference::handoff::attach(admission.backstop.take(), pid);
        let pgid = dispatch::spawned_leader_pgid(pid);
        // A new renewal loop, watching the new process, on the SAME lease
        // record: the claim was kept, so its one record is kept too.
        drop(self.start_lease_renewal_loop(issue, &lease_sweep_id, pid, &log_path));
        self.children.insert(sweep_id.clone(), child);
        if let Err(e) =
            self.record_child_pid_in_lock(issue, pid, pgid, model.as_deref(), effort.as_deref())
        {
            log::warn!(
                "roll resume: failed to record child pid {pid} in the lock for issue #{issue}: {e}"
            );
        }
        self.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: SweepKind::Issue(issue),
                pid,
                pgid,
                token_name,
                runtime,
                runtime_source: admission.admitted.as_ref().map(|a| a.source.clone()),
                log_path: log_path.clone(),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model,
                effort,
                depends_on: None,
                repo: Some(self.config.workspace_root.display().to_string()),
                overflow: false,
            },
        );
        match self.config.resolve_journal_path() {
            Ok(journal_path) => {
                if let Err(e) = sweep_journal::record_sweep_at(
                    &journal_path,
                    &self.config.workspace_root.display().to_string(),
                    issue,
                    pid,
                    Utc::now(),
                ) {
                    log::warn!("sweep_journal: failed to record the resume of #{issue}: {e}");
                }
            }
            Err(e) => log::warn!("sweep_journal: cannot resolve the journal path: {e}"),
        }
        Ok(ResumedSweep {
            sweep_id,
            pid,
            log_path,
            header_anchor,
        })
    }

    /// How a resumed sweep's child is doing.
    pub(crate) fn roll_resume_child(&mut self, sweep_id: &str) -> ResumeChild {
        let Some(child) = self.children.get_mut(sweep_id) else {
            return ResumeChild::Untracked;
        };
        match child.try_wait() {
            Ok(None) => ResumeChild::Running,
            Ok(Some(status)) => ResumeChild::Exited(status.code()),
            Err(_) => ResumeChild::Untracked,
        }
    }

    /// Undo a resume that did not start its session: stop whatever is left of
    /// the child's tree and drop its entry. The lock stays (now naming the
    /// failed run) for [`Self::release_roll_item`].
    pub(crate) fn abandon_roll_resume(&mut self, sweep_id: &str) {
        if let Some(mut child) = self.children.remove(sweep_id) {
            let pid = child.id();
            let spec = crate::auto_update::pause_roll::teardown::TreeSpec {
                pid: Some(pid),
                pgid: Some(pid),
                ..Default::default()
            };
            let _ = crate::auto_update::pause_roll::teardown::teardown_tree(
                &spec,
                Duration::from_millis(500),
            );
            let _ = child.wait();
        }
        self.entries.remove(sweep_id);
    }

    /// H5 has finished with a paused sweep it did not resume: remove its lock
    /// and journal entry, and forget any entry for `sweep_ids` (the paused
    /// run's id, and a failed resume's). The checkpoint is kept, so the next
    /// dispatch skips the phases already done (design §9). A lock some other
    /// run owns by now is left alone.
    pub(crate) fn release_roll_item(&mut self, issue: u32, sweep_ids: &[&str]) {
        let lock = self.config.locks_dir().join(format!("issue-{issue}"));
        let ours = self.roll_lock_owner(issue).is_none_or(|o| {
            sweep_ids.contains(&o.sweep_id.as_str())
                || sweep_ids.iter().any(|id| Self::owner_is_run(&o, id))
        });
        if ours && lock.exists() {
            if let Err(e) = std::fs::remove_dir_all(&lock) {
                log::warn!("roll resume: could not remove {}: {e}", lock.display());
            }
        }
        if ours {
            if let Ok(journal_path) = self.config.resolve_journal_path() {
                let _ = sweep_journal::remove_sweep_at(
                    &journal_path,
                    &self.config.workspace_root.display().to_string(),
                    issue,
                );
            }
        }
        for id in sweep_ids {
            self.children.remove(*id);
            self.entries.remove(*id);
        }
    }

    /// H5 has finished with a paused **PR-set** sweep (`/loom:sweep --prs …`).
    /// It claims no issue, so there is no label and no lease to give back, and
    /// it is never resumed by this path: what it leaves behind is one claim
    /// lock per PR, which would block every later PR-set dispatch naming one
    /// of them. Release the locks `sweep_id` owns and forget its entry.
    /// Returns the PRs released.
    pub(crate) fn release_roll_prset(&mut self, sweep_id: &str) -> Vec<u32> {
        let mut released = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.config.locks_dir()) {
            for entry in entries.filter_map(std::result::Result::ok) {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(pr) = name.strip_prefix("pr-").and_then(|n| n.parse::<u32>().ok()) else {
                    continue;
                };
                let owned = std::fs::read_to_string(entry.path().join("owner.json"))
                    .ok()
                    .and_then(|raw| serde_json::from_str::<LockOwner>(&raw).ok())
                    .is_some_and(|o| o.sweep_id == sweep_id);
                if owned
                    && matches!(
                        self.release_pr_lock_owned(pr, sweep_id),
                        LockReleaseOutcome::Released
                    )
                {
                    released.push(pr);
                }
            }
        }
        self.children.remove(sweep_id);
        self.entries.remove(sweep_id);
        released.sort_unstable();
        released
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "roll_resume_tests.rs"]
mod tests;
