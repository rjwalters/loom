//! Mount-drift recreation for the session reconcile pass (issue #10364 Part
//! B, Epic #10452 Phase 2).
//!
//! A host-mode session container's workspace mounts are fixed when it is
//! created (#9979). A repository registered later is unreachable from it
//! (every Codex tick there fails `chdir to cwd`), and one deregistered later
//! stays mounted read-write while Codex runs with its own sandbox off. Part A
//! detects both ([`mount_drift`]) and refuses a dispatch into
//! an unmounted workdir; this module fixes the container.
//!
//! # Per pass, for a running container with drift
//!
//! Only accounts the pass already acts on reach here: enabled,
//! session-managed, not held, not backing off, container running.
//!
//! 1. **Private-clone** (label `loom.workspace-mode=private-clone`, or the
//!    account configured under `.private-sessions`, or that cannot be ruled
//!    out): never drift-recreated.
//! 2. **Reported** once per distinct drift: every `extra` path at WARN (a
//!    deregistered repository still mounted read-write), `missing` at INFO.
//! 3. **Not achievable** → left alone ([`super::Outcome::DriftUnachievable`],
//!    WARN once, no retry until the drift itself changes): the previous pass
//!    already recreated it for drift and the fresh container *still* drifts
//!    (intended ≠ achievable), or it only *lacks* mounts and the intended set
//!    would be refused (`/`, no registered root under the workspace, the
//!    home directory, a `firewall: true` overlap). The refusal is checked
//!    before any teardown: removing a container that is merely missing a
//!    mount, with nothing allowed in its place, would only lose capacity.
//!    With `extra` drift a refused recreate does **not** save the container:
//!    see step 6.
//! 4. **Fresh re-check**: one direct `docker inspect` must still show the
//!    same container id, running; otherwise it changed under the pass and is
//!    left for the next one.
//! 5. **Busy** (the `docker top` in-flight check, the #5119 contract):
//!    deferred, logged, re-checked next pass. Never killed.
//! 6. **Idle**: re-check the operator hold, `docker stop` (graceful,
//!    [`STOP_GRACE`]) + `docker rm`, then [`super::recreate_container`]
//!    against the workspace and image of the account's last operator start
//!    (`.session-last-start.json`, which survives a daemon restart), else
//!    the container's own `loom.workspace` label and image. The new
//!    container's mounts are computed from the registry **now**. The next
//!    pass confirms it is running and no longer drifts. If the container has
//!    `extra` drift and the recreate would be refused, it is still stopped
//!    and removed, and nothing replaces it
//!    ([`super::Outcome::DriftRemoved`]): `session start` would not create a
//!    container with those mounts today, so one must not keep running with
//!    them. The pass's missing-container path retries the start on its
//!    backoff and succeeds once the registry or roster allows one.
//!
//! # What counts as `extra`
//!
//! [`effective_drift`]: Part A's registry comparison, plus any workspace
//! mount that [`check_mount_denials`] refuses today ([`Denials`]): the home
//! directory or an ancestor, or a path overlapping a `firewall: true`
//! repository, **even if it is still registered**. A container created with
//! `--mount-workspace <one git checkout>` is not `extra` merely because that
//! checkout is not (or no longer) registered: `session start` accepts an
//! unregistered checkout as an explicit operator grant, the reconciler
//! cannot tell "deregistered" from "never registered", and a recreate would
//! mount it again. It becomes `extra` only through a denial.
//!
//! Unlike an operator `accounts session stop`, the teardown writes **no**
//! hold: the reconciler is replacing the container, not keeping it down, and
//! it never lifts one. A timed-out `docker` call here (inspect, top, stop,
//! rm, run: [`crate::tokens_pool::docker_cli::DockerTimedOut`]) propagates
//! unchanged, so it ends the pass at pass level like any other timeout and
//! is never counted against the account. Any other failed teardown or
//! recreate takes the pass's ordinary per-account backoff;
//! a recreate that was refused after the teardown leaves the container
//! missing, which the pass's missing-container path then recreates (on the
//! same backoff).
//!
//! Known window (shared with `accounts session stop`): an exec that starts
//! between the `docker top` check and `docker stop` is interrupted. Selection
//! passes a drifted account over (#10454) where it can tell, which narrows
//! it.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{recreate_container, AccountMemory, Outcome, PassInputs};
use crate::tokens_pool::account_registry::AccountDescriptor;
use crate::tokens_pool::session_hold::OperatorHeld;
use crate::tokens_pool::session_lifecycle::{
    check_mount_denials, container_name, firewalled_repo_paths, workspace_mount_roots,
    ContainerRunner, SessionLifecycle, STOP_GRACE,
};
use crate::tokens_pool::session_state::{
    container_running, is_private_clone, mount_drift, workspace_label, workspace_mounts,
    MountDrift, Snapshot,
};

