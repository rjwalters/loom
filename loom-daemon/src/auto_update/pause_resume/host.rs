//! What H5 needs from the host, behind a trait (issue #10832).
//!
//! [`super::run_h5`] decides and records; everything that touches a process,
//! a registry, the role runner or the forge goes through [`ResumeHost`]. The
//! production implementation is [`DaemonResumeHost`]; the tests drive the same
//! orchestrator with a scripted host, and with real registries and real
//! process trees behind a fake `gh`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{PauseResumeStatus, REASON_SESSION_DOWN, REASON_STORE};
use crate::auto_update::pause_manifest::{ItemKind, ManifestItem, SafePointRecord};
use crate::auto_update::pause_roll::teardown::{self, TeardownReport, TreeSpec};
use crate::event_bus::EventBus;
use crate::ipc::{DrainState, ResumeHold};
use crate::role_runner::roll_resume::{self as role_resume, RoleResumeHandle, RoleResumeState};
use crate::role_runner::InProgressGuard;
use crate::roll_pause::{self, claim_breadcrumb, PauseRequest};
use crate::sweep_registry::resume_handle::RollResumeLaunch;
use crate::sweep_registry::roll_requeue::RollRequeueNotice;
use crate::sweep_registry::roll_resume::{
    self as sweep_resume, ResumeChild, RollResumeRefusal, RollResumeSpec, WorktreeExpectation,
    REASON_RESUME_FAILED,
};
use crate::sweep_registry::SweepRegistry;
use crate::workspace_pool::WorkspacePool;

/// The reaper-hold owner of a resumed sweep H5 has not confirmed yet.
pub(crate) const RESUME_HOLD_OWNER: &str = "h5-resume";

/// A relaunched item H5 is still confirming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launched {
    /// The resumed run's own item id (its sweep id, for a sweep).
    pub item_id: String,
    pub pid: Option<u32>,
    /// When it was launched.
    pub at: Instant,
    /// The per-issue log and this dispatch's header anchor, for a sweep.
    pub log_path: Option<PathBuf>,
    pub header_anchor: Option<String>,
}

/// How a relaunched item is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Liveness {
    /// Its process is running.
    Running,
    /// It ran to a clean end already.
    Finished,
    /// It ended without starting its session: requeue with `reason`.
    Died { reason: String, detail: String },
}

/// The host side of H5. `Send + Sync` because forge writes run on worker
/// threads the orchestrator can stop waiting for.
pub(crate) trait ResumeHost: Send + Sync {
    /// Make sure dispatch is held for H5, and say by whom. Called at every
    /// checkpoint: it places H5's own hold when nothing holds dispatch.
    fn hold_dispatch(&self) -> ResumeHold;
    /// One health sample: IPC answering, heartbeat fresh.
    ///
    /// # Errors
    /// What is unhealthy.
    fn health_sample(&self) -> Result<(), String>;
    /// The safe-point record on disk for an item H4 never marked `paused`,
    /// when it answers `request` (the pause that wrote the manifest).
    fn safe_point(&self, item: &ManifestItem, request: &PauseRequest) -> Option<SafePointRecord>;
    /// Reap whatever is still alive of `item`'s recorded process tree.
    fn reap_residue(&self, item: &ManifestItem) -> TeardownReport;
    /// Refresh `item`'s lease record once.
    ///
    /// # Errors
    /// When the refresh could not be done; H5 continues.
    fn refresh_lease(&self, item: &ManifestItem, timeout: Duration) -> Result<(), String>;
    /// The id of a live run already resuming `item` (a crash during H5).
    fn already_resumed(&self, item: &ManifestItem) -> Option<String>;
    /// The §4 cross-checks.
    ///
    /// # Errors
    /// The first check that fails, as a design §9 reason.
    fn check(
        &self,
        item: &ManifestItem,
        launch: &RollResumeLaunch,
    ) -> Result<(), RollResumeRefusal>;
    /// Relaunch `item` from its saved session. `wait` bounds how long the
    /// launch itself may take.
    ///
    /// # Errors
    /// With a design §9 reason.
    fn launch(
        &self,
        item: &ManifestItem,
        launch: &RollResumeLaunch,
        wait: Duration,
    ) -> Result<Launched, RollResumeRefusal>;
    /// How `launched` is doing.
    fn liveness(&self, item: &ManifestItem, launched: &Launched) -> Liveness;
    /// Undo a launch that did not start its session.
    fn abandon(&self, item: &ManifestItem, launched: &Launched);
    /// `item` is resumed as `new_item_id`: hand it back to ordinary
    /// supervision.
    fn settle(&self, manifest_id: &str, item: &ManifestItem, new_item_id: &str);
    /// Release `item`'s claim: with `forge`, the label and the comment first;
    /// always its lock and journal entry (the checkpoint is kept).
    ///
    /// # Errors
    /// When a forge write failed. Nothing local was released.
    fn requeue(
        &self,
        item: &ManifestItem,
        notice: &RollRequeueNotice,
        forge: bool,
    ) -> Result<(), String>;
    /// Hand `item` to ordinary restart recovery.
    fn recover(&self, manifest_id: &str, item: &ManifestItem);
    /// H5 is finished with `manifest_id`: lift the recovery suppression and
    /// the dispatch hold.
    fn finish(&self, manifest_id: &str, note: &str);
    /// Publish an event on the daemon's bus.
    fn emit(&self, topic: &str, payload: serde_json::Value);
    /// Publish the status `status --json` reports.
    fn publish(&self, status: &PauseResumeStatus);
}

