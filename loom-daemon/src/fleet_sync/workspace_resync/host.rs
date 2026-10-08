//! The host side of the workspace resync (#10718): the H0 gate, and the glue
//! that runs a pass from the fleet-sync timer with the daemon's real inputs.
//!
//! # A pass never delays fleet-sync
//!
//! The timer does not wait for a pass. [`spawn_pass`] starts one on its own
//! task and returns; the fleet-store sync, the floor and `paused`/`stopped`
//! enforcement keep their cadence whatever git is doing. Passes are
//! single-flight: while one is running the next tick starts none (it is
//! skipped, not queued), so there are never two. A finished pass records its
//! result in the current status snapshot.
//!
//! The other half of "bounded" is inside the pass: a time budget for
//! classification, a deadline after which no resync starts, and a short
//! timeout on every git child (see the parent module).
//!
//! # A pass that does not end
//!
//! Every child a pass runs has a timeout, so a pass ends. If one does not
//! anyway (a filesystem that stops answering, a defect), two things happen
//! (#10987):
//!
//! * At the [`STUCK_AFTER_TICKS`]-th tick in a row that finds it still
//!   running, a `pass-stuck` alert is published, once for that pass.
//! * After [`ABANDON_AFTER`] the task stops waiting for it and gives the
//!   single-flight slot up, so the slot is never held forever. A thread
//!   cannot be killed, so the old pass may still exist and still hold the
//!   pass memory: until it ends, each new pass finds the memory taken, does
//!   nothing, and says so on the status snapshot. Nothing piles up, and the
//!   passes resume by themselves when the old one ends.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, TryLockError};
use std::time::{Duration, Instant};

use chrono::Utc;

use super::{
    Alert, Env, LazyPayload, Memory, WorkspacePass, ALERT_TOPIC, PASS_BUDGET, PASS_DEADLINE,
};
use crate::event_bus::EventBus;
use crate::fleet_state::Enforcer;
use crate::fleet_store::gh::GhTransport;
use crate::fleet_store::resync_claim::{ClaimForge, ClaimGh};
use crate::fleet_sync::{Mode, PassInputs};
use crate::init::payload::{Payload, Stamp};
use crate::install_compat::Version;

/// Why this host may not claim or write this tick.
///
/// There is no H-state enum yet (gauges are #10721); this is the one question
/// the workspace pass needs answered, over signals that already exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotCurrent {
    /// New dispatch is paused: a drain, a roll's pause, or a fleet hold (H4).
    Draining,
    /// A pause roll is armed, committed or in progress, so a restart is
    /// coming (H3/H4, #10831).
    RollPending,
    /// The binary on disk is not the one this process runs (H3).
    Staged,
    /// The running version is below the fleet floor (H7, floor).
    FloorBelow,
    /// The self-update loop is backing off or has given up (H7).
    Stalled,
    /// The startup pass has not completed (H5).
    Unverified,
    /// Not a state at all but a fact about the binary: it is not an official
    /// release build (the release workflow did not build it, or the release
    /// tag names another commit), so its payload is no release. Such a host
    /// never resyncs, and says so once, not per repo.
    NotAReleaseBuild,
    /// The binary carries a release stamp, but the forge has not yet
    /// confirmed that the release tag names its commit. Nothing is pushed
    /// until it has; the lookup is retried with backoff.
    ReleaseUnverified,
}

impl NotCurrent {
    /// The reason, as `loom-daemon status` prints it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draining => "draining",
            Self::RollPending => "roll pending",
            Self::Staged => "a different binary is staged on disk",
            Self::FloorBelow => "running version is below the fleet floor",
            Self::Stalled => "self-update is stalled",
            Self::Unverified => "startup pass not complete",
            Self::NotAReleaseBuild => "this daemon is not an official release build",
            Self::ReleaseUnverified => "this daemon's release tag is not verified yet",
        }
    }

    /// A fact about the binary, not a state the host is passing through.
    #[must_use]
    pub fn is_about_the_build(self) -> bool {
        matches!(self, Self::NotAReleaseBuild | Self::ReleaseUnverified)
    }

    /// The host line of a pass: `host not H0: <reason>`.
    #[must_use]
    pub fn host_note(self) -> String {
        match self {
            Self::NotAReleaseBuild => format!("host never resyncs: {}", self.as_str()),
            _ => format!("host not H0: {}", self.as_str()),
        }
    }
}

