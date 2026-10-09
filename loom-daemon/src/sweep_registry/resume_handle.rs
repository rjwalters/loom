//! Resume handles for pause-and-roll (issue #10830; design
//! `docs/design/daemon-roll-pause-resume.md` §3, §5, §10).
//!
//! At dispatch, every daemon-dispatched agent is given:
//!
//! * an **item id** ([`roll_pause::ITEM_ENV`]), the only thing that arms the
//!   pause hook, so in-session and attended agents are never parked;
//! * its **pause state dir** ([`roll_pause::DIR_ENV`]);
//! * for Claude, a **pinned session id** generated here, before launch
//!   ([`resume::CLAUDE_SESSION_ENV`]); for Codex, a **handle file**
//!   ([`resume::HANDLE_FILE_ENV`]) that `spawn-codex.sh` fills in live;
//! * on Linux, the **systemd scope unit** name the spawn script will use
//!   ([`resume::SCOPE_UNIT_ENV`]), which is the teardown unit at H4.
//!
//! For an issue sweep the same facts are stamped into the claim lock's
//! `owner.json` (`scope_unit`, `agent_started_at`, `resume_handle`), with the
//! #4980/#8056/#9314 schema-evolution contract: all optional, absent on an
//! older `owner.json`, which still parses. [`SweepRegistry::in_flight_snapshot`]
//! reads them back, merging a Codex handle captured after launch, for the H4
//! classifier (#10831).
//!
//! A roll resume (H5, #10832) launches through the same path with a
//! [`RollResumeLaunch`]: the saved session id and resume prompt replace the
//! pinned id, and the new run's handle records its lineage (`resume_of`,
//! `resume_count`) and the session's first start.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::roll_pause::{self, resume};

use super::locks::LockOwner;
use super::SweepRegistry;

/// The resume handle recorded in `owner.json` (design §6 `resume_handle`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ResumeHandle {
    pub(crate) runtime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_store: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) container: Option<String>,
    /// The Codex sandbox mode of the original launch (#10831), merged from
    /// the live-captured handle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) sandbox: Option<String>,
    #[serde(default)]
    pub(crate) resume_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) resume_of: Option<String>,
    /// The sweep id the issue's lease record was published under, when it is
    /// not this run's own (#10832). A resumed run keeps the claim, and with it
    /// the claim's one lease record: it renews the record the original
    /// dispatch wrote instead of publishing a second one, which every
    /// co-occupancy check would read as two sweeps sharing the worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) lease_sweep_id: Option<String>,
}

/// What a roll resume (H5, #10832) relaunches: the saved session of a paused
/// agent, taken from its pause-manifest item. Carried on [`DispatchSession`]
/// so the resumed launch goes through the same spawn path as a fresh one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RollResumeLaunch {
    /// `claude` or `codex`.
    pub(crate) runtime: String,
    /// The runtime session to resume.
    pub(crate) session_id: String,
    /// The prompt the resumed session receives ([`resume::resume_prompt`]).
    pub(crate) prompt: String,
    /// The paused run's item id, recorded as the new run's `resume_of`.
    pub(crate) resume_of: String,
    /// Roll resumes of this session so far, this one included.
    pub(crate) resume_count: u32,
    /// The session's FIRST start, carried so the 5-minute rule keeps counting
    /// from it. `None` falls back to "now".
    pub(crate) agent_started_at: Option<String>,
    /// The session store recorded at the pause: for Codex, the account's
    /// `CODEX_HOME`, which the resume must pin.
    pub(crate) session_store: Option<String>,
    /// The account recorded at the pause.
    pub(crate) account: Option<String>,
    /// The session container the agent ran in, for a session-exec item.
    pub(crate) container: Option<String>,
    /// The Codex sandbox mode of the ORIGINAL launch. The resume asks for the
    /// same one rather than re-deriving it (see [`Self::requested_sandbox`]).
    pub(crate) sandbox: Option<String>,
    /// The sweep id the claim's lease record is published under (the first
    /// dispatch's), which the resumed run keeps renewing.
    pub(crate) lease_sweep_id: Option<String>,
}