/// The pause state dir of `item`.
fn pause_dir(item: &ManifestItem) -> PathBuf {
    roll_pause::item_dir(&roll_pause::default_pause_root(Path::new(&item.repo)), &item.id)
}

/// The sweep-registry resume spec of a sweep item. `None` for anything else.
pub(crate) fn sweep_spec(item: &ManifestItem, launch: &RollResumeLaunch) -> Option<RollResumeSpec> {
    let handle = item.resume_handle.as_ref()?;
    Some(RollResumeSpec {
        issue: item.issue?,
        old_sweep_id: item.id.clone(),
        launch: launch.clone(),
        model: handle.model.clone(),
        effort: handle.effort.clone(),
        worktree: item.worktree.as_ref().map(|w| WorktreeExpectation {
            path: PathBuf::from(&w.path),
            branch: w.branch.clone(),
            head: w.head.clone(),
        }),
    })
}

/// The recorded process tree of `item`, as PR 2's teardown takes it: the
/// recorded pid with its start time, its process group and its scope unit.
/// Nothing is seeded from the worktree (an operator shell or an attended
/// session there is not the agent's). The teardown itself checks, before any
/// signal, that the pid is still the recorded process ([`teardown::leader_state`]):
/// after a restart the number may belong to a stranger, and a forced H4 finish
/// records a kill it never verified.
pub(crate) fn residue_spec(item: &ManifestItem) -> TreeSpec {
    let observed = item
        .pid_started_at
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&chrono::Utc));
    TreeSpec {
        pid: item.pid,
        pid_started_at: observed,
        recorded_started_at: item.run_started_at,
        pgid: item.pgid,
        scope_unit: item.scope_unit.clone(),
    }
}

/// The session-store and session-container checks shared by sweeps and role
/// runs (design §4).
pub(crate) fn session_checks(
    launch: &RollResumeLaunch,
    cwd: &str,
    container_ready: impl Fn(&str, &str) -> Result<(), String>,
) -> Result<(), RollResumeRefusal> {
    roll_pause::resume::session_store_reachable(
        &launch.runtime,
        &launch.session_id,
        launch.session_store.as_deref(),
    )
    .map_err(|why| RollResumeRefusal::new(REASON_STORE, why))?;
    if let Some(container) = &launch.container {
        container_ready(container, cwd)
            .map_err(|why| RollResumeRefusal::new(REASON_SESSION_DOWN, why))?;
    }
    Ok(())
}

