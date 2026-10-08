//! What the H4 pause needs from the host, behind a trait (issue #10831).
//!
//! [`super::run_h4`] decides and records; everything that touches a process,
//! the sweep registry or the forge goes through [`PauseHost`]. The production
//! implementation is [`DaemonPauseHost`]; the tests drive the same
//! orchestrator with fake agents.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::teardown::{self, TeardownReport, TreeSpec};
use crate::auto_update::pause_manifest::{ItemKind, ResumeHandle, Runtime, WorktreeRecord};
use crate::event_bus::EventBus;
use crate::roll_pause;
use crate::sweep_registry::roll_requeue::RollRequeueNotice;
use crate::workspace_pool::WorkspacePool;

/// One in-flight agent as the snapshot found it (design §7 H4 step 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The manifest item id: the sweep id, or a role run's synthetic id.
    pub id: String,
    pub kind: ItemKind,
    /// The owning workspace root.
    pub repo: PathBuf,
    /// The agent's pause state dir. `None` when the agent was dispatched
    /// without an item id (before #10830), so it cannot be asked to pause.
    pub pause_dir: Option<PathBuf>,
    pub issue: Option<u32>,
    pub pid: Option<u32>,
    pub pgid: Option<u32>,
    /// The scope unit recorded at dispatch. Not proof the scope exists.
    pub scope_unit: Option<String>,
    /// First start of the session (the 5-minute rule's clock).
    pub agent_started_at: Option<DateTime<Utc>>,
    /// Start of this process.
    pub run_started_at: Option<DateTime<Utc>>,
    pub resume_handle: Option<ResumeHandle>,
    pub worktree: Option<WorktreeRecord>,
    pub checkpoint_phase: Option<String>,
    pub log_path: Option<String>,
    pub role: Option<String>,
    pub timeout_remaining_secs: Option<u64>,
    pub holds_issue_creation_mutex: bool,
}

impl Candidate {
    /// Seconds since the session first started, at `now`. `None` when the
    /// start is unknown **or in the future** (clock skew): a start time the
    /// clock contradicts says nothing about the agent's age, so it must not
    /// read as "just started" and reset a long-running agent.
    #[must_use]
    pub fn agent_age_secs(&self, now: DateTime<Utc>) -> Option<u64> {
        let started = self.agent_started_at?;
        u64::try_from((now - started).num_seconds()).ok()
    }

    /// The process-tree identity the teardown works from.
    #[must_use]
    pub fn tree_spec(&self) -> TreeSpec {
        TreeSpec {
            pid: self.pid,
            pgid: self.pgid,
            scope_unit: self.scope_unit.clone(),
            worktree: self.worktree.as_ref().map(|w| PathBuf::from(&w.path)),
        }
    }
}

/// The host side of an H4 pause. `Send + Sync` because forge writes run on
/// worker threads the orchestrator can stop waiting for.
pub trait PauseHost: Send + Sync {
    /// Dispatches still `Pending` (spawn in progress) across every root.
    fn pending_dispatches(&self) -> usize;
    /// Every in-flight agent: sweeps across all managed roots, plus role runs.
    fn snapshot(&self) -> Vec<Candidate>;
    /// Take `c` over from the sweep reaper (or hand it back), so a tree this
    /// pause stops keeps its lock, journal entry and checkpoint.
    fn hold(&self, c: &Candidate, held: bool);
    /// Whether `c`'s agent process is still running.
    fn is_alive(&self, c: &Candidate) -> bool;
    /// Stop `c`'s whole process tree.
    fn teardown(&self, c: &Candidate) -> TeardownReport;
    /// Refresh `c`'s lease record once.
    ///
    /// # Errors
    /// When the refresh could not be done; the pause continues.
    fn refresh_lease(&self, c: &Candidate, timeout: Duration) -> Result<(), String>;
    /// Do `c`'s requeue forge writes (label + comment).
    ///
    /// # Errors
    /// When a write failed; the item stays `planned` for the next start.
    fn requeue(&self, c: &Candidate, notice: &RollRequeueNotice) -> Result<(), String>;
    /// Publish an event on the daemon's bus.
    fn emit(&self, topic: &str, payload: serde_json::Value);
}

/// The production [`PauseHost`]: the workspace pool's sweep registries, the
/// role runner's live runs, real process trees and the real forge.
pub struct DaemonPauseHost {
    pool: Arc<WorkspacePool>,
    fallback_root: PathBuf,
    bus: Arc<EventBus>,
}

