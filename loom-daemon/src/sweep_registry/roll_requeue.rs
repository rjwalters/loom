//! Requeue-and-record for an agent a daemon roll could not pause (issue
//! #10831; design `docs/design/daemon-roll-pause-resume.md` §9).
//!
//! A roll's H4 pause requeues every sweep it cannot resume: an agent younger
//! than `minResumableAgeSecs` (`young-agent-reset`), one that missed the pause
//! budget (`pause-budget-missed`), one with no session to resume
//! (`session-not-resumable`). "Never silently lost" means each requeue does
//! two forge writes here (the third leg, the `daemon.roll.item` event, is the
//! pause's own):
//!
//! 1. **Label**: [`SweepRegistry::restore_label_to_ready`], unchanged, so the
//!    #9463 rule (a closed issue is never re-queued) and the #4206/#4887 park
//!    rules keep applying.
//! 2. **Comment**: one comment naming the roll (`from → to`), the phase and
//!    age the agent had reached, the reason, and whether a worktree with
//!    uncommitted edits is left on the host.
//!
//! The claim lock, journal entry and checkpoint are **not** touched: H4 never
//! deletes them (design §8), and the kept checkpoint lets the requeued sweep's
//! next dispatch skip the phases it already finished.
//!
//! Lives beside `restore_to_ready.rs` rather than in `guards.rs`, which is
//! size-ratcheted.

use super::*;

use crate::auto_update::pause_manifest::{
    ItemKind, ManifestItem, RollStepLabelRelease, RollStepRelease,
};
use crate::cli::pr_lease::{self, Record, Role};
use crate::forge_check_claim::{self, LeaseRow};
use crate::step_role::StepRole;
use crate::worktree_ops::gh::OpenPrProbe;

enum StepLease {
    Own { role_row: Option<u64> },
    Gone,
    Retired,
    Peer,
}