/// Whether the session container `name` is running with `cwd` mounted.
fn docker_container_ready(name: &str, cwd: &str) -> Result<(), String> {
    let mut cmd = std::process::Command::new(
        std::env::var("LOOM_CODEX_SESSION_DOCKER").unwrap_or_else(|_| "docker".to_string()),
    );
    cmd.args(["inspect", "--type", "container", name]);
    let out = crate::sweep_registry::reaper::output_with_timeout(cmd, Duration::from_secs(15))
        .map_err(|e| format!("docker inspect {name}: {e}"))?
        .ok_or_else(|| format!("docker inspect {name} timed out"))?;
    let state = serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .ok()
        .and_then(|v| v.as_array().and_then(|a| a.first().cloned()))
        .filter(|_| out.status.success())
        .ok_or_else(|| format!("session container {name} does not exist"))?;
    if state["State"]["Running"].as_bool() != Some(true) {
        return Err(format!("session container {name} is not running"));
    }
    if !crate::session_exec::posture::mounted(&state, cwd) {
        return Err(format!("session container {name} does not mount {cwd}"));
    }
    Ok(())
}

/// The sweep half of H5 over one registry: shared by the production host and
/// by the tests' real-registry host, so both drive the same code.
pub(crate) mod sweeps {
    use super::*;

    pub(crate) fn locked(
        registry: &Arc<Mutex<SweepRegistry>>,
    ) -> std::sync::MutexGuard<'_, SweepRegistry> {
        registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn refresh_lease(
        registry: &Arc<Mutex<SweepRegistry>>,
        item: &ManifestItem,
        timeout: Duration,
    ) -> Result<(), String> {
        let Some(issue) = item.issue else {
            return Ok(()); // a PR-set sweep has no issue lease
        };
        let Some((root, host)) = locked(registry).roll_resume_lease_identity() else {
            return Ok(());
        };
        let lease_id = item
            .resume_handle
            .as_ref()
            .and_then(|h| h.lease_sweep_id.as_deref())
            .unwrap_or(&item.id);
        crate::sweep_registry::roll_requeue::refresh_lease_once(
            &root, &host, issue, lease_id, timeout,
        )
    }

    pub(crate) fn already_resumed(
        registry: &Arc<Mutex<SweepRegistry>>,
        item: &ManifestItem,
    ) -> Option<String> {
        locked(registry).live_roll_resume_of(item.issue?, &item.id)
    }

    pub(crate) fn check(
        registry: &Arc<Mutex<SweepRegistry>>,
        item: &ManifestItem,
        launch: &RollResumeLaunch,
    ) -> Result<(), RollResumeRefusal> {
        let Some(spec) = sweep_spec(item, launch) else {
            return Err(RollResumeRefusal::new(super::super::REASON_PR_SET, "not an issue sweep"));
        };
        locked(registry).roll_resume_checks(&spec)
    }

    pub(crate) fn launch(
        registry: &Arc<Mutex<SweepRegistry>>,
        item: &ManifestItem,
        launch: &RollResumeLaunch,
    ) -> Result<Launched, RollResumeRefusal> {
        let Some(spec) = sweep_spec(item, launch) else {
            return Err(RollResumeRefusal::new(super::super::REASON_PR_SET, "not an issue sweep"));
        };
        let mut prepared = locked(registry).begin_roll_resume(&spec)?;
        // The account-selection poll runs without the registry lock (#6592).
        let (token, runtime, death) = crate::sweep_registry::poll_and_classify_spawned_child(
            &mut prepared.child,
            &prepared.log_path,
            &prepared.header_anchor,
        );
        let resumed = locked(registry).finish_roll_resume(prepared, token, runtime, death)?;
        // H5 confirms it before the sweep reaper may act on it.
        roll_pause::hold::hold(&resumed.sweep_id, RESUME_HOLD_OWNER);
        Ok(Launched {
            item_id: resumed.sweep_id,
            pid: Some(resumed.pid),
            at: Instant::now(),
            log_path: Some(resumed.log_path),
            header_anchor: Some(resumed.header_anchor),
        })
    }

