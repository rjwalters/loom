//! Mount-drift recreation for the session reconcile pass (issue #10364 Part
//! B, Epic #10452 Phase 2).
//!
//! A host-mode session container's workspace mounts are fixed when it is
//! created (#9979). A repository registered later is unreachable from it
//! (every Codex tick there fails `chdir to cwd`), and one deregistered later
//! stays mounted read-write while Codex runs with its own sandbox off. Part A
//! detects both ([`mount_drift`]) and refuses a dispatch into an unmounted
//! workdir; this module fixes the container.
//!
//! # Safety rules
//!
//! 1. **Missing information means no action.** Fail-closed (stopping a
//!    container and leaving nothing in its place) applies only to a
//!    *positively established denial*: a specific mounted path that
//!    [`Denials::check`] refuses on inputs that were all read. Everything
//!    that cannot be decided leaves the container exactly as it is:
//!    * the workspace registry could not be read or parsed → no drift
//!      handling for the whole pass ([`PassInputs::registered`] is `None`;
//!      `run_tick` WARNs, on a backoff);
//!    * the registry is readable but lists **nothing** under the
//!      container's workspace (an empty registry, a truncated file) →
//!      deferred with a WARN ([`DeferReason::NothingIntended`]). "Nothing is
//!      intended" is never a reason to remove;
//!    * a registered root under the workspace is not a directory right now
//!      (an unmounted volume) → deferred ([`DeferReason::RootUnavailable`]);
//!    * the firewall roster cannot be read → no mount is treated as denied,
//!      **and** no recreate is treated as accepted, so nothing is torn down.
//! 2. **One acceptance check.** Whether a recreate would be accepted is
//!    asked of [`PassInputs::would_create_accept`], which in production is
//!    [`session_mount_gate::create_roots`] — the function
//!    `ProcessContainerRunner::create` itself calls, with the same argument.
//! 3. **A removal is never repeated, and never undone by the pass.** A
//!    fail-closed removal is recorded on disk first
//!    ([`session_drift_removal`]). While the record stands, the
//!    missing-container path does not recreate the container
//!    ([`removal_stands`]) and a container that reappears with a denied
//!    mount is WARNed about, not removed again. The record is cleared only
//!    when the denial positively no longer applies, or by an operator start.
//! 4. **A dispatch is never stopped**, including one that is only starting:
//!    the teardown takes the per-container dispatch lock exclusively
//!    ([`session_dispatch_lock`]) right before `docker stop` and defers if a
//!    dispatch holds it or the lock state is unknown. `docker top` (the
//!    #5119 contract) stays as the second line.
//!
//! # Per pass, for a running container with drift
//!
//! Only accounts the pass already acts on reach here: enabled,
//! session-managed, not held, not backing off, container running.
//!
//! 1. **Private-clone** (label `loom.workspace-mode=private-clone`, or the
//!    account configured under `.private-sessions`, or that cannot be ruled
//!    out): never drift-recreated.
//! 2. **Reported** once per distinct drift: every `extra` path at WARN,
//!    `missing` at INFO.
//! 3. **Not achievable** → left running ([`Outcome::DriftUnachievable`], WARN
//!    once, no retry until the drift itself changes): the previous pass
//!    already recreated it and the fresh container *still* drifts, or the
//!    recreate would be refused and no mounted path is positively denied.
//! 4. **Fresh re-check**: one direct `docker inspect` must still show the
//!    same container id, running.
//! 5. **Busy** (`docker top`, then the dispatch lock): deferred, logged,
//!    re-checked next pass. Never killed.
//! 6. **Idle**: re-check the operator hold, `docker stop` (graceful,
//!    [`STOP_GRACE`]) + `docker rm`, then [`recreate_container`] against the
//!    workspace and image of the account's last operator start
//!    (`.session-last-start.json`), else the container's own label and
//!    image. The mounts are computed from the registry **now**. If a mounted
//!    path is positively denied and the recreate would be refused, the
//!    container is stopped and removed and nothing replaces it
//!    ([`Outcome::DriftRemoved`], rule 3).
//!
//! The teardown writes **no** operator hold and never lifts one. A timed-out
//! `docker` call propagates unchanged
//! ([`crate::tokens_pool::docker_cli::DockerTimedOut`]) and ends the pass at
//! pass level.
//!
//! # What counts as `extra`
//!
//! Part A's registry comparison, plus any workspace mount the denials refuse
//! today (the home directory or an ancestor, an overlap with a `firewall:
//! true` repository) **even if it is still registered**. A container created
//! with `--mount-workspace <one git checkout>` is not `extra` merely because
//! that checkout is unregistered: `session start` accepts that as an explicit
//! operator grant, and a recreate would mount it again. Such a container
//! keeps the checkout mounted after `workspace remove` until an operator
//! runs `accounts session stop` (the `workspace remove` report says so).
//!
//! A drifted container keeps receiving dispatches for the repositories it
//! does mount until it is recreated: selection (#10454) reads only whether a
//! container is running, not whether its mounts are current.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{recreate_container, AccountMemory, Outcome, PassInputs};
use crate::tokens_pool::account_registry::AccountDescriptor;
use crate::tokens_pool::session_dispatch_lock::{self, Exclusive};
use crate::tokens_pool::session_drift_removal::{self, DriftRemoval};
use crate::tokens_pool::session_hold::{now_unix_ms, OperatorHeld};
use crate::tokens_pool::session_lifecycle::{
    container_name, workspace_mount_roots, ContainerRunner, SessionLifecycle, STOP_GRACE,
};
pub use crate::tokens_pool::session_mount_gate::Denials;
use crate::tokens_pool::session_state::{
    container_running, is_private_clone, mount_drift, workspace_label, workspace_mounts,
    MountDrift, Snapshot,
};
use crate::workspace_registry::normalize_path;