impl DaemonPauseHost {
    #[must_use]
    pub fn new(pool: Arc<WorkspacePool>, fallback_root: PathBuf, bus: Arc<EventBus>) -> Self {
        Self {
            pool,
            fallback_root,
            bus,
        }
    }

    fn roots(&self) -> Vec<PathBuf> {
        crate::workspace_registry::WorkspaceRegistry::load_default()
            .unwrap_or_default()
            .effective_roots(&self.fallback_root)
    }
}

fn parse_time(s: Option<&str>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s?)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// The manifest form of a lock's resume handle.
fn manifest_handle(h: &crate::sweep_registry::resume_handle::ResumeHandle) -> ResumeHandle {
    ResumeHandle {
        runtime: Runtime::from(h.runtime.clone()),
        session_id: h.session_id.clone(),
        session_store: h.session_store.clone(),
        account: h.account.clone(),
        model: h.model.clone(),
        effort: h.effort.clone(),
        cwd: h.cwd.clone(),
        container: h
            .container
            .as_ref()
            .map(|name| serde_json::json!({ "name": name, "account": h.account })),
        sandbox: h.sandbox.clone(),
        resume_count: h.resume_count,
        resume_of: h.resume_of.clone(),
    }
}

/// A role run's candidate. `holders` are the roles holding an issue-creation
/// mutex right now.
fn role_candidate(run: &roll_pause::live_runs::LiveRun, holders: &[String]) -> Candidate {
    let item_dir = roll_pause::item_dir(&run.pause_root, &run.item_id);
    let mut handle = ResumeHandle {
        runtime: Runtime::from(run.runtime.clone()),
        session_id: run.claude_session_id.clone(),
        session_store: None,
        account: None,
        model: run.model.clone(),
        effort: None,
        cwd: Some(run.root.display().to_string()),
        container: None,
        sandbox: None,
        resume_count: 0,
        resume_of: None,
    };
    // A Codex role run's session id is captured after launch.
    if let Some(captured) = roll_pause::resume::read_handle(&item_dir.join(roll_pause::HANDLE_FILE))
    {
        if handle.session_id.is_none() && roll_pause::resume::valid_session_id(&captured.session_id)
        {
            handle.session_id = Some(captured.session_id.clone());
        }
        handle.session_store = captured.session_store.clone();
        handle.account = captured.account.clone();
        handle.sandbox = captured.sandbox.clone();
        handle.container = captured
            .container
            .as_ref()
            .map(|name| serde_json::json!({ "name": name, "account": captured.account }));
    }
    Candidate {
        id: run.item_id.clone(),
        kind: ItemKind::RoleRun,
        repo: run.root.clone(),
        pause_dir: Some(item_dir),
        issue: None,
        pid: Some(run.pid),
        // The role child is spawned with `process_group(0)`: it leads its group.
        pgid: Some(run.pid),
        scope_unit: run.scope_unit.clone(),
        agent_started_at: Some(run.started_at),
        run_started_at: Some(run.started_at),
        resume_handle: Some(handle),
        worktree: None,
        checkpoint_phase: None,
        log_path: None,
        role: Some(run.role.clone()),
        timeout_remaining_secs: Some(run.timeout_remaining_secs(std::time::Instant::now())),
        holds_issue_creation_mutex: holders.iter().any(|r| r.eq_ignore_ascii_case(&run.role)),
    }
}