impl RollResumeLaunch {
    /// The sandbox mode the resume asks `spawn-codex.sh` for
    /// (`LOOM_CODEX_SANDBOX`), or `None` to ask for nothing.
    ///
    /// The recorded mode is requested as recorded, with one exception. A
    /// session-exec item recorded `danger-full-access` because its container
    /// was the boundary: `session-exec posture` granted it, nothing asked for
    /// it. Asking for it now would turn a grant into a request, so the resume
    /// asks for nothing there and lets the posture check grant the same mode
    /// again, or refuse the launch (which requeues the item). The mode the
    /// resumed launch actually reports is compared with the recorded one
    /// afterwards (`roll_resume::sandbox_mismatch`).
    pub(crate) fn requested_sandbox(&self) -> Option<&str> {
        let mode = self.sandbox.as_deref()?;
        (self.container.is_none() || mode != "danger-full-access").then_some(mode)
    }
}

/// One dispatch's pause-and-roll identity, built before the child is spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DispatchSession {
    pub(crate) item_id: String,
    pub(crate) runtime: String,
    pub(crate) pause_root: PathBuf,
    pub(crate) claude_session_id: Option<String>,
    pub(crate) scope_unit: Option<String>,
    pub(crate) agent_started_at: String,
    pub(crate) cwd: PathBuf,
    /// Set when this launch resumes a paused session instead of starting one.
    pub(crate) resume: Option<RollResumeLaunch>,
}

impl DispatchSession {
    /// Name a fresh dispatch. `runtime` is the admitted runtime (`None` is the
    /// default `spawn-claude.sh` path). `None` when `item_id` is unusable as a
    /// directory name, which leaves the dispatch exactly as before #10830.
    pub(crate) fn new(item_id: &str, workspace_root: &Path, runtime: Option<&str>) -> Option<Self> {
        if !roll_pause::valid_item_id(item_id) {
            return None;
        }
        let runtime = runtime.unwrap_or("claude").to_string();
        let claude_session_id = (runtime == "claude").then(|| uuid::Uuid::new_v4().to_string());
        let scope_unit = cfg!(target_os = "linux")
            .then(|| format!("loom-agent-{}.scope", item_id.replace('.', "_")));
        Some(DispatchSession {
            item_id: item_id.to_string(),
            runtime,
            pause_root: roll_pause::default_pause_root(workspace_root),
            claude_session_id,
            scope_unit,
            agent_started_at: chrono::Utc::now().to_rfc3339(),
            cwd: workspace_root.to_path_buf(),
            resume: None,
        })
    }

    /// Name a launch that resumes `launch`'s saved session (#10832). The new
    /// run gets its own item id (and so its own pause state dir and scope
    /// unit); the session id, its first start and its lineage are carried.
    /// `None` when either id is unusable.
    pub(crate) fn resumed(
        item_id: &str,
        workspace_root: &Path,
        launch: &RollResumeLaunch,
    ) -> Option<Self> {
        if !resume::valid_session_id(&launch.session_id) {
            return None;
        }
        let mut session = Self::new(item_id, workspace_root, Some(&launch.runtime))?;
        // The session already exists: it is resumed, never pinned again.
        session.claude_session_id = None;
        if let Some(first_start) = &launch.agent_started_at {
            session.agent_started_at.clone_from(first_start);
        }
        session.resume = Some(launch.clone());
        Some(session)
    }

    /// This item's pause state dir.
    pub(crate) fn item_dir(&self) -> PathBuf {
        roll_pause::item_dir(&self.pause_root, &self.item_id)
    }