impl std::fmt::Display for NotCurrent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The signals [`host_gate`] reads. All process-wide and already maintained
/// by other parts of the daemon; [`HostGateInputs::live`] collects them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostGateInputs {
    /// The binary has not been shown NOT to be an official release build: it
    /// carries the release workflow's stamp for its own version, and the
    /// release tag has not been found to name another commit.
    pub release_build: bool,
    /// The forge confirmed the release tag names this binary's commit.
    pub release_verified: bool,
    /// A fleet-sync pass has completed in this process.
    pub verified: bool,
    /// The drain flag is set, or the fleet state holds this host.
    pub draining: bool,
    /// A roll is coming: a pause roll is armed, committed or in progress
    /// (#10831, [`crate::fleet_state::DrainFacts::roll_in_progress`]). Not a
    /// fleet-state `paused` hold or an operator drain; those are `draining`.
    pub roll_pending: bool,
    /// The running binary's file was replaced or removed since boot.
    pub staged: bool,
    /// The running version is below `loom_min_version`.
    pub below_floor: bool,
    /// The self-update loop is in backoff or terminal.
    pub stalled: bool,
}

/// Is this host in H0, i.e. may it claim and write? Pure.
///
/// A newer release merely existing is NOT a reason to wait: a host that is
/// settling (H1) is verified and running, and what it installs is the version
/// it runs. Waiting for a settle period or a roll window would starve the
/// resync on a fleet that releases many times a day (#10885).
///
/// # Errors
/// The first reason that applies.
pub fn host_gate(i: &HostGateInputs) -> Result<(), NotCurrent> {
    let checks = [
        (!i.release_build, NotCurrent::NotAReleaseBuild),
        (!i.release_verified, NotCurrent::ReleaseUnverified),
        (!i.verified, NotCurrent::Unverified),
        (i.draining, NotCurrent::Draining),
        (i.staged, NotCurrent::Staged),
        (i.roll_pending, NotCurrent::RollPending),
        (i.stalled, NotCurrent::Stalled),
        (i.below_floor, NotCurrent::FloorBelow),
    ];
    match checks.iter().find(|(fails, _)| *fails) {
        Some((_, why)) => Err(*why),
        None => Ok(()),
    }
}

// ============================================================================
// Live signals
// ============================================================================

static VERIFIED: AtomicBool = AtomicBool::new(false);

/// A fleet-sync pass has completed in this process.
pub(in crate::fleet_sync) fn mark_verified() {
    VERIFIED.store(true, Ordering::Relaxed);
}

/// Identity of the file a process runs from: replaced by a roll's install
/// (write, then rename) long before the restart lands.
type ExeId = (u64, u64, u64);

fn exe_id() -> Option<ExeId> {
    let exe = std::env::current_exe().ok()?;
    // Linux appends this once the inode is unlinked.
    if exe.to_string_lossy().ends_with(" (deleted)") {
        return None;
    }
    let meta = std::fs::metadata(&exe).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((meta.dev(), meta.ino(), meta.len()))
    }
    #[cfg(not(unix))]
    {
        Some((0, 0, meta.len()))
    }
}

fn boot_exe() -> &'static OnceLock<Option<ExeId>> {
    static BOOT: OnceLock<Option<ExeId>> = OnceLock::new();
    &BOOT
}

/// Record the running binary's file identity. Called once, at boot.
pub(in crate::fleet_sync) fn mark_boot() {
    boot_exe().get_or_init(exe_id);
}

impl HostGateInputs {
    /// The signals as they are right now.
    #[must_use]
    pub fn live(enforcer: &dyn Enforcer) -> Self {
        let facts = enforcer.drain_facts();
        let update = crate::auto_update::global_status_snapshot();
        let running = Version::parse(env!("CARGO_PKG_VERSION"));
        let floor = crate::fleet_sync::loom_min_version()
            .as_deref()
            .and_then(Version::parse);
        // Read without asking the forge: `run_live` does the asking, once.
        let provenance = crate::release_provenance::current();
        Self {
            release_build: !provenance.refuted(),
            release_verified: Stamp::this_binary().is_some_and(|s| s.release_build),
            verified: VERIFIED.load(Ordering::Relaxed),
            draining: facts.draining,
            roll_pending: facts.roll_in_progress,
            // An unreadable identity at boot proves nothing either way.
            staged: boot_exe()
                .get()
                .copied()
                .flatten()
                .is_some_and(|boot| exe_id() != Some(boot)),
            below_floor: matches!((running, floor), (Some(r), Some(f)) if r < f),
            stalled: update.enabled
                && (update.backoff_secs.is_some() || update.terminal_reason.is_some()),
        }
    }
}

// ============================================================================
// Running a pass from the timer
// ============================================================================

fn memory() -> &'static Mutex<Memory> {
    static MEMORY: OnceLock<Mutex<Memory>> = OnceLock::new();
    MEMORY.get_or_init(|| Mutex::new(Memory::default()))
}