    pub(crate) fn liveness(
        registry: &Arc<Mutex<SweepRegistry>>,
        item: &ManifestItem,
        launched: &Launched,
    ) -> Liveness {
        let child = locked(registry).roll_resume_child(&launched.item_id);
        sweep_liveness(child, launched, item)
    }

    pub(crate) fn abandon(registry: &Arc<Mutex<SweepRegistry>>, launched: &Launched) {
        locked(registry).abandon_roll_resume(&launched.item_id);
        roll_pause::hold::release(&launched.item_id, RESUME_HOLD_OWNER);
    }

    /// Release a sweep item's claim. With `forge`, the label restore and the
    /// comment first, but only while the claim is still this run's to give
    /// back ([`SweepRegistry::roll_claim_still_ours`]).
    pub(crate) fn requeue(
        registry: &Arc<Mutex<SweepRegistry>>,
        item: &ManifestItem,
        notice: &RollRequeueNotice,
        forge: bool,
    ) -> Result<(), String> {
        let mut sr = locked(registry);
        let Some(issue) = item.issue else {
            let released = sr.release_roll_prset(&item.id);
            log::info!("pause_resume: {}: released PR lock(s) {released:?}", item.id);
            return Ok(());
        };
        if forge {
            match sr.roll_claim_still_ours(issue, &item.id) {
                Ok(()) => sr
                    .requeue_for_roll(issue, notice)
                    .map_err(|e| format!("{e:#}"))?,
                Err(why) => log::warn!(
                    "pause_resume: {}: #{issue} is no longer this run's claim ({why}); nothing \
                     is written to the forge for it",
                    item.id
                ),
            }
        }
        sr.release_roll_item(issue, &[&item.id]);
        Ok(())
    }

    pub(crate) fn recover(registry: &Arc<Mutex<SweepRegistry>>, item: &ManifestItem) {
        let Some(issue) = item.issue else {
            return;
        };
        let only = HashSet::from([issue]);
        if let Err(e) = locked(registry).reconstruct_issues(Some(&only)) {
            log::warn!("pause_resume: {}: restart recovery failed: {e:#}", item.id);
        }
    }
}

/// The production [`ResumeHost`].
pub(crate) struct DaemonResumeHost {
    pool: Arc<WorkspacePool>,
    fallback_root: PathBuf,
    bus: Arc<EventBus>,
    drain: Arc<DrainState>,
    in_progress: InProgressGuard,
    started: Instant,
    /// Resumed role runs still being watched, by their new item id.
    roles: Mutex<HashMap<String, RoleResumeHandle>>,
}

impl DaemonResumeHost {
    pub(crate) fn new(
        pool: Arc<WorkspacePool>,
        fallback_root: PathBuf,
        bus: Arc<EventBus>,
        drain: Arc<DrainState>,
        in_progress: InProgressGuard,
    ) -> Self {
        Self {
            pool,
            fallback_root,
            bus,
            drain,
            in_progress,
            started: Instant::now(),
            roles: Mutex::new(HashMap::new()),
        }
    }

    fn registry(&self, item: &ManifestItem) -> Arc<Mutex<SweepRegistry>> {
        self.pool.get_or_provision(Path::new(&item.repo))
    }

    fn roles(&self) -> std::sync::MutexGuard<'_, HashMap<String, RoleResumeHandle>> {
        self.roles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// One IPC `Ping`/`Pong` round trip on the daemon's own socket.
fn ipc_answers() -> Result<(), String> {
    use std::io::{BufRead, BufReader, Write};
    let socket = std::env::var("LOOM_SOCKET_PATH").map_or_else(
        |_| dirs::home_dir().map(|h| h.join(".loom").join("loom-daemon.sock")),
        |p| Some(PathBuf::from(p)),
    );
    let Some(socket) = socket else {
        return Err("no IPC socket path resolves".to_string());
    };
    let timeout = Some(Duration::from_secs(5));
    let mut stream = std::os::unix::net::UnixStream::connect(&socket)
        .map_err(|e| format!("IPC socket {} does not accept: {e}", socket.display()))?;
    let _ = stream.set_read_timeout(timeout);
    let _ = stream.set_write_timeout(timeout);
    let request = serde_json::to_string(&crate::types::Request::Ping).map_err(|e| e.to_string())?;
    stream
        .write_all(format!("{request}\n").as_bytes())
        .map_err(|e| format!("IPC ping could not be sent: {e}"))?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|e| format!("IPC ping got no answer: {e}"))?;
    match serde_json::from_str::<crate::types::Response>(&line) {
        Ok(crate::types::Response::Pong) => Ok(()),
        _ => Err("IPC ping was not answered with Pong".to_string()),
    }
}