    /// Hand the identity to the child through its environment.
    pub(crate) fn apply_env(&self, cmd: &mut Command) {
        cmd.env(roll_pause::ITEM_ENV, &self.item_id)
            .env(roll_pause::DIR_ENV, &self.pause_root)
            .env(resume::HANDLE_FILE_ENV, self.item_dir().join(roll_pause::HANDLE_FILE));
        match &self.claude_session_id {
            Some(id) => cmd.env(resume::CLAUDE_SESSION_ENV, id),
            None => cmd.env_remove(resume::CLAUDE_SESSION_ENV),
        };
        if let Some(unit) = &self.scope_unit {
            cmd.env(resume::SCOPE_UNIT_ENV, unit);
        }
        let Some(launch) = &self.resume else {
            // A daemon-dispatched fresh launch is never a resume, whatever the
            // daemon's own environment holds.
            cmd.env_remove(resume::RESUME_SESSION_ENV)
                .env_remove(resume::RESUME_PROMPT_ENV);
            return;
        };
        cmd.env(resume::RESUME_SESSION_ENV, &launch.session_id)
            .env(resume::RESUME_PROMPT_ENV, &launch.prompt);
        if launch.runtime == "codex" {
            // The rollout lives in the paused session's own CODEX_HOME, and a
            // session-exec item in that account's container: pin both, and
            // ask for the sandbox mode the session was launched with.
            match &launch.session_store {
                Some(home) => cmd.env("LOOM_CODEX_HOME", home),
                None => cmd.env_remove("LOOM_CODEX_HOME"),
            };
            if let Some(account) = &launch.account {
                cmd.env("LOOM_ACCOUNT_NAME", account);
            }
            if launch.container.is_some() {
                cmd.env("LOOM_CODEX_SESSION_EXEC", "1");
            }
            match launch.requested_sandbox() {
                Some(mode) => cmd.env("LOOM_CODEX_SANDBOX", mode),
                None => cmd.env_remove("LOOM_CODEX_SANDBOX"),
            };
        }
    }

    /// The handle recorded at dispatch. A Codex session id is not known yet;
    /// [`SweepRegistry::in_flight_snapshot`] merges it from the handle file.
    pub(crate) fn handle(&self, model: Option<&str>, effort: Option<&str>) -> ResumeHandle {
        let fresh = ResumeHandle {
            runtime: self.runtime.clone(),
            session_id: self.claude_session_id.clone(),
            model: model.filter(|m| !m.is_empty()).map(str::to_string),
            effort: effort.filter(|e| !e.is_empty()).map(str::to_string),
            cwd: Some(self.cwd.display().to_string()),
            ..ResumeHandle::default()
        };
        match &self.resume {
            None => fresh,
            // A resumed run keeps its session and records where it came from,
            // so the next roll can resume it again and count the attempts.
            Some(launch) => ResumeHandle {
                session_id: Some(launch.session_id.clone()),
                session_store: launch.session_store.clone(),
                account: launch.account.clone(),
                container: launch.container.clone(),
                sandbox: launch.sandbox.clone(),
                resume_count: launch.resume_count,
                resume_of: Some(launch.resume_of.clone()),
                lease_sweep_id: launch.lease_sweep_id.clone(),
                ..fresh
            },
        }
    }

    /// Stamp the identity onto a lock owner record.
    pub(crate) fn stamp(&self, owner: &mut LockOwner, model: Option<&str>, effort: Option<&str>) {
        owner.item_id = Some(self.item_id.clone());
        owner.scope_unit.clone_from(&self.scope_unit);
        owner.agent_started_at = Some(self.agent_started_at.clone());
        owner.resume_handle = Some(self.handle(model, effort));
    }
}

/// One in-flight agent, as the H4 classifier (PR 2) sees it.
#[allow(dead_code)] // consumed by the H4 pause (#10831); exercised by tests here
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InFlightAgent {
    pub(crate) issue: u32,
    pub(crate) sweep_id: String,
    pub(crate) pid: u32,
    pub(crate) pgid: Option<u32>,
    pub(crate) item_id: Option<String>,
    pub(crate) scope_unit: Option<String>,
    pub(crate) agent_started_at: Option<String>,
    pub(crate) resume_handle: Option<ResumeHandle>,
}