/// What a session container may not mount whatever the registry says: the
/// inputs of [`check_mount_denials`], read once per pass.
#[derive(Debug, Default, Clone)]
pub struct Denials {
    pub home: Option<PathBuf>,
    /// `firewall: true` repository paths from the cached fleet roster.
    pub firewalled: Vec<PathBuf>,
}

impl Denials {
    /// The home directory and the roster's firewalled paths, as `session
    /// start` reads them for `daemon_root`. An unreadable roster yields no
    /// firewall verdict here (nothing is torn down on a broken input);
    /// `session start` itself still fails closed on it.
    #[must_use]
    pub fn load(daemon_root: &Path) -> Self {
        Self {
            home: dirs::home_dir(),
            firewalled: firewalled_repo_paths(daemon_root).unwrap_or_else(|error| {
                log::debug!("session_reconcile: firewall roster unreadable ({error:#})");
                Vec::new()
            }),
        }
    }

    fn check(&self, roots: &[PathBuf]) -> anyhow::Result<()> {
        check_mount_denials(roots, self.home.as_deref(), &self.firewalled)
    }
}

/// A running container's drift as the reconciler acts on it: the registry
/// comparison ([`mount_drift`]) plus, as `extra`, every workspace mount that
/// `denials` refuses today. Empty for a private-clone or unlabelled container.
#[must_use]
pub fn effective_drift(inspect: &Value, registered: &[PathBuf], denials: &Denials) -> MountDrift {
    let mut drift = mount_drift(inspect, registered);
    for mount in workspace_mounts(inspect) {
        if !drift.extra.contains(&mount) && denials.check(std::slice::from_ref(&mount)).is_err() {
            drift.extra.push(mount);
        }
    }
    drift.extra.sort();
    drift
}

/// Why a drifted container was not recreated this pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferReason {
    /// A `docker exec` is in flight.
    Busy,
    /// The container is no longer the one the snapshot saw (gone, stopped,
    /// or replaced) by the time of the pre-action re-check.
    Changed,
}

/// What the pass remembers about one account's drift between passes.
#[derive(Debug, Default, Clone)]
pub(super) struct DriftMemory {
    /// The drift the last recreate was for; the next running sighting must
    /// show none.
    recreated: Option<MountDrift>,
    /// A drift a recreate cannot fix; not retried while it is unchanged.
    unachievable: Option<MountDrift>,
    /// The drift already reported (WARN/INFO once per distinct drift).
    reported: Option<MountDrift>,
    busy_reported: bool,
}

/// Processing order for one account's container: `0` running with `extra`
/// drift (a containment gap), `1` running with only `missing` drift, `2`
/// anything else (including an unavailable snapshot).
#[must_use]
pub fn priority(snapshot: &Snapshot, container: &str, inputs: &PassInputs<'_>) -> u8 {
    let Some(inspect) = snapshot.inspect_of(container) else {
        return 2;
    };
    if !container_running(inspect) {
        return 2;
    }
    let drift = effective_drift(inspect, inputs.registered, inputs.denials);
    if !drift.extra.is_empty() {
        0
    } else if drift.missing.is_empty() {
        2
    } else {
        1
    }
}

/// Whether recreating against `workspace` with `registered` would be
/// refused: the checks `session start` itself makes before `docker run`.
fn refusal(workspace: &Path, inputs: &PassInputs<'_>) -> anyhow::Result<()> {
    if workspace.parent().is_none() {
        anyhow::bail!("it would mount the whole filesystem");
    }
    inputs
        .denials
        .check(&workspace_mount_roots(workspace, inputs.registered)?)
}

