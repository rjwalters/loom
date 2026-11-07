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