impl SweepRegistry {
    /// The forge half common to H4 and H5. Only old-step sub-actions whose
    /// write and post-state were observed enter the durable progress record.
    pub(crate) fn roll_step_release(
        &self,
        item: &mut ManifestItem,
        notice: &RollRequeueNotice,
    ) -> std::result::Result<&'static str, String> {
        self.roll_step_release_with(item, notice, crate::pr_lease_reap::reap_for_roll)
    }

    pub(crate) fn roll_step_release_with(
        &self,
        item: &mut ManifestItem,
        notice: &RollRequeueNotice,
        reap: fn(&Path, &crate::pr_lease_reap::Held, &Path) -> bool,
    ) -> std::result::Result<&'static str, String> {
        let role = item.step.ok_or("missing roll step role")?;
        if item.kind != ItemKind::Sweep {
            return Err("recorded step is not a step sweep".into());
        }
        let number = match (role.acts_on_pr(), item.issue, item.pr) {
            (false, Some(number), None) | (true, None, Some(number)) if number != 0 => number,
            _ => return Err("recorded roll step and item disagree".into()),
        };
        if self.config.skip_label_flip {
            return Ok("none");
        }
        let mut progress = item.roll_step_release()?;
        let result =
            self.perform_roll_step_release(item, number, role, notice, &mut progress, reap);
        item.set_roll_step_release(&progress);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn perform_roll_step_release(
        &self,
        item: &ManifestItem,
        number: u32,
        role: StepRole,
        notice: &RollRequeueNotice,
        progress: &mut RollStepRelease,
        reap: fn(&Path, &crate::pr_lease_reap::Held, &Path) -> bool,
    ) -> std::result::Result<&'static str, String> {
        let (_, labels) = self.roll_step_labels(number, role)?;
        if crate::work_finder::is_frozen(&labels) {
            return Ok("none");
        }
        let claim = role.claim_label();
        let ready = role.restore_label();
        let claim_exists = claim.is_some_and(|label| labels.iter().any(|l| l == label));
        let host = self.published_host_id();
        if progress.lease_host.as_ref().is_some_and(|old| old != &host) {
            return Err("the recorded lease host differs from this registry".into());
        }
        // Role leases are scoped to the step id. Only the Builder's sweep
        // lease can retain the old lease identity through a relaunch.
        let lease_id = if role == StepRole::Builder {
            item.resume_handle
                .as_ref()
                .and_then(|h| h.lease_sweep_id.as_deref())
                .unwrap_or(&item.id)
        } else {
            &item.id
        };
        let lease = self.roll_step_lease(number, role, &host, lease_id)?;
        let role_row = match lease {
            StepLease::Peer => return Ok("none"),
            StepLease::Gone if claim_exists => {
                return Err(
                    "claim label is present without positive old-step lease ownership".into()
                );
            }
            StepLease::Retired if claim_exists => return Ok("none"),
            StepLease::Gone | StepLease::Retired => {
                return self.roll_step_notice(item, number, role, notice, progress);
            }
            StepLease::Own { role_row } => role_row,
        };
        if progress.lease_row.is_some() && progress.lease_row != role_row {
            return Err("the live lease differs from the recorded row".into());
        }
        progress.lease_host = Some(host.clone());
        progress.lease_row = role_row;
        if role == StepRole::Builder {
            let Some(owner) = self.roll_lock_owner(number) else {
                return Err("could not read the Builder lock owner".into());
            };
            if !Self::owner_is_run(&owner, &item.id)
                || self.live_roll_resume_of(number, &item.id).is_some()
            {
                return Ok("none");
            }
            match self.probe_open_linked_pr(number) {
                OpenPrProbe::Open(_) => return Ok("none"),
                OpenPrProbe::ProbeFailed => {
                    return Err("could not verify Builder open-PR hold".into())
                }
                OpenPrProbe::NoneOpen => {}
            }
        }
        if let Some(claim) = claim.filter(|_| claim_exists) {
            let started = item
                .run_started_at
                .or(item.agent_started_at)
                .ok_or("missing recorded step start")?;
            let labeled_at = self
                .fetch_label_applied_at(number, claim)
                .ok_or("could not verify step claim label history")?;
            if role == StepRole::Builder
                && super::step_reap::label_reapplied_after(labeled_at, started)
                || role != StepRole::Builder
                    && super::step_reap::label_reapplied_after(started, labeled_at)
            {
                return Ok("none");
            }
            // A claim present after our confirmed removal is another claim.
            if progress.claim_removed {
                return Ok("none");
            }
            let (closed, labels) = self.roll_step_labels(number, role)?;
            if let Some(plan) = self.plan_roll_step_release(number, role, &labels, closed) {
                let mut remove_claim = true;
                if plan == RollStepLabelRelease::QueueRestored {
                    // The ready-label write must complete before the old
                    // claim is removed, including across a partial response.
                    match self.roll_step_lease(number, role, &host, lease_id)? {
                        StepLease::Own { role_row: row } if row == role_row => {}
                        StepLease::Peer | StepLease::Retired => return Ok("none"),
                        _ => {
                            return Err("step lease changed before restoring the ready label".into())
                        }
                    }
                    self.add_roll_ready_label(number, role)
                        .map_err(|e| format!("{e:#}"))?;
                    let (_, after) = self.roll_step_labels(number, role)?;
                    if !ready.is_some_and(|r| after.iter().any(|l| l == r)) {
                        return Err("could not verify the restored ready label".into());
                    }
                    progress.queue_restored = true;
                    if crate::work_finder::is_frozen(&after) {
                        return Ok("none");
                    }
                    remove_claim = after.iter().any(|label| label == claim);
                }
                if remove_claim {
                    let current_label = self
                        .fetch_label_applied_at(number, claim)
                        .ok_or("could not recheck the step claim label")?;
                    if current_label != labeled_at {
                        return Ok("none");
                    }
                    match self.roll_step_lease(number, role, &host, lease_id)? {
                        StepLease::Own { role_row: row } if row == role_row => {}
                        StepLease::Peer | StepLease::Retired => return Ok("none"),
                        _ => {
                            return Err("step lease changed before removing the claim label".into())
                        }
                    }
                    self.remove_roll_claim_label(number, role)
                        .map_err(|e| format!("{e:#}"))?;
                    let (_, after) = self.roll_step_labels(number, role)?;
                    if after.iter().any(|label| label == claim) {
                        return Err("could not verify removal of the step claim label".into());
                    }
                    progress.claim_removed = true;
                }
            } else if crate::work_finder::is_frozen(&labels) {
                return Ok("none");
            }
        }
        // A failed exchange is reconciled against the exact departing lease
        // row. An already-released lease never counts as a new action.
        match self.roll_step_lease(number, role, &host, lease_id)? {
            StepLease::Peer => return Ok(progress.outcome()),
            StepLease::Gone | StepLease::Retired => {}
            StepLease::Own { role_row: current } if current != role_row => {
                return Err("step lease changed before it could be released".into());
            }
            StepLease::Own { role_row: Some(id) } => {
                let held = crate::pr_lease_reap::Held {
                    pr: u64::from(number),
                    role: role.role_name().into(),
                    host: host.clone(),
                    sweep: lease_id.into(),
                };
                if !reap(&self.config.workspace_root, &held, &self.resolved_gh()) {
                    return Err("could not confirm reap of the old role lease".into());
                }
                let rows = self
                    .read_roll_role_lease_rows(number)
                    .ok_or("could not verify reaped role lease")?;
                if !rows.iter().any(|row| {
                    row.id == id
                        && matches!(pr_lease::parse_record(&row.body), Some(Record::Released(owner))
                        if owner.host == host && owner.sweep == lease_id
                            && owner.role.as_str() == role.role_name() && owner.step == Some(role))
                }) {
                    return Err("old role lease release is unverified".into());
                }
                progress.lease_released = true;
            }
            StepLease::Own { role_row: None } => {
                if !self.yield_finished_builder_lease(number, lease_id, "roll-requeue") {
                    return Err("could not yield the old Builder lease".into());
                }
                let rows = self.roll_builder_lease_rows(number)?;
                if !rows.iter().any(|row| {
                    forge_check_claim::parse_yield_marker_line(&row.body)
                        .is_some_and(|(owner, sweep)| owner == host && sweep == lease_id)
                }) {
                    return Err("Builder lease yield is unverified".into());
                }
                progress.lease_released = true;
            }
        }
        self.roll_step_notice(item, number, role, notice, progress)
    }

    fn roll_step_labels(
        &self,
        number: u32,
        role: StepRole,
    ) -> std::result::Result<(bool, Vec<String>), String> {
        let path = format!("repos/{{owner}}/{{repo}}/issues/{number}");
        let output = self
            .gh_read(
                "roll.step_item",
                [
                    "api",
                    &path,
                    "--jq",
                    "{state, labels:[.labels[].name], pr:has(\"pull_request\")}",
                ],
            )
            .map_err(|e| e.to_string())?
            .filter(|response| response.status.success())
            .ok_or("could not read step item state")?;
        let value: serde_json::Value =
            serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
        if value["pr"].as_bool() != Some(role.acts_on_pr()) {
            return Err("forge item does not match the recorded step role".into());
        }
        let closed = match value["state"].as_str() {
            Some("open") => false,
            Some("closed") => true,
            _ => return Err("step item state is unverified".into()),
        };
        let labels = value["labels"]
            .as_array()
            .ok_or("step item labels are unverified")?
            .iter()
            .map(|label| label.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or("step item labels are malformed")?;
        Ok((closed, labels))
    }

    fn roll_builder_lease_rows(&self, number: u32) -> std::result::Result<Vec<LeaseRow>, String> {
        let projection = forge_check_claim::issue_lease_comments_jq();
        let raw = self.roll_comment_rows(number, &projection)?;
        let rows = forge_check_claim::parse_lease_rows(&raw);
        if rows.len() != raw.lines().count()
            || rows.iter().any(|row| {
                Self::parse_lease_marker_line(&row.body).is_none()
                    && forge_check_claim::parse_yield_marker_line(&row.body).is_none()
            })
        {
            return Err("malformed Builder lease record".into());
        }
        Ok(rows)
    }

    fn roll_step_lease(
        &self,
        number: u32,
        role: StepRole,
        host: &str,
        sweep: &str,
    ) -> std::result::Result<StepLease, String> {
        if role == StepRole::Builder {
            let rows = self.roll_builder_lease_rows(number)?;
            let live = forge_check_claim::live_leases_by_freshness(
                &rows,
                Utc::now(),
                crate::claim_reconciliation::resolve_lease_ttl_minutes(),
            );
            return Ok(match live.as_slice() {
                [] if rows.iter().any(|row| {
                    forge_check_claim::parse_yield_marker_line(&row.body)
                        .is_some_and(|(owner, id)| owner == host && id == sweep)
                }) =>
                {
                    StepLease::Retired
                }
                [] => StepLease::Gone,
                [(h, s)] if h == host && s == sweep => StepLease::Own { role_row: None },
                _ => StepLease::Peer,
            });
        }
        let rows = self
            .read_roll_role_lease_rows(number)
            .ok_or("could not read step role leases")?
            .into_iter()
            .map(|row| pr_lease::Row {
                id: row.id,
                updated_at: row.updated_at,
                body: row.body,
            })
            .collect::<Vec<_>>();
        let key = match role {
            StepRole::Curator => Role::Curator,
            StepRole::Judge => Role::Judge,
            StepRole::Doctor => Role::Doctor,
            StepRole::ChampionPromote | StepRole::ChampionMerge => Role::Champion,
            StepRole::Builder => unreachable!("Builder leases use issue records"),
        };
        let live =
            pr_lease::live_leases(&rows, key, Utc::now(), super::step_lease_owner::lease_ttl());
        match live.as_slice() {
            [] if rows.iter().any(|row| {
                matches!(
                    pr_lease::parse_record(&row.body),
                    Some(Record::Released(owner) | Record::Yield(owner))
                        if owner.host == host && owner.sweep == sweep
                            && owner.role == key && owner.step == Some(role)
                )
            }) =>
            {
                Ok(StepLease::Retired)
            }
            [] => Ok(StepLease::Gone),
            [lease] if lease.holder.host == host && lease.holder.sweep == sweep => {
                if lease.holder.step != Some(role) {
                    return Err("live old lease does not identify the recorded step role".into());
                }
                Ok(StepLease::Own {
                    role_row: Some(lease.id),
                })
            }
            leases
                if leases
                    .iter()
                    .any(|l| l.holder.host != host || l.holder.sweep != sweep) =>
            {
                Ok(StepLease::Peer)
            }
            _ => Err("ambiguous old role lease ownership".into()),
        }
    }

    fn roll_comment_rows(
        &self,
        number: u32,
        projection: &str,
    ) -> std::result::Result<String, String> {
        let path = format!("repos/{{owner}}/{{repo}}/issues/{number}/comments");
        let output = self
            .gh_read("roll.step_comments", ["api", &path, "--paginate", "--jq", projection])
            .map_err(|e| e.to_string())?
            .filter(|o| o.status.success())
            .ok_or("could not read roll step comment records")?;
        let raw = std::str::from_utf8(&output.stdout).map_err(|e| e.to_string())?;
        if raw.lines().any(|line| {
            !serde_json::from_str::<serde_json::Value>(line).is_ok_and(|v| v.is_object())
        }) {
            return Err("malformed roll step comment records".into());
        }
        let policy = crate::comment_trust::TrustPolicy::for_root(&self.config.workspace_root);
        Ok(String::from_utf8_lossy(&policy.trusted_ndjson(&output.stdout)).into_owned())
    }

    fn roll_step_notice(
        &self,
        item: &ManifestItem,
        number: u32,
        role: StepRole,
        notice: &RollRequeueNotice,
        progress: &RollStepRelease,
    ) -> std::result::Result<&'static str, String> {
        let outcome = progress.outcome();
        if outcome == "none" {
            return Ok(outcome);
        }
        // A trusted marker makes a lost POST response safe to reconcile.
        let marker = format!(
            "<!-- loom:roll-step-release manifest={} item={} role={} -->",
            notice.manifest_id,
            item.id,
            role.as_str()
        );
        let projection = format!(
            r#".[] | select(.body | startswith("<!-- loom:roll-step-release ")) | {{body, {}}}"#,
            crate::comment_trust::records::AUTHOR_JQ,
        );
        let found = |rows: &str| {
            rows.lines().any(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|row| {
                        row["body"]
                            .as_str()
                            .map(|body| body.lines().next() == Some(marker.as_str()))
                    })
                    .unwrap_or(false)
            })
        };
        if found(&self.roll_comment_rows(number, &projection)?) {
            return Ok(outcome);
        }
        let mut actions = Vec::new();
        if let Some(claim) = role.claim_label().filter(|_| progress.claim_removed) {
            actions.push(format!("Removed `{claim}`."));
        }
        if let Some(ready) = role.restore_label().filter(|_| progress.queue_restored) {
            actions.push(format!("Restored `{ready}`."));
        }
        if progress.lease_released {
            actions.push("Released this step's lease.".into());
        }
        let body =
            format!(
            "{marker}\n\nThe {role} step was interrupted while rolling from `{}` (`{}`). {}\n\n{}",
            notice.from_version, notice.reason, actions.join(" "), ROLL_REQUEUE_COMMENT_MARKER,
        );
        let number_arg = number.to_string();
        let kind = if role.acts_on_pr() { "pr" } else { "issue" };
        let mut args = vec![kind, "comment", &number_arg, "--body", &body];
        let repo_flag = crate::claim_reconciliation::gh_call::loom_repo_flag();
        args.extend(repo_flag.iter().map(String::as_str));
        let result = self.gh_write("roll.step_comment", args);
        if matches!(result, Ok(Some(output)) if output.status.success())
            || found(&self.roll_comment_rows(number, &projection)?)
        {
            Ok(outcome)
        } else {
            Err("could not confirm the roll step release notice".into())
        }
    }
}