/// The registered workspaces that exist on this host.
fn registered_roots() -> Vec<PathBuf> {
    crate::workspace_registry::default_registry_path()
        .and_then(|p| crate::workspace_registry::WorkspaceRegistry::load(&p))
        .map(|r| r.roots())
        .unwrap_or_default()
        .into_iter()
        .filter(|root| root.is_dir())
        .collect()
}

fn github_nwo(root: &Path) -> Option<String> {
    crate::init::git::extract_repo_info(root).map(|(owner, repo)| format!("{owner}/{repo}"))
}

/// One workspace pass with the daemon's real inputs. Blocking (git and `gh`
/// children), so it runs on a blocking thread.
fn run_live(
    inputs: &PassInputs,
    mode: Mode,
    enforcer: Option<&dyn Enforcer>,
    bus: Option<&EventBus>,
) -> WorkspacePass {
    // No enforcer means no drain state to read (a test, a read-only caller):
    // there is no way to know the host is in H0, so there is no pass.
    let (Some(enforcer), Some(running)) = (enforcer, Version::parse(env!("CARGO_PKG_VERSION")))
    else {
        return WorkspacePass::default();
    };
    let started = Instant::now();
    let mut guard = match memory().try_lock() {
        Ok(guard) => guard,
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        Err(TryLockError::WouldBlock) => {
            // An abandoned pass (see the module docs) still holds it. Waiting
            // here would park one more thread every tick.
            log::warn!(
                "workspace_resync: an earlier pass that never ended still holds the pass \
                 memory; nothing is checked this tick"
            );
            // The last finished pass's findings stay on the snapshot.
            let last = latest();
            return WorkspacePass {
                running: last.running,
                host: Some("an earlier workspace pass has not ended; nothing was checked".into()),
                workspaces: last.workspaces,
                ..WorkspacePass::default()
            };
        }
    };
    // Release provenance, part (b): ask the forge what the release tag names.
    // Once per process (the answer is cached; a lookup with no answer is
    // retried with backoff), and only from a host that could write: one with
    // `fleet.autoApply` off or dispatch paused asks nobody.
    if mode == Mode::Write && !enforcer.drain_facts().draining {
        let found = crate::release_provenance::ensure(Utc::now(), inputs.interval, &|repo, tag| {
            // The release-resolve machinery: `gh api`, peeling an
            // annotated tag to the commit it names.
            crate::release_fetch::source::resolve_tag_commit(
                &|path| crate::release_fetch::source::gh_api(&inputs.workspace, path),
                repo,
                tag,
            )
        });
        if guard.noted.insert(format!("provenance:{found}")) {
            log::info!("workspace_resync: release provenance: {found}");
        }
    }
    let payload = LazyPayload::new(Payload::embedded);
    let env = Env {
        host: &inputs.host,
        running,
        floor: crate::fleet_sync::loom_min_version()
            .as_deref()
            .and_then(Version::parse),
        interval: inputs.interval,
        payload: &payload,
        nwo: &github_nwo,
        forge: &|root, nwo| -> Box<dyn ClaimForge> {
            Box::new(ClaimGh(GhTransport::new(root, nwo)))
        },
        may_write: &|root, nwo| match crate::write_scope::repo_writable(root, nwo) {
            crate::write_scope::Verdict::Allow(_) => Ok(()),
            crate::write_scope::Verdict::Deny(why) => Err(why),
        },
        gate: &|| super::host_gate(&HostGateInputs::live(enforcer)),
        clock: &Utc::now,
        spent: &|| started.elapsed() >= PASS_BUDGET,
        overdue: &|| started.elapsed() >= PASS_DEADLINE,
        heads: &super::heads::live,
    };
    let pass = super::run(&env, &registered_roots(), mode, &mut guard);
    drop(guard);
    if let Some(bus) = bus {
        for alert in &pass.alerts {
            let payload = serde_json::to_value(alert).unwrap_or(serde_json::Value::Null);
            let _ = bus.publish_generic(ALERT_TOPIC, payload);
        }
    }
    pass
}

// ============================================================================
// Single flight
// ============================================================================

static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// The one running pass. Dropping it lets the next one start, on every way
/// out of the task, a panic included.
pub(super) struct Flight<'a>(&'a AtomicBool);

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Take the single-flight slot; `None` while another pass holds it.
pub(super) fn begin(slot: &AtomicBool) -> Option<Flight<'_>> {
    // Lazily: a `Flight` built for a refused caller would free the slot when
    // it was dropped.
    (!slot.swap(true, Ordering::AcqRel)).then(|| Flight(slot))
}

fn latest_cell() -> &'static Mutex<WorkspacePass> {
    static LATEST: OnceLock<Mutex<WorkspacePass>> = OnceLock::new();
    LATEST.get_or_init(|| Mutex::new(WorkspacePass::default()))
}