/// Why a drifted container was not acted on this pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferReason {
    /// A `docker exec` is in flight, or a dispatch holds the dispatch lock.
    Busy,
    /// The dispatch lock could not be created, opened or queried.
    LockUnknown,
    /// The container is no longer the one the snapshot saw.
    Changed,
    /// The registry lists nothing under the container's workspace.
    NothingIntended,
    /// A registered root under the workspace is not a directory right now.
    RootUnavailable,
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
    /// The last deferral already logged above DEBUG.
    deferred: Option<DeferReason>,
    private_reported: bool,
    /// The standing removal record was already WARNed about.
    removal_reported: bool,
}

/// One running container's drift as the reconciler acts on it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Assessed {
    /// [`mount_drift`], with every `denied` mount added to `extra`.
    pub drift: MountDrift,
    /// Mounted paths the denials positively refuse. Empty when `denials` is
    /// `None` (unknown is not denied).
    pub denied: Vec<PathBuf>,
}

/// Assess `inspect` against `registered` and, when they could be read, the
/// `denials` for its workspace. Empty for a private-clone or unlabelled
/// container.
#[must_use]
pub fn assess(inspect: &Value, registered: &[PathBuf], denials: Option<&Denials>) -> Assessed {
    let mut drift = mount_drift(inspect, registered);
    let denied: Vec<PathBuf> = denials.map_or_else(Vec::new, |denials| {
        workspace_mounts(inspect)
            .into_iter()
            .filter(|mount| denials.check(std::slice::from_ref(mount)).is_err())
            .collect()
    });
    for mount in &denied {
        if !drift.extra.contains(mount) {
            drift.extra.push(mount.clone());
        }
    }
    drift.extra.sort();
    Assessed { drift, denied }
}

fn assess_for(inspect: &Value, inputs: &PassInputs<'_>) -> Option<Assessed> {
    let registered = inputs.registered?;
    let denials = workspace_label(inspect).and_then(|label| (inputs.denials_for)(label).ok());
    Some(assess(inspect, registered, denials.as_ref()))
}

/// The last [`priority`] class.
pub const LAST_CLASS: u8 = 2;

/// Processing order for one account's container: `0` running with `extra`
/// drift (a containment gap), `1` running with only `missing` drift, `2`
/// anything else (including an unavailable snapshot or an unknown registry).
#[must_use]
pub fn priority(snapshot: &Snapshot, container: &str, inputs: &PassInputs<'_>) -> u8 {
    let Some(inspect) = snapshot.inspect_of(container) else {
        return 2;
    };
    if !container_running(inspect) {
        return 2;
    }
    match assess_for(inspect, inputs) {
        Some(assessed) if !assessed.drift.extra.is_empty() => 0,
        Some(assessed) if !assessed.drift.missing.is_empty() => 1,
        _ => 2,
    }
}

/// Registered roots under `label` that are not directories right now.
/// [`workspace_mount_roots`] silently drops those, which would make their
/// mounts read as `extra`; "the path is missing at the moment" is not a
/// decision.
fn unavailable_roots(label: &Path, registered: &[PathBuf]) -> Vec<PathBuf> {
    let canonical = normalize_path(label);
    registered
        .iter()
        .filter(|root| root.starts_with(&canonical) && **root != canonical && !root.is_dir())
        .cloned()
        .collect()
}