/// Whether the daemon's own heartbeat is being written. A process younger
/// than two beats is not judged on it.
fn heartbeat_fresh(root: &Path, uptime: Duration) -> Result<(), String> {
    use crate::daemon_install_state::{check_heartbeat, HeartbeatFreshness};
    let config = crate::daemon_heartbeat::read_heartbeat_config(root);
    if !crate::daemon_heartbeat::resolve_enabled(&config) {
        return Ok(());
    }
    let interval = crate::daemon_heartbeat::resolve_interval(&config)
        .as_secs()
        .max(1);
    let Some(file) = crate::daemon_heartbeat::resolve_heartbeat_path() else {
        return Ok(());
    };
    let young = uptime.as_secs() < interval.saturating_mul(2);
    match check_heartbeat(&file, interval.saturating_mul(3).max(30), Some(uptime.as_secs())) {
        (HeartbeatFreshness::Fresh, _) => Ok(()),
        (HeartbeatFreshness::Stale, age) => {
            Err(format!("the heartbeat is {}s old", age.unwrap_or_default()))
        }
        _ if young => Ok(()),
        (HeartbeatFreshness::PriorBoot, _) => {
            Err("the heartbeat has not been written since this process started".to_string())
        }
        (HeartbeatFreshness::Unknown, _) => Err("there is no heartbeat file".to_string()),
    }
}

/// The role half of a requeue: give back the claim the run's breadcrumb
/// names, on the forge, and forget the breadcrumb. Only while the claim is
/// still this run's to give back ([`role_resume::claim_still_ours`]), as the
/// sweep half does.
pub(crate) fn requeue_role(
    item: &ManifestItem,
    notice: &RollRequeueNotice,
    forge: bool,
    gh: &Path,
) -> Result<(), String> {
    let dir = pause_dir(item);
    let claim = claim_breadcrumb::read(&dir).or_else(|| {
        item.claim
            .as_ref()
            .and_then(claim_breadcrumb::ClaimBreadcrumb::from_manifest)
    });
    if let (true, Some(claim)) = (forge, claim) {
        let root = Path::new(&item.repo);
        match role_resume::claim_still_ours(root, gh, &claim, item.stopped_at) {
            Ok(()) => role_resume::release_claim(root, gh, &claim, &notice.comment_body())?,
            Err(why) => log::warn!(
                "pause_resume: {}: {} on {} #{} is no longer this run's claim ({why}); nothing \
                 is written to the forge for it",
                item.id,
                claim.label,
                claim.on,
                claim.number
            ),
        }
        claim_breadcrumb::clear(&dir);
    }
    Ok(())
}

impl ResumeHost for DaemonResumeHost {
    fn hold_dispatch(&self) -> ResumeHold {
        self.drain.roll_resume_hold(
            "dispatch HELD: this daemon started after a version roll paused its agents; it is \
             verifying its health and resuming them (#10832). The hold ends by itself."
                .to_string(),
        )
    }

    fn health_sample(&self) -> Result<(), String> {
        ipc_answers()?;
        heartbeat_fresh(&self.fallback_root, self.started.elapsed())
    }

    fn safe_point(&self, item: &ManifestItem, request: &PauseRequest) -> Option<SafePointRecord> {
        disk_safe_point(item, request)
    }

    fn reap_residue(&self, item: &ManifestItem) -> TeardownReport {
        reap_residue(item)
    }