#[allow(dead_code)] // consumed by the H4 pause (#10831)
impl InFlightAgent {
    /// The classifier input at `now`.
    pub(crate) fn classify_input(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> crate::auto_update::pause_classify::ClassifyInput {
        use crate::auto_update::pause_manifest::{ItemKind, Runtime};
        let age = self
            .agent_started_at
            .as_deref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .and_then(|t| u64::try_from((now - t.with_timezone(&chrono::Utc)).num_seconds()).ok());
        crate::auto_update::pause_classify::ClassifyInput {
            kind: ItemKind::Sweep,
            agent_age_secs: age,
            runtime: self
                .resume_handle
                .as_ref()
                .map(|h| Runtime::from(h.runtime.clone())),
            session_id: self
                .resume_handle
                .as_ref()
                .and_then(|h| h.session_id.clone()),
        }
    }
}

/// Fill a dispatch-time handle with what the spawn script captured after
/// launch (the Codex session id, its store, account and container).
pub(crate) fn merge_captured(handle: &mut ResumeHandle, captured: &resume::CapturedHandle) {
    if handle.session_id.is_none() && resume::valid_session_id(&captured.session_id) {
        handle.session_id = Some(captured.session_id.clone());
    }
    for (slot, value) in [
        (&mut handle.session_store, &captured.session_store),
        (&mut handle.account, &captured.account),
        (&mut handle.container, &captured.container),
        (&mut handle.sandbox, &captured.sandbox),
    ] {
        if slot.is_none() {
            slot.clone_from(value);
        }
    }
}

/// Parse one `owner.json` into an in-flight agent, merging a captured handle.
pub(crate) fn agent_from_owner(raw: &str, pause_root: &Path) -> Option<InFlightAgent> {
    let owner: LockOwner = serde_json::from_str(raw).ok()?;
    let mut handle = owner.resume_handle.clone();
    if let (Some(h), Some(item)) = (handle.as_mut(), owner.item_id.as_deref()) {
        let file = roll_pause::item_dir(pause_root, item).join(roll_pause::HANDLE_FILE);
        if let Some(captured) = resume::read_handle(&file) {
            merge_captured(h, &captured);
        }
    }
    Some(InFlightAgent {
        issue: owner.issue,
        sweep_id: owner.sweep_id,
        pid: owner.owner_pid,
        pgid: owner.pgid,
        item_id: owner.item_id,
        scope_unit: owner.scope_unit,
        agent_started_at: owner.agent_started_at,
        resume_handle: handle,
    })
}

impl SweepRegistry {
    /// Stamp a dispatch's resume identity into its claim lock. Best-effort: a
    /// failure only costs the roll this agent's resume handle (it would then
    /// be requeued as `session-not-resumable`), never the dispatch.
    pub(crate) fn stamp_resume_handle_in_lock(
        &self,
        issue: u32,
        session: &DispatchSession,
        model: Option<&str>,
        effort: Option<&str>,
    ) {
        let path = self
            .config
            .locks_dir()
            .join(format!("issue-{issue}"))
            .join("owner.json");
        let result = std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|raw| Ok(serde_json::from_str::<LockOwner>(&raw)?))
            .and_then(|mut owner| {
                session.stamp(&mut owner, model, effort);
                std::fs::write(&path, serde_json::to_string_pretty(&owner)?)?;
                Ok(())
            });
        if let Err(e) = result {
            log::warn!(
                "issue #{issue}: could not record the resume handle in {}: {e:#}",
                path.display()
            );
        }
    }

    /// Every live claim lock of this registry as an in-flight agent, with its
    /// resume handle (and any live-captured Codex session id) merged in. The
    /// H4 classifier's input (PR 2); read-only.
    #[allow(dead_code)] // consumed by the H4 pause (#10831)
    pub(crate) fn in_flight_snapshot(&self) -> Vec<InFlightAgent> {
        let pause_root = roll_pause::default_pause_root(&self.config.workspace_root);
        let Ok(entries) = std::fs::read_dir(self.config.locks_dir()) else {
            return Vec::new();
        };
        let mut agents: Vec<InFlightAgent> = entries
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("issue-"))
            .filter_map(|e| std::fs::read_to_string(e.path().join("owner.json")).ok())
            .filter_map(|raw| agent_from_owner(&raw, &pause_root))
            .collect();
        agents.sort_by_key(|a| a.issue);
        agents
    }
}

#[cfg(test)]
#[path = "resume_handle_tests.rs"]
mod tests;