/// What the last finished workspace pass found. The timer puts it on each
/// status snapshot it publishes, so a fleet-sync pass does not blank it.
pub(in crate::fleet_sync) fn latest() -> WorkspacePass {
    latest_cell()
        .lock()
        .map(|found| found.clone())
        .unwrap_or_default()
}

/// Ticks in a row that find the previous pass still running before that is
/// an alert.
pub const STUCK_AFTER_TICKS: u32 = 5;

/// How long the task waits for a pass before it gives the single-flight slot
/// up. Far beyond what a pass can take with every child at its timeout, and
/// the claim's stale window: by then the fence refuses any push the old pass
/// might still try.
pub const ABANDON_AFTER: Duration = Duration::from_secs(15 * 60);

/// Ticks in a row that found the previous pass still running.
static REFUSED: AtomicU32 = AtomicU32::new(0);

/// Count one tick that found the previous pass still running. Returns the
/// count, and whether this is the tick that alerts.
pub(super) fn note_refused(refused: &AtomicU32) -> (u32, bool) {
    let ticks = refused.fetch_add(1, Ordering::AcqRel).saturating_add(1);
    (ticks, ticks == STUCK_AFTER_TICKS)
}

fn publish_stuck(bus: Option<&EventBus>, ticks: u32, detail: String) {
    log::error!("fleet_sync: {detail}");
    let alert = Alert {
        root: PathBuf::new(),
        repo: None,
        kind: "pass-stuck",
        failures: ticks,
        detail,
        next_attempt: Utc::now(),
    };
    if let Some(bus) = bus {
        let payload = serde_json::to_value(&alert).unwrap_or(serde_json::Value::Null);
        let _ = bus.publish_generic(ALERT_TOPIC, payload);
    }
}

/// How a supervised pass ended.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Ended<T> {
    /// It returned.
    Done(T),
    /// It panicked.
    Panicked(String),
    /// It was still running at the deadline. It is not waited for any longer;
    /// its thread runs on until whatever it is stuck in lets go.
    Abandoned,
}

/// Run `work` on a blocking thread and wait for it for at most `deadline`,
/// holding `flight` meanwhile. The single-flight slot is free again when
/// this returns, however the work ended.
pub(super) async fn supervise<T: Send + 'static>(
    flight: Flight<'_>,
    deadline: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> Ended<T> {
    let _flight = flight;
    match tokio::time::timeout(deadline, tokio::task::spawn_blocking(work)).await {
        Ok(Ok(found)) => Ended::Done(found),
        Ok(Err(e)) => Ended::Panicked(e.to_string()),
        Err(_) => Ended::Abandoned,
    }
}

/// Start a workspace pass on its own task and return at once. Called by the
/// timer task: once at startup (after the startup pass, with the drain state
/// wired) and after every timer pass, once run-state enforcement has been
/// applied. Returns `false`, and starts nothing, while the previous pass is
/// still running.
pub(in crate::fleet_sync) fn spawn_pass(
    inputs: &PassInputs,
    mode: Mode,
    enforcer: &Option<Arc<dyn Enforcer>>,
    bus: &Option<Arc<EventBus>>,
) -> bool {
    let Some(flight) = begin(&IN_FLIGHT) else {
        let (ticks, alert) = note_refused(&REFUSED);
        if alert {
            let detail = format!(
                "a workspace pass is still running {ticks} ticks after it started; no pass has \
                 started since, and the workspace states shown are from before it"
            );
            publish_stuck(bus.as_deref(), ticks, detail);
        } else {
            log::info!(
                "fleet_sync: the previous workspace pass is still running; none is started this \
                 tick ({ticks} in a row)"
            );
        }
        return false;
    };
    REFUSED.store(0, Ordering::Release);
    let (owned, enforcer, bus) = (inputs.clone(), enforcer.clone(), bus.clone());
    tokio::spawn(async move {
        let stuck_bus = bus.clone();
        let work = move || run_live(&owned, mode, enforcer.as_deref(), bus.as_deref());
        match supervise(flight, ABANDON_AFTER, work).await {
            Ended::Done(found) => {
                if let Ok(mut latest) = latest_cell().lock() {
                    latest.clone_from(&found);
                }
                crate::fleet_sync::publish_workspaces(&found);
            }
            Ended::Panicked(e) => log::warn!("fleet_sync: a workspace pass panicked: {e}"),
            Ended::Abandoned => {
                let detail = format!(
                    "a workspace pass did not end within {}s and is no longer waited for; passes \
                     resume when it ends",
                    ABANDON_AFTER.as_secs()
                );
                publish_stuck(stuck_bus.as_deref(), STUCK_AFTER_TICKS, detail);
            }
        }
    });
    true
}
