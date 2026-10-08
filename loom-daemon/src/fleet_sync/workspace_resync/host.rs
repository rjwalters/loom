//! The host side of the workspace resync (#10718): the H0 gate, and the glue
//! that runs a pass from the fleet-sync timer with the daemon's real inputs.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use chrono::Utc;

use super::{Env, LazyPayload, Memory, WorkspacePass, ALERT_TOPIC};
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
    /// A roll is retained across a refused drain and will re-arm (H2/H3).
    RollPending,
    /// The binary on disk is not the one this process runs (H3).
    Staged,
    /// The running version is below the fleet floor (H7, floor).
    FloorBelow,
    /// The self-update loop is backing off or has given up (H7).
    Stalled,
    /// The startup pass has not completed (H5).
    Unverified,
    /// Not a state at all but a fact about the binary: it was not built from
    /// a clean checkout, so its payload is no release. Such a host never
    /// resyncs, and says so once, not per repo.
    NotAReleaseBuild,
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
            Self::NotAReleaseBuild => "this daemon is not a release build",
        }
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
    /// The embedded payload is a release's tracked tree, unmodified.
    pub release_build: bool,
    /// A fleet-sync pass has completed in this process.
    pub verified: bool,
    /// The drain flag is set, or the fleet state holds this host.
    pub draining: bool,
    /// The drain descriptor retains a roll.
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
        let (draining, roll_pending) = enforcer.drain_facts();
        let update = crate::auto_update::global_status_snapshot();
        let running = Version::parse(env!("CARGO_PKG_VERSION"));
        let floor = crate::fleet_sync::loom_min_version()
            .as_deref()
            .and_then(Version::parse);
        Self {
            release_build: Stamp::this_binary().is_some_and(|s| s.release_build),
            verified: VERIFIED.load(Ordering::Relaxed),
            draining,
            roll_pending,
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
    };
    let mut guard = match memory().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
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

/// Run the workspace half and record it in `status`, the snapshot the pass
/// before it published. Called by the timer task: once at startup (after the
/// startup pass, with the drain state wired) and after every timer pass, once
/// run-state enforcement has been applied. `None` means the startup pass did
/// not finish; the first timer pass covers it.
pub(in crate::fleet_sync) async fn pass(
    inputs: &PassInputs,
    mode: Mode,
    status: Option<crate::fleet_sync::FleetSyncStatus>,
    enforcer: &Option<Arc<dyn Enforcer>>,
    bus: &Option<Arc<EventBus>>,
) {
    let Some(mut status) = status else {
        return;
    };
    let (owned, enforcer, bus) = (inputs.clone(), enforcer.clone(), bus.clone());
    let found = tokio::task::spawn_blocking(move || {
        run_live(&owned, mode, enforcer.as_deref(), bus.as_deref())
    })
    .await;
    match found {
        Ok(found) => {
            status.workspaces = found;
            crate::fleet_sync::publish(&status);
        }
        Err(e) => log::warn!("fleet_sync: a workspace pass panicked: {e}"),
    }
}