fn paths(list: &[PathBuf]) -> String {
    if list.is_empty() {
        return "none".into();
    }
    list.iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn report(container: &str, name: &str, drift: &MountDrift, mem: &mut DriftMemory) {
    if mem.reported.as_ref() == Some(drift) {
        return;
    }
    for path in &drift.extra {
        log::warn!(
            "session_reconcile: {container} (account {name}) still mounts {} read-write, which \
             it may no longer mount: it left the workspace registry, or is now denied (home \
             directory, `firewall: true`). Codex runs there with its own sandbox off; \
             recreating the container at its first idle moment",
            path.display()
        );
    }
    if !drift.missing.is_empty() {
        log::info!(
            "session_reconcile: {container} (account {name}) does not mount {} (registered \
             since it was created); recreating it when idle",
            paths(&drift.missing)
        );
    }
    mem.reported = Some(drift.clone());
    mem.busy_reported = false;
}

fn unachievable(
    container: &str,
    name: &str,
    drift: MountDrift,
    why: &str,
    mem: &mut DriftMemory,
) -> Outcome {
    log::warn!(
        "session_reconcile: {container} (account {name}): mount drift a recreate cannot fix \
         ({why}; missing: {}; extra: {}). Leaving it as is and not retrying until the registry \
         changes; fix the workspace registry or recreate it by hand: `loom-daemon accounts \
         session stop {name}` then `accounts session start {name} --mount-workspace <checkout \
         parent>`",
        paths(&drift.missing),
        paths(&drift.extra)
    );
    mem.unachievable = Some(drift);
    Outcome::DriftUnachievable
}

/// The drift step for a running container the snapshot holds as `inspect`.
/// Returns [`Outcome::Running`] when its mounts match the registry.
pub(super) fn reconcile_running<R: ContainerRunner>(
    lifecycle: &mut SessionLifecycle<R>,
    account: &AccountDescriptor,
    inspect: &Value,
    inputs: &PassInputs<'_>,
    account_mem: &mut AccountMemory,
) -> anyhow::Result<Outcome> {
    let name = account.id.name.as_str();
    let container = container_name(name);
    let mem = &mut account_mem.drift;
    let drift = effective_drift(inspect, inputs.registered, inputs.denials);
    if drift.is_empty() {
        if mem.recreated.is_some() {
            log::info!("session_reconcile: {container}: mounts match the registry after recreate");
        }
        *mem = DriftMemory::default();
        return Ok(Outcome::Running);
    }
    if is_private_clone(inspect) || (inputs.is_private_clone)(account).unwrap_or(true) {
        log::debug!("session_reconcile: {container}: private-clone; never drift-recreated");
        return Ok(Outcome::PrivateCloneSkipped);
    }
    if mem.recreated.take().is_some() {
        return Ok(unachievable(
            &container,
            name,
            drift,
            "a freshly recreated container still drifts",
            mem,
        ));
    }
    if mem.unachievable.as_ref() == Some(&drift) {
        log::debug!("session_reconcile: {container}: drift unchanged and not fixable; skipped");
        return Ok(Outcome::DriftUnachievable);
    }
    mem.unachievable = None;
    report(&container, name, &drift, mem);
    let Some(label) = workspace_label(inspect) else {
        return Ok(Outcome::Running);
    };
    let (workspace, image) = match inputs.index.last_start(name) {
        Some(start) => (start.workspace, Some(start.image)),
        None => (label.to_path_buf(), account_mem.image.clone()),
    };
    let mem = &mut account_mem.drift;
    let refused = refusal(&workspace, inputs).err();
    if let Some(error) = refused.as_ref().filter(|_| drift.extra.is_empty()) {
        let why = format!("recreating against {} is refused: {error:#}", workspace.display());
        return Ok(unachievable(&container, name, drift, &why, mem));
    }
    let snapshot_id = inspect["Id"].as_str().unwrap_or_default();
    match lifecycle.runner().inspect(&container)? {
        Some(now) if now.running && now.id == snapshot_id => {}
        _ => {
            log::debug!("session_reconcile: {container}: changed since the snapshot; next pass");
            return Ok(Outcome::DriftDeferred {
                reason: DeferReason::Changed,
            });
        }
    }
    if lifecycle.runner().has_active_exec(&container)? {
        if mem.busy_reported {
            log::debug!("session_reconcile: {container}: drifted but still busy");
        } else {
            log::info!(
                "session_reconcile: {container}: drifted but has an in-flight `docker exec`; \
                 deferring the recreate and re-checking next pass"
            );
            mem.busy_reported = true;
        }
        return Ok(Outcome::DriftDeferred {
            reason: DeferReason::Busy,
        });
    }
    let is_held = || inputs.index.is_held(name);
    if is_held() {
        return Err(OperatorHeld.into());
    }
    lifecycle.runner().stop_and_remove(&container, STOP_GRACE)?;
    if let Some(error) = refused {
        log::warn!(
            "session_reconcile: {container} (account {name}) mounted {} which it may no longer \
             mount, and no session container is allowed in its place ({error:#}); stopped and \
             removed it (it was idle). It is recreated automatically once `accounts session \
             start {name} --mount-workspace {}` would be accepted",
            paths(&drift.extra),
            workspace.display()
        );
        account_mem.drift = DriftMemory::default();
        return Ok(Outcome::DriftRemoved { drift });
    }
    recreate_container(lifecycle, name, &workspace, image, &is_held)?;
    account_mem.awaiting_confirm = true;
    account_mem.drift = DriftMemory {
        recreated: Some(drift.clone()),
        ..DriftMemory::default()
    };
    log::warn!(
        "session_reconcile: {container}: recreated for mount drift (missing: {}; extra: {}) \
         against --mount-workspace {} with the current registry",
        paths(&drift.missing),
        paths(&drift.extra),
        workspace.display()
    );
    Ok(Outcome::DriftRecreated { workspace, drift })
}