/// Whether recreating against `workspace` would be refused: `/`, then the
/// one check `create` itself makes.
fn refusal(workspace: &Path, inputs: &PassInputs<'_>) -> anyhow::Result<()> {
    if workspace.parent().is_none() {
        anyhow::bail!("it would mount the whole filesystem");
    }
    (inputs.would_create_accept)(workspace)
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
             directory, `firewall: true`). Codex runs there with its own sandbox off",
            path.display()
        );
    }
    if !drift.missing.is_empty() {
        log::info!(
            "session_reconcile: {container} (account {name}) does not mount {} (registered \
             since it was created)",
            paths(&drift.missing)
        );
    }
    mem.reported = Some(drift.clone());
    mem.deferred = None;
}

/// Leave the container running this pass; say why once per reason.
fn defer(container: &str, reason: DeferReason, detail: &str, mem: &mut DriftMemory) -> Outcome {
    if mem.deferred == Some(reason) {
        log::debug!("session_reconcile: {container}: still deferred ({reason:?})");
    } else if reason == DeferReason::Busy {
        log::info!("session_reconcile: {container}: mounts drifted; {detail}");
    } else {
        log::warn!("session_reconcile: {container}: mounts drifted; {detail}");
    }
    mem.deferred = Some(reason);
    Outcome::DriftDeferred { reason }
}

fn unachievable(
    container: &str,
    name: &str,
    drift: MountDrift,
    why: &str,
    mem: &mut DriftMemory,
) -> Outcome {
    log::warn!(
        "session_reconcile: {container} (account {name}): mount drift the reconciler will not \
         act on ({why}; missing: {}; extra: {}). Leaving the container running and not retrying \
         until the drift changes; fix the workspace registry or recreate it by hand: \
         `loom-daemon accounts session stop {name}` then `accounts session start {name} \
         --mount-workspace <checkout parent>`",
        paths(&drift.missing),
        paths(&drift.extra)
    );
    mem.unachievable = Some(drift);
    Outcome::DriftUnachievable
}

/// The drift step for a running container the snapshot holds as `inspect`.
/// Returns [`Outcome::Running`] when its mounts match the registry, or when
/// the registry is unknown this pass.
#[allow(clippy::too_many_lines)]
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
    let (Some(registered), Some(label)) = (inputs.registered, workspace_label(inspect)) else {
        return Ok(Outcome::Running);
    };
    if is_private_clone(inspect) {
        return Ok(Outcome::Running);
    }
    let unavailable = unavailable_roots(label, registered);
    if !unavailable.is_empty() {
        let detail = format!(
            "not deciding while registered root(s) {} are not directories (unmounted volume?)",
            paths(&unavailable)
        );
        return Ok(defer(&container, DeferReason::RootUnavailable, &detail, mem));
    }
    let denials = (inputs.denials_for)(label);
    let Assessed { drift, denied } = assess(inspect, registered, denials.as_ref().ok());
    if drift.is_empty() {
        if mem.recreated.is_some() {
            log::info!("session_reconcile: {container}: mounts match the registry after recreate");
        }
        if denials.is_ok() {
            // Running, and positively nothing denied: a removal record (the
            // operator's own start normally deleted it already) is moot.
            session_drift_removal::clear(inputs.index.profiles(name));
        }
        *mem = DriftMemory::default();
        return Ok(Outcome::Running);
    }
    match (inputs.is_private_clone)(account) {
        Ok(false) => mem.private_reported = false,
        configured => {
            if let (Err(error), false) = (&configured, mem.private_reported) {
                log::warn!(
                    "session_reconcile: {container}: mounts drifted, but private-clone mode \
                     cannot be ruled out ({error:#}); never drift-recreated until it can"
                );
                mem.private_reported = true;
            }
            return Ok(Outcome::PrivateCloneSkipped);
        }
    }
    if denied.is_empty() && workspace_mount_roots(label, registered).is_err() {
        let detail = format!(
            "the workspace registry lists nothing under {} (emptied or truncated?); not \
             removing a container because nothing is intended",
            label.display()
        );
        return Ok(defer(&container, DeferReason::NothingIntended, &detail, mem));
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
        log::debug!("session_reconcile: {container}: drift unchanged and not acted on; skipped");
        return Ok(Outcome::DriftUnachievable);
    }
    mem.unachievable = None;
    report(&container, name, &drift, mem);
    let (workspace, image) = match inputs.index.last_start(name) {
        Some(start) => (start.workspace, Some(start.image)),
        None => (label.to_path_buf(), account_mem.image.clone()),
    };
    let mem = &mut account_mem.drift;
    let refused = refusal(&workspace, inputs).err();
    if let Some(error) = &refused {
        if denied.is_empty() {
            let why = format!("recreating against {} is refused: {error:#}", workspace.display());
            return Ok(unachievable(&container, name, drift, &why, mem));
        }
        if session_drift_removal::read(inputs.index.profiles(name)).is_some() {
            // Removed once for a denial and it is back (started by something
            // that does not share this pass's verdict). Never remove twice.
            let why = "it was already removed once for a denied mount and has been started again";
            return Ok(unachievable(&container, name, drift, why, mem));
        }
    }
    let snapshot_id = inspect["Id"].as_str().unwrap_or_default();
    match lifecycle.runner().inspect(&container)? {
        Some(now) if now.running && now.id == snapshot_id => {}
        _ => return Ok(defer(&container, DeferReason::Changed, "it changed since the read", mem)),
    }
    if lifecycle.runner().has_active_exec(&container)? {
        let detail = "an in-flight `docker exec`; deferring and re-checking next pass";
        return Ok(defer(&container, DeferReason::Busy, detail, mem));
    }
    let is_held = || inputs.index.is_held(name);
    if is_held() {
        return Err(OperatorHeld.into());
    }
    // Held until the container is gone or recreated: no dispatch can start
    // against it, and one that already started makes this defer.
    let _teardown = match session_dispatch_lock::try_exclusive(inputs.dispatch_locks, &container) {
        Exclusive::Acquired(lock) => lock,
        Exclusive::Busy => {
            let detail = "a dispatch is starting or running; deferring and re-checking next pass";
            return Ok(defer(&container, DeferReason::Busy, detail, mem));
        }
        Exclusive::Unknown(why) => {
            let detail = format!("the dispatch lock is unusable ({why}); not stopping it");
            return Ok(defer(&container, DeferReason::LockUnknown, &detail, mem));
        }
    };
    if let Some(error) = refused {
        let removal = DriftRemoval {
            schema_version: 1,
            workspace: workspace.clone(),
            denied: denied.clone(),
            reason: format!("{error:#}"),
            removed_at_unix_ms: now_unix_ms(),
        };
        if let Err(error) = session_drift_removal::record(&account.credential_reference, &removal) {
            let why = format!("the removal could not be recorded ({error:#}), so it is not made");
            return Ok(unachievable(&container, name, drift, &why, mem));
        }
        lifecycle.runner().stop_and_remove(&container, STOP_GRACE)?;
        log::warn!(
            "session_reconcile: {container} (account {name}) mounted {}, which it may not mount, \
             and no session container is accepted in its place ({error:#}); stopped and removed \
             it (it was idle). It stays down until the denial no longer applies, or until \
             `loom-daemon accounts session start {name} --mount-workspace {}`",
            paths(&denied),
            workspace.display()
        );
        mem.removal_reported = true;
        return Ok(Outcome::DriftRemoved { drift });
    }
    lifecycle.runner().stop_and_remove(&container, STOP_GRACE)?;
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