/// Marker on every roll-requeue comment, so tooling can find them.
pub(crate) const ROLL_REQUEUE_COMMENT_MARKER: &str = "<!-- loom:roll-requeue -->";

/// What a roll-requeue comment says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RollRequeueNotice {
    /// The version the daemon is rolling from.
    pub(crate) from_version: String,
    /// The version it is rolling to (`None` for a source rebuild with no
    /// release version).
    pub(crate) to_version: Option<String>,
    /// The sweep checkpoint phase the agent had reached, when one exists.
    pub(crate) phase: Option<String>,
    /// Seconds since the agent's session first started, when known.
    pub(crate) agent_age_secs: Option<u64>,
    /// The closed-set reason (design §9).
    pub(crate) reason: String,
    /// The worktree path, when the item has one.
    pub(crate) worktree: Option<String>,
    /// Whether that worktree has uncommitted edits (`None` = not probed).
    pub(crate) worktree_dirty: Option<bool>,
    /// The pause manifest this requeue is recorded in.
    pub(crate) manifest_id: String,
}

impl RollRequeueNotice {
    /// The comment body.
    #[must_use]
    pub(crate) fn comment_body(&self) -> String {
        let to = self.to_version.as_deref().unwrap_or("a rebuilt binary");
        let phase = self.phase.as_deref().unwrap_or("unknown (no checkpoint)");
        let age = self
            .agent_age_secs
            .map_or_else(|| "unknown".to_string(), |s| format!("{s}s"));
        let meaning = match self.reason.as_str() {
            "young-agent-reset" => {
                "the agent had been running for less than the minimum resumable age, so it was \
                 stopped and reset rather than paused"
            }
            "pause-budget-missed" => {
                "the agent did not reach a safe point (a moment with no tool call running) \
                 before the pause budget ran out"
            }
            "session-not-resumable" => "no resumable session was recorded for the agent",
            // The reasons below are the resuming side's (H5, #10832): the
            // agent was paused, and could not be resumed after the restart.
            "manifest-stale" => {
                "the daemon came back too long after the pause to resume safely: the claim's \
                 lease could have aged out, so the paused agent was not resumed"
            }
            "lease-lost" => "the claim's lease was no longer this host's after the restart",
            "issue-parked" => "the issue was parked while its agent was paused",
            "worktree-changed" => {
                "the agent's worktree was missing, on another branch or at another commit after \
                 the restart"
            }
            "session-store-unavailable" => {
                "the agent's saved session could not be found after the restart"
            }
            "session-resume-failed" => "the agent's saved session did not start again",
            "resume-attempts-exhausted" => {
                "the session had already been resumed across three rolls"
            }
            "session-down" => "the agent's session container refused the resumed session",
            "role-disabled" => "the role is no longer enabled for this repository",
            "resume-timeout" => "the resume budget ran out before this agent was reached",
            r if r.starts_with("guard-refused:") => {
                "a dispatch guard refused to resume the agent after the restart"
            }
            _ => "the agent could not be paused for the roll",
        };
        // H4 requeues instead of pausing; H5 requeues what it could not resume.
        let did = if matches!(
            self.reason.as_str(),
            "young-agent-reset" | "pause-budget-missed" | "session-not-resumable"
        ) {
            "requeued this item instead of pausing it"
        } else {
            "paused this item, could not resume it afterwards, and requeued it"
        };
        let worktree = match (&self.worktree, self.worktree_dirty) {
            (Some(path), Some(true)) => format!(
                "A worktree with **uncommitted edits** remains on the host at `{path}`; the next \
                 dispatch reuses it."
            ),
            (Some(path), Some(false)) => {
                format!("The worktree at `{path}` has no uncommitted edits.")
            }
            (Some(path), None) => format!("The worktree at `{path}` was not probed."),
            (None, _) => "No worktree was recorded for it.".to_string(),
        };
        format!(
            "{ROLL_REQUEUE_COMMENT_MARKER}\nThe Loom daemon on this host restarted for a version \
             roll (`{from}` → `{to}`) and {did}.\n\n\
             - **Reason:** `{reason}` — {meaning}.\n\
             - **Phase reached:** {phase}\n\
             - **Agent age:** {age}\n\
             - **Worktree:** {worktree}\n\
             - **Pause manifest:** `{manifest}`\n\nThe claim was released; the item is back in the \
             queue and its sweep checkpoint is kept, so a later dispatch skips the phases already \
             finished.",
            from = self.from_version,
            reason = self.reason,
            manifest = self.manifest_id,
        )
    }
}