/// Every non-terminal sweep of one registry as a candidate: the registry entry
/// (pid, group, log, start) joined with its claim lock's pause-and-roll
/// identity (item id, scope unit, first start, resume handle) and the on-disk
/// facts the manifest records (checkpoint phase, worktree state).
pub(crate) fn sweep_candidates(
    sr: &crate::sweep_registry::SweepRegistry,
    root: &Path,
) -> Vec<Candidate> {
    let locks: HashMap<String, _> = sr
        .in_flight_snapshot()
        .into_iter()
        .map(|a| (a.sweep_id.clone(), a))
        .collect();
    let pause_root = roll_pause::default_pause_root(root);
    sr.list(None)
        .into_iter()
        .filter(|i| !i.state.is_terminal())
        .map(|info| {
            let issue = match &info.kind {
                crate::types::SweepKind::Issue(n) => Some(*n),
                crate::types::SweepKind::PrSet(_) => None,
            };
            let agent = locks.get(&info.sweep_id);
            let facts = issue.map(|n| sr.roll_item_facts(n)).unwrap_or_default();
            // Only a sweep dispatched with an item id (#10830) has a pause dir
            // its hook reads; the id is the sweep id.
            let pause_dir = agent
                .and_then(|a| a.item_id.as_deref())
                .filter(|id| roll_pause::valid_item_id(id))
                .map(|id| roll_pause::item_dir(&pause_root, id));
            Candidate {
                id: info.sweep_id.clone(),
                kind: ItemKind::Sweep,
                repo: root.to_path_buf(),
                pause_dir,
                issue,
                pid: Some(info.pid),
                pgid: info.pgid.or(agent.and_then(|a| a.pgid)),
                scope_unit: agent.and_then(|a| a.scope_unit.clone()),
                agent_started_at: agent
                    .and_then(|a| parse_time(a.agent_started_at.as_deref()))
                    .or(Some(info.started_at)),
                run_started_at: Some(info.started_at),
                resume_handle: agent
                    .and_then(|a| a.resume_handle.as_ref())
                    .map(manifest_handle),
                worktree: facts.worktree.as_ref().map(|path| WorktreeRecord {
                    path: path.display().to_string(),
                    branch: facts.branch.clone(),
                    head: facts.head.clone(),
                    dirty: facts.worktree_dirty,
                }),
                checkpoint_phase: facts.checkpoint_phase.clone(),
                log_path: Some(info.log_path.display().to_string()),
                role: None,
                timeout_remaining_secs: None,
                holds_issue_creation_mutex: false,
            }
        })
        .collect()
}

impl PauseHost for DaemonPauseHost {
    fn pending_dispatches(&self) -> usize {
        let mut pending = 0;
        for root in self.roots() {
            let registry = self.pool.get_or_provision(&root);
            let sr = registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending += sr.list(Some(&crate::types::SweepState::Pending)).len();
        }
        pending
    }

    fn snapshot(&self) -> Vec<Candidate> {
        let mut out = Vec::new();
        for root in self.roots() {
            let registry = self.pool.get_or_provision(&root);
            let sr = registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            out.extend(sweep_candidates(&sr, &root));
        }
        let holders: Vec<String> = crate::issue_creation_mutex::holder_snapshot()
            .iter()
            .map(|t| t.role.to_string())
            .collect();
        out.extend(
            roll_pause::live_runs::snapshot()
                .iter()
                .map(|run| role_candidate(run, &holders)),
        );
        out
    }

    fn hold(&self, c: &Candidate, held: bool) {
        if c.kind == ItemKind::Sweep {
            if held {
                roll_pause::hold::hold(&c.id);
            } else {
                roll_pause::hold::release(&c.id);
            }
        }
    }

    fn is_alive(&self, c: &Candidate) -> bool {
        c.pid.is_some_and(teardown::pid_running)
    }

    fn teardown(&self, c: &Candidate) -> TeardownReport {
        teardown::teardown_tree(&c.tree_spec(), teardown::TERM_GRACE)
    }

    fn refresh_lease(&self, c: &Candidate, timeout: Duration) -> Result<(), String> {
        let Some(issue) = c.issue else {
            return Ok(()); // a role run or PR-set sweep has no issue lease
        };
        let (root, host) = {
            let registry = self.pool.get_or_provision(&c.repo);
            let sr = registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            sr.lease_refresh_identity()
        };
        crate::sweep_registry::roll_requeue::refresh_lease_once(&root, &host, issue, &c.id, timeout)
    }

    fn requeue(&self, c: &Candidate, notice: &RollRequeueNotice) -> Result<(), String> {
        let Some(issue) = c.issue else {
            // A role run's claim breadcrumb arrives with the resume side
            // (#10832); until then its own staleness rule releases any claim
            // label it took (design §9). The record is the event and manifest.
            return Ok(());
        };
        let registry = self.pool.get_or_provision(&c.repo);
        let sr = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sr.requeue_for_roll(issue, notice)
            .map_err(|e| format!("{e:#}"))
    }

    fn emit(&self, topic: &str, payload: serde_json::Value) {
        let _ = self.bus.publish_generic(topic, payload);
    }
}

/// The repo a candidate belongs to, as the manifest spells it.
#[must_use]
pub fn repo_string(root: &Path) -> String {
    root.display().to_string()
}