/// Whether a recorded fail-closed removal still forbids recreating `name`'s
/// missing container against `workspace`. It stands unless every input can
/// be read and says the denial is over: the registry is known, the denials
/// for the workspace load, `create` would accept the workspace, and none of
/// the recorded paths is still both denied and about to be mounted.
/// Clears the record (and returns `None`) once it no longer stands.
pub(super) fn removal_stands(
    name: &str,
    workspace: &Path,
    inputs: &PassInputs<'_>,
    account_mem: &mut AccountMemory,
) -> Option<Outcome> {
    let profiles = inputs.index.profiles(name);
    let removal = session_drift_removal::read(profiles)?;
    let over = || -> Option<()> {
        let registered = inputs.registered?;
        let denials = (inputs.denials_for)(workspace).ok()?;
        (inputs.would_create_accept)(workspace).ok()?;
        let intended = workspace_mount_roots(workspace, registered).ok()?;
        let still = |path: &PathBuf| {
            intended
                .iter()
                .any(|root| path.starts_with(root) || root.starts_with(path))
                && denials.check(std::slice::from_ref(path)).is_err()
        };
        (removal.schema_version == 1 && !removal.denied.iter().any(still)).then_some(())
    };
    let mem = &mut account_mem.drift;
    if over().is_some() {
        log::info!(
            "session_reconcile: {} (account {name}): the denied mount it was removed for no \
             longer applies; recreating it",
            container_name(name)
        );
        session_drift_removal::clear(profiles);
        mem.removal_reported = false;
        return None;
    }
    if !std::mem::replace(&mut mem.removal_reported, true) {
        log::warn!(
            "session_reconcile: {} (account {name}) stays down: it was removed for mounting {} \
             ({}), and that denial still stands or cannot be re-checked. Not recreating it; \
             start it by hand to override: `loom-daemon accounts session start {name} \
             --mount-workspace <checkout parent>`",
            container_name(name),
            paths(&removal.denied),
            removal.reason
        );
    }
    Some(Outcome::DriftRemovalStands)
}