    fn refresh_lease(&self, item: &ManifestItem, timeout: Duration) -> Result<(), String> {
        if item.kind != ItemKind::Sweep {
            return Ok(()); // a role run has no issue lease
        }
        sweeps::refresh_lease(&self.registry(item), item, timeout)
    }

    fn already_resumed(&self, item: &ManifestItem) -> Option<String> {
        if item.kind == ItemKind::Sweep {
            return sweeps::already_resumed(&self.registry(item), item);
        }
        roll_pause::live_runs::snapshot()
            .into_iter()
            .find(|run| run.resume.as_ref().is_some_and(|l| l.resume_of == item.id))
            .map(|run| run.item_id)
    }

    fn check(
        &self,
        item: &ManifestItem,
        launch: &RollResumeLaunch,
    ) -> Result<(), RollResumeRefusal> {
        let cwd = item
            .resume_handle
            .as_ref()
            .and_then(|h| h.cwd.clone())
            .unwrap_or_else(|| item.repo.clone());
        if item.kind == ItemKind::Sweep {
            sweeps::check(&self.registry(item), item, launch)?;
        } else {
            role_resume::enabled_role(Path::new(&item.repo), item.role.as_deref().unwrap_or(""))?;
        }
        session_checks(launch, &cwd, docker_container_ready)
    }

    fn launch(
        &self,
        item: &ManifestItem,
        launch: &RollResumeLaunch,
        wait: Duration,
    ) -> Result<Launched, RollResumeRefusal> {
        if item.kind == ItemKind::Sweep {
            return sweeps::launch(&self.registry(item), item, launch);
        }
        let spec = role_resume::RoleResumeSpec {
            root: PathBuf::from(&item.repo),
            role: item.role.clone().unwrap_or_default(),
            launch: launch.clone(),
            timeout_remaining: item.timeout_remaining_secs.map(Duration::from_secs),
            holds_issue_creation_mutex: item.holds_issue_creation_mutex,
            spawn_bin: None,
            gh_bin: None,
        };
        let handle =
            role_resume::launch(&spec, &self.in_progress, wait.min(Duration::from_secs(60)))?;
        let launched = Launched {
            item_id: handle.item_id.clone(),
            pid: handle.pid,
            at: Instant::now(),
            log_path: None,
            header_anchor: None,
        };
        self.roles().insert(launched.item_id.clone(), handle);
        Ok(launched)
    }

    fn liveness(&self, item: &ManifestItem, launched: &Launched) -> Liveness {
        if item.kind == ItemKind::Sweep {
            return sweeps::liveness(&self.registry(item), item, launched);
        }
        role_liveness(self.roles().get_mut(&launched.item_id))
    }

    fn abandon(&self, item: &ManifestItem, launched: &Launched) {
        if item.kind == ItemKind::Sweep {
            sweeps::abandon(&self.registry(item), launched);
        } else {
            abandon_role(launched);
            self.roles().remove(&launched.item_id);
        }
    }

    fn settle(&self, manifest_id: &str, item: &ManifestItem, new_item_id: &str) {
        roll_pause::suppress::release_item(manifest_id, &item.id);
        // The sweep reaper takes the resumed run from here.
        roll_pause::hold::release(new_item_id, RESUME_HOLD_OWNER);
    }

    fn requeue(
        &self,
        item: &ManifestItem,
        notice: &RollRequeueNotice,
        forge: bool,
    ) -> Result<(), String> {
        if item.kind == ItemKind::Sweep {
            sweeps::requeue(&self.registry(item), item, notice, forge)
        } else {
            requeue_role(item, notice, forge, &crate::write_scope::default_gh())
        }
    }

    fn recover(&self, manifest_id: &str, item: &ManifestItem) {
        roll_pause::suppress::release_item(manifest_id, &item.id);
        if item.kind == ItemKind::Sweep {
            sweeps::recover(&self.registry(item), item);
        }
    }

    fn finish(&self, manifest_id: &str, note: &str) {
        roll_pause::suppress::disarm(manifest_id);
        self.drain.release_roll_resume_hold(note.to_string());
    }