/// What the registry knows about one issue sweep's on-disk state, for the
/// pause manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RollItemFacts {
    pub(crate) checkpoint_phase: Option<String>,
    pub(crate) worktree: Option<PathBuf>,
    pub(crate) worktree_dirty: Option<bool>,
    pub(crate) branch: Option<String>,
    pub(crate) head: Option<String>,
}

pub(super) fn git_line(worktree: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(worktree).args(args);
    let out = reaper::output_with_timeout(cmd, reaper::reap_gh_timeout()).ok()??;
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !line.is_empty()).then_some(line)
}

impl SweepRegistry {
    /// Requeue `issue` for a roll: restore the label (state- and park-checked)
    /// and post the one explanatory comment.
    ///
    /// # Errors
    /// When the label restore fails, or the comment could not be posted. The
    /// caller leaves the item `planned` so the next start finishes it.
    ///
    /// A `loom:frozen` issue gets neither (#11743): the restore's own label
    /// read finds it, writes nothing, and the comment is skipped. That read
    /// fails open like every release path (a failed read restores and
    /// comments as before), and a frozen skip is `Ok`, so the item is not
    /// retried at the next start.
    pub(crate) fn requeue_for_roll(&self, issue: u32, notice: &RollRequeueNotice) -> Result<()> {
        let released = self.restore_label_to_ready_checked(issue)?;
        // #11852: an issue held at `loom:building` by its open PR is not
        // requeued either, so it gets no requeue comment.
        if self.config.skip_label_flip || released != restore_to_ready::ClaimRelease::Released {
            return Ok(());
        }
        let body = notice.comment_body();
        let issue_arg = issue.to_string();
        let mut comment = vec!["issue", "comment", &issue_arg, "--body", &body];
        let repo_flag = crate::claim_reconciliation::gh_call::loom_repo_flag();
        comment.extend(repo_flag.iter().map(String::as_str));
        match self.gh_write("roll.requeue_comment", comment) {
            Ok(Some(output)) if output.status.success() => Ok(()),
            Ok(Some(output)) => Err(anyhow::anyhow!(
                "roll-requeue comment for #{issue} exited {:?}: {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            )),
            Ok(None) => Err(anyhow::anyhow!(
                "roll-requeue comment for #{issue} exceeded {}s",
                reaper::reap_gh_timeout().as_secs()
            )),
            Err(e) => Err(anyhow::anyhow!("roll-requeue comment for #{issue} failed: {e}")),
        }
    }

    /// The checkpoint phase and worktree state of `issue`'s sweep. Read-only;
    /// every probe degrades to `None`.
    pub(crate) fn roll_item_facts(&self, issue: u32) -> RollItemFacts {
        let checkpoint = self
            .config
            .checkpoint_dir()
            .join(format!("issue-{issue}.json"));
        let worktree = self.worktree_path(issue);
        if !worktree.is_dir() {
            return RollItemFacts {
                checkpoint_phase: reaper::read_checkpoint_phase(&checkpoint),
                ..RollItemFacts::default()
            };
        }
        RollItemFacts {
            checkpoint_phase: reaper::read_checkpoint_phase(&checkpoint),
            worktree_dirty: Some(self.worktree_dirty(issue)),
            branch: git_line(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"]),
            head: git_line(&worktree, &["rev-parse", "HEAD"]),
            worktree: Some(worktree),
        }
    }

    /// A registry with this one's configuration and none of its state, for
    /// forge writes that must not run under the live registry's mutex
    /// ([`Self::requeue_for_roll`] reads only the configuration). It tracks no
    /// sweeps, holds no children and has no event bus.
    pub(crate) fn detached_for_forge_writes(&self) -> SweepRegistry {
        SweepRegistry::new(self.config.clone())
    }

    /// What a one-shot lease refresh needs, captured under the registry lock so
    /// the refresh itself ([`refresh_lease_once`]) can run without it.
    pub(crate) fn lease_refresh_identity(&self) -> (PathBuf, String) {
        (self.config.workspace_root.clone(), self.published_host_id())
    }
}

/// Refresh `issue`'s lease record once (design §7 H4 step 6), through the same
/// helper the dispatch-time renewal loop uses, targeted at this host's own
/// record (`--host`/`--sweep-id`, #6485). Stopping an agent stops its renewal
/// loop, so this is what gives the next start a full lease TTL to resume in.
///
/// # Errors
/// When the helper is missing, fails, or runs past `timeout`.
pub(crate) fn refresh_lease_once(
    workspace_root: &Path,
    host: &str,
    issue: u32,
    sweep_id: &str,
    timeout: Duration,
) -> std::result::Result<(), String> {
    let script = workspace_root.join(dispatch::LEASE_RENEW_SCRIPT_REL);
    if !script.is_file() {
        return Err(format!("no {} in this workspace", dispatch::LEASE_RENEW_SCRIPT_REL));
    }
    let mut cmd = Command::new(&script);
    cmd.arg("renew-once")
        .arg(issue.to_string())
        .arg("--host")
        .arg(host)
        .arg("--sweep-id")
        .arg(sweep_id)
        .current_dir(workspace_root)
        .stdin(Stdio::null());
    // Vet and perform the lease write under the root owner's credential, not the
    // daemon's process-global one (#5401): on a cross-owner root the latter is
    // refused by the write-scope probe. The recovery variant covers a roll that
    // runs soon after a daemon start, before the root is registered (#6722).
    crate::credential_preflight::apply_gh_config_for_root_with_recovery(&mut cmd, workspace_root);
    match reaper::output_with_timeout(cmd, timeout) {
        Ok(Some(out)) if out.status.success() => Ok(()),
        Ok(Some(out)) => Err(format!(
            "renew-once exited {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Ok(None) => Err(format!("renew-once exceeded {}s", timeout.as_secs())),
        Err(e) => Err(format!("renew-once could not run: {e}")),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A stub `renew-once` helper that records the `GH_CONFIG_DIR` it ran under;
    /// returns that recorded value (`""` when the child inherited none).
    fn gh_config_dir_seen_by_refresh(root: &Path) -> String {
        let script = root.join(dispatch::LEASE_RENEW_SCRIPT_REL);
        std::fs::create_dir_all(script.parent().unwrap()).unwrap();
        let out = root.join("seen");
        std::fs::write(
            &script,
            format!(
                "#!/usr/bin/env bash\nprintf '%s' \"${{GH_CONFIG_DIR-}}\" > \"{}\"\n",
                out.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        refresh_lease_once(root, "h", 7, "sweep-7", Duration::from_secs(10)).unwrap();
        std::fs::read_to_string(out).unwrap()
    }

    /// The `renew-once` child runs under the root owner's `GH_CONFIG_DIR` for a
    /// registered cross-owner root, and inherits the process one otherwise.
    #[test]
    #[serial_test::serial]
    fn lease_refresh_child_carries_the_root_owners_gh_config_dir() {
        crate::credential_preflight::clear_owner_root_registry();
        let dir = tempfile::tempdir().unwrap();
        let cross = dir.path().join("cross");
        let same = dir.path().join("same");
        let owner_dir = dir.path().join("gh-config-by-owner/2AMLogic");
        // The registry key canonicalizes an existing root (macOS `/var` ->
        // `/private/var`) but falls back to the raw path for a missing one, so
        // create the roots first to keep the registered and looked-up keys equal.
        std::fs::create_dir_all(&cross).unwrap();
        std::fs::create_dir_all(&same).unwrap();
        crate::credential_preflight::register_root_gh_config_dir(&cross, &owner_dir);

        assert_eq!(gh_config_dir_seen_by_refresh(&cross), owner_dir.display().to_string());
        let inherited = std::env::var("GH_CONFIG_DIR").unwrap_or_default();
        assert_eq!(gh_config_dir_seen_by_refresh(&same), inherited);

        crate::credential_preflight::clear_owner_root_registry();
    }

    fn notice(reason: &str) -> RollRequeueNotice {
        RollRequeueNotice {
            from_version: "0.19.887".to_string(),
            to_version: Some("0.19.900".to_string()),
            phase: Some("builder".to_string()),
            agent_age_secs: Some(42),
            reason: reason.to_string(),
            worktree: Some("/r/.loom/worktrees/issue-7".to_string()),
            worktree_dirty: Some(true),
            manifest_id: "rp-1".to_string(),
        }
    }

    #[test]
    fn the_comment_names_the_roll_phase_age_reason_and_worktree() {
        let body = notice("young-agent-reset").comment_body();
        for needle in [
            ROLL_REQUEUE_COMMENT_MARKER,
            "`0.19.887` → `0.19.900`",
            "`young-agent-reset`",
            "builder",
            "42s",
            "uncommitted edits",
            "/r/.loom/worktrees/issue-7",
            "rp-1",
        ] {
            assert!(body.contains(needle), "missing {needle:?} in: {body}");
        }
        let clean = RollRequeueNotice {
            worktree_dirty: Some(false),
            ..notice("pause-budget-missed")
        }
        .comment_body();
        assert!(clean.contains("no uncommitted edits"), "{clean}");
        assert!(clean.contains("pause budget"), "{clean}");
    }

    /// One requeue restores the label and posts exactly one comment.
    #[test]
    fn a_requeue_restores_the_label_and_posts_one_comment() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("gh.log");
        let gh = dir.path().join("gh");
        std::fs::write(
            &gh,
            format!(
                "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\n\
                 if [ \"$1\" = issue ] && [ \"$2\" = view ]; then echo false; fi\nexit 0\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.gh_bin = Some(gh);
        let registry = SweepRegistry::new(config);
        registry
            .requeue_for_roll(7, &notice("pause-budget-missed"))
            .unwrap();
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(calls.matches("issue comment 7").count(), 1, "exactly one comment: {calls}");
        assert!(calls.contains("loom:issue"), "the label was restored: {calls}");
        assert!(calls.contains("pause-budget-missed"), "{calls}");
    }

    /// #11743: a `loom:frozen` issue is requeued with zero writes — no label
    /// edit and no comment — and the requeue still reports success.
    #[test]
    #[serial_test::serial]
    fn a_frozen_issue_is_requeued_without_any_write() {
        std::env::set_var("LOOM_REPO", "rjwalters/loom");
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("gh.log");
        let gh = dir.path().join("gh");
        std::fs::write(
            &gh,
            format!(
                "#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> \"{}\"\n\
                 case \"$*\" in api\\ repos/*/issues/7\\ --jq*) \
                 echo '{{\"state\":\"open\",\"labels\":[\"loom:building\",\"loom:frozen\"]}}' ;; \
                 esac\nexit 0\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.gh_bin = Some(gh);
        config.skip_label_flip = false;
        let registry = SweepRegistry::new(config);
        registry
            .requeue_for_roll(7, &notice("pause-budget-missed"))
            .unwrap();
        std::env::remove_var("LOOM_REPO");
        let calls = std::fs::read_to_string(&log).unwrap();
        assert!(calls.contains("/issues/7 --jq"), "the restore read the labels: {calls}");
        assert!(!calls.contains("issue edit"), "{calls}");
        assert!(!calls.contains("issue comment"), "{calls}");
    }
}