    fn emit(&self, topic: &str, payload: serde_json::Value) {
        let _ = self.bus.publish_generic(topic, payload);
    }

    fn publish(&self, status: &PauseResumeStatus) {
        super::startup::publish(status);
    }
}

/// The safe-point record on disk for `item`, when it answers `request`. A
/// record left by an earlier pause of the same item is not this pause's.
pub(crate) fn disk_safe_point(
    item: &ManifestItem,
    request: &PauseRequest,
) -> Option<SafePointRecord> {
    let sp = roll_pause::read_safe_point_for(&pause_dir(item), request)?;
    Some(SafePointRecord {
        reached_at: sp.reached_at,
        parked_tool: Some(sp.parked_tool),
        parked_summary: Some(sp.parked_summary),
    })
}

/// Reap whatever is still alive of `item`'s recorded process tree.
pub(crate) fn reap_residue(item: &ManifestItem) -> TeardownReport {
    let spec = residue_spec(item);
    if spec == TreeSpec::default() {
        return TeardownReport::default();
    }
    teardown::teardown_tree(&spec, teardown::TERM_GRACE)
}

/// A resumed role run's liveness. A run H5 no longer has a handle for is not
/// known to have started its session, so it is not counted as resumed (the
/// sweep half's `ResumeChild::Untracked`).
pub(crate) fn role_liveness(handle: Option<&mut RoleResumeHandle>) -> Liveness {
    match handle.map(RoleResumeHandle::state) {
        Some(RoleResumeState::Running) => Liveness::Running,
        Some(RoleResumeState::Succeeded) => Liveness::Finished,
        Some(RoleResumeState::Failed(how)) => Liveness::Died {
            reason: REASON_RESUME_FAILED.to_string(),
            detail: how,
        },
        None => Liveness::Died {
            reason: REASON_RESUME_FAILED.to_string(),
            detail: "the resumed role run is no longer tracked".to_string(),
        },
    }
}

/// Stop a resumed role run that did not start its session. Its thread sees
/// the child end and releases the in-progress guard.
pub(crate) fn abandon_role(launched: &Launched) {
    if let Some(pid) = launched.pid {
        let _ = teardown::teardown_tree(
            // The role thread's own unreaped child, started no later than
            // now: the identity the teardown checks before it signals.
            &TreeSpec {
                pid: Some(pid),
                pgid: Some(pid),
                recorded_started_at: Some(chrono::Utc::now()),
                ..TreeSpec::default()
            },
            Duration::from_millis(500),
        );
    }
}

/// A resumed sweep's liveness from its child's state and its own log region.
pub(crate) fn sweep_liveness(
    child: ResumeChild,
    launched: &Launched,
    item: &ManifestItem,
) -> Liveness {
    let log = launched
        .log_path
        .as_deref()
        .and_then(|p| std::fs::read(p).ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let anchor = launched.header_anchor.as_deref().unwrap_or_default();
    let region = sweep_resume::dispatch_region(&log, anchor);
    let died = |detail: String| {
        // `session-exec host` announces a refused container in the log.
        let reason = if crate::session_exec::refusal::announced(region).is_some() {
            REASON_SESSION_DOWN
        } else {
            REASON_RESUME_FAILED
        };
        Liveness::Died {
            reason: reason.to_string(),
            detail,
        }
    };
    let recorded = item
        .resume_handle
        .as_ref()
        .and_then(|h| h.sandbox.as_deref());
    match child {
        ResumeChild::Running => match launched
            .log_path
            .as_deref()
            .and_then(|p| sweep_resume::sandbox_mismatch(p, anchor, recorded))
        {
            Some(mismatch) => died(mismatch),
            None => Liveness::Running,
        },
        ResumeChild::Exited(Some(0)) => Liveness::Finished,
        ResumeChild::Exited(code) => died(format!(
            "the resumed process exited ({}) before its session was running",
            code.map_or_else(|| "killed by a signal".to_string(), |c| format!("code {c}"))
        )),
        ResumeChild::Untracked => died("the resumed process is no longer tracked".to_string()),
    }
}
