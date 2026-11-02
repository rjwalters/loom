//! Daemon-version coupling for the session reconcile pass (issue #11727
//! Slice 2).
//!
//! A session container keeps the `loom-daemon` of the image it was started
//! from. When the host daemon rolls and a host-side hook starts calling a
//! subcommand the container lacks, every Codex role run there fails. This
//! module heals that: a running container whose daemon is **older** than the
//! host's is recreated once idle, from the session image pinned to the host
//! version (`<repo>:<host version>`, never `:latest`), and no start or
//! recreate may use an image whose tag is older than the running container's
//! daemon or the host's (no downgrade).
//!
//! # Safety rules (the mount-drift rules of [`super::drift`], reused)
//!
//! 1. **Act only on a positive finding.** The host version must have been
//!    read (`PassInputs::host_version`), the container's version must parse
//!    from `docker exec <c> loom-daemon --version`, and the container's must
//!    be strictly lower. Anything unreadable, equal, or newer: no action.
//! 2. **Idle only.** A fresh inspect must show the same running container, no
//!    `docker exec` may be in flight, and the dispatch lock must be taken
//!    exclusively (held through the recreate). Busy: deferred, re-checked
//!    next pass. Never killed.
//! 3. **One attempt per (container version, host version).** A recreate that
//!    leaves the container still skewed is not retried until a version
//!    changes, so a broken recreate cannot churn an idle seat every pass.
//!    A pull that fails **before** the teardown has churned nothing: the seat
//!    is intact and the pinned tag is usually a release workflow that has not
//!    finished (the session image follows the tag by ~25 minutes), so the
//!    pull is retried with a doubling pass backoff
//!    ([`PULL_RETRY_MAX_SKIP`] passes at most) until it succeeds or a version
//!    changes (#13493).
//! 4. **Private-clone sessions** and accounts whose mode cannot be ruled out
//!    are never touched; the workspace registry must be readable.
//!
//! Versions are compared numerically per component (`0.19.624 < 0.19.1151`),
//! never as strings.

use std::cmp::Ordering;
use std::time::Duration;

use serde_json::Value;

use super::drift::DeferReason;
use super::{recreate_container, AccountMemory, Outcome, PassInputs};
use crate::tokens_pool::account_registry::AccountDescriptor;
use crate::tokens_pool::session_dispatch_lock::{self, Exclusive};
use crate::tokens_pool::session_hold::OperatorHeld;
use crate::tokens_pool::session_lifecycle::{
    container_name, ContainerRunner, SessionLifecycle, UndoLock, STOP_GRACE,
};
use crate::tokens_pool::session_state::{container_running, is_private_clone, workspace_label};

/// How long the `loom-daemon --version` probe may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// A dotted numeric daemon version (`0.19.1151`).
#[derive(Debug, Clone)]
pub struct DaemonVersion(Vec<u64>);

impl DaemonVersion {
    /// Parse `text` as exactly a version (`0.19.1151`, optionally followed by
    /// a `-`/`+` suffix). At least two components, so a commit hash is never
    /// mistaken for one.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let core = text.split(['-', '+']).next()?;
        let parts: Vec<u64> = core
            .split('.')
            .map(|p| if p.is_empty() { None } else { p.parse().ok() })
            .collect::<Option<_>>()?;
        (parts.len() >= 2).then_some(Self(parts))
    }

    /// The first version-looking token of `--version` output such as
    /// `loom-daemon 0.19.1152 (commit 2d5fe17, built …)`.
    #[must_use]
    pub fn from_version_output(output: &str) -> Option<Self> {
        output
            .split_whitespace()
            .filter(|t| t.starts_with(|c: char| c.is_ascii_digit()))
            .find_map(Self::parse)
    }

    /// The version a session image tag names (`repo:0.19.1151`). `None` for
    /// `:latest`, an untagged name, or a digest pin: they say nothing.
    #[must_use]
    pub fn from_image(image: &str) -> Option<Self> {
        let (_, tag) = image_repo_and_tag(image);
        Self::parse(tag?)
    }
}

impl PartialEq for DaemonVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for DaemonVersion {}

impl Ord for DaemonVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        let n = self.0.len().max(other.0.len());
        (0..n)
            .map(|i| {
                let a = self.0.get(i).copied().unwrap_or(0);
                let b = other.0.get(i).copied().unwrap_or(0);
                a.cmp(&b)
            })
            .find(|o| o.is_ne())
            .unwrap_or(Ordering::Equal)
    }
}

impl PartialOrd for DaemonVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for DaemonVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parts: Vec<String> = self.0.iter().map(u64::to_string).collect();
        f.write_str(&parts.join("."))
    }
}

/// `(repository, tag)` of an image reference; a digest pin has no tag, and a
/// registry port (`host:5000/img`) is not a tag.
///
/// A reference may carry both a tag and a digest (`repo:0.19.1209@sha256:…`,
/// which is what `docker inspect` and the last-start sidecar record for a
/// pulled-by-tag image). The repository is still everything before the tag:
/// returning `repo:0.19.1209` as the repository made [`pinned_image`] build
/// `repo:0.19.1209:<host>`, an invalid reference `docker pull` refuses, so a
/// skewed seat was never recreated (#13479). The tag is still reported as
/// `None` for a digest pin: the digest, not the tag, names the image, so it
/// says nothing [`refuse_downgrade`] may act on.
fn image_repo_and_tag(image: &str) -> (&str, Option<&str>) {
    let name = image.split('@').next().unwrap_or(image);
    let digest_pinned = name.len() != image.len();
    match name.rsplit_once(':') {
        Some((repo, tag)) if !tag.contains('/') => {
            (repo, if digest_pinned { None } else { Some(tag) })
        }
        _ => (name, None),
    }
}

/// `image` re-pinned to `host`: same repository, tag = the host version.
#[must_use]
pub fn pinned_image(image: &str, host: &DaemonVersion) -> String {
    format!("{}:{host}", image_repo_and_tag(image).0)
}

/// No downgrade: `image` is refused when its tag names a version older than
/// any of `floors` (the running container's daemon, the host's). An image
/// whose tag names no version (`:latest`, a digest) cannot be judged and is
/// allowed.
///
/// # Errors
/// The image is older than a floor.
pub fn refuse_downgrade(image: &str, floors: &[&DaemonVersion]) -> anyhow::Result<()> {
    let Some(tag) = DaemonVersion::from_image(image) else {
        return Ok(());
    };
    match floors.iter().find(|floor| tag < ***floor) {
        Some(floor) => anyhow::bail!(
            "image {image} carries loom-daemon {tag}, older than {floor}; refusing the downgrade"
        ),
        None => Ok(()),
    }
}

/// [`refuse_downgrade`] against this pass's host version (a no-op when the
/// host version was not read).
///
/// # Errors
/// The image is older than the host's daemon.
pub fn guard_image(inputs: &PassInputs<'_>, image: Option<&str>) -> anyhow::Result<()> {
    match (inputs.host_version.as_ref(), image) {
        (Some(host), Some(image)) => refuse_downgrade(image, &[host]),
        _ => Ok(()),
    }
}

/// Longest run of passes skipped between two pulls of a pinned image that is
/// not available yet: 30 passes is half an hour at the default interval.
pub const PULL_RETRY_MAX_SKIP: u32 = 30;

/// What the pass remembers about one account's version skew.
#[derive(Debug, Default, Clone)]
pub(super) struct SkewMemory {
    /// The container id and daemon version already probed (a running
    /// container's daemon cannot change).
    probed: Option<(String, DaemonVersion)>,
    /// The `(container, host)` versions a recreate was already attempted for.
    attempted: Option<(DaemonVersion, DaemonVersion)>,
    deferred: Option<DeferReason>,
    /// A pinned image the pass could not pull for `(container, host)`: how
    /// many pulls failed and how many passes remain to skip before the next.
    pull: Option<PullRetry>,
}

/// Backoff state for a pinned image that is not available yet (rule 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PullRetry {
    versions: (DaemonVersion, DaemonVersion),
    failures: u32,
    skip: u32,
}

impl PullRetry {
    /// The run of passes to skip after `failures` failed pulls: 1, 2, 4, …
    /// capped at [`PULL_RETRY_MAX_SKIP`].
    pub(super) fn skip_after(failures: u32) -> u32 {
        1u32.checked_shl(failures.saturating_sub(1))
            .unwrap_or(PULL_RETRY_MAX_SKIP)
            .min(PULL_RETRY_MAX_SKIP)
    }
}

/// Whether this pass should pull the pinned image for `versions` or sit out
/// the backoff. Counts the pass down when it sits out.
fn pull_due(mem: &mut SkewMemory, versions: &(DaemonVersion, DaemonVersion)) -> bool {
    match &mut mem.pull {
        Some(retry) if retry.versions == *versions && retry.skip > 0 => {
            retry.skip -= 1;
            false
        }
        _ => true,
    }
}

/// A pull of the pinned image for `versions` failed: schedule the next one.
fn note_pull_failure(mem: &mut SkewMemory, versions: (DaemonVersion, DaemonVersion)) -> u32 {
    let failures = match &mem.pull {
        Some(retry) if retry.versions == versions => retry.failures + 1,
        _ => 1,
    };
    let skip = PullRetry::skip_after(failures);
    mem.pull = Some(PullRetry {
        versions,
        failures,
        skip,
    });
    skip
}

fn probe<R: ContainerRunner>(
    lifecycle: &SessionLifecycle<R>,
    container: &str,
    id: &str,
    mem: &mut SkewMemory,
) -> Option<DaemonVersion> {
    if let Some((cached_id, version)) = &mem.probed {
        if cached_id == id {
            return Some(version.clone());
        }
    }
    let out = match lifecycle.runner().exec_capture(
        container,
        &["loom-daemon", "--version"],
        PROBE_TIMEOUT,
    ) {
        Ok(out) if out.success && !out.unavailable && !out.timed_out => out,
        Ok(out) => {
            log::debug!(
                "session_reconcile: {container}: `loom-daemon --version` did not answer \
                 (exit {:?}); no version-skew decision",
                out.exit_code
            );
            return None;
        }
        Err(error) => {
            log::debug!("session_reconcile: {container}: version probe failed ({error:#})");
            return None;
        }
    };
    let version = DaemonVersion::from_version_output(&out.output)?;
    mem.probed = Some((id.to_string(), version.clone()));
    Some(version)
}

fn defer(container: &str, reason: DeferReason, detail: &str, mem: &mut SkewMemory) -> Outcome {
    if mem.deferred == Some(reason) {
        log::debug!("session_reconcile: {container}: version skew still deferred ({reason:?})");
    } else {
        log::info!("session_reconcile: {container}: daemon older than the host's; {detail}");
    }
    mem.deferred = Some(reason);
    Outcome::SkewDeferred { reason }
}

/// The version-skew step for a healthy running container. `None`: nothing to
/// do (no positive finding, or nothing may be decided this pass).
#[allow(clippy::too_many_lines)]
pub(super) fn reconcile_skew<R: ContainerRunner>(
    lifecycle: &mut SessionLifecycle<R>,
    account: &AccountDescriptor,
    inspect: &Value,
    inputs: &PassInputs<'_>,
    account_mem: &mut AccountMemory,
) -> anyhow::Result<Option<Outcome>> {
    let name = account.id.name.as_str();
    let container = container_name(name);
    let (Some(host), Some(label), Some(_)) =
        (inputs.host_version.as_ref(), workspace_label(inspect), inputs.registered)
    else {
        return Ok(None);
    };
    if !container_running(inspect) || is_private_clone(inspect) {
        return Ok(None);
    }
    let id = inspect["Id"].as_str().unwrap_or_default();
    let image = inspect["Config"]["Image"].as_str().map(str::to_string);
    let mem = &mut account_mem.skew;
    let Some(have) = probe(lifecycle, &container, id, mem) else {
        return Ok(None);
    };
    if have >= *host {
        *mem = SkewMemory {
            probed: mem.probed.take(),
            ..SkewMemory::default()
        };
        return Ok(None);
    }
    if mem.attempted.as_ref() == Some(&(have.clone(), host.clone())) {
        log::debug!("session_reconcile: {container}: skew unchanged since the last recreate");
        return Ok(None);
    }
    if !pull_due(mem, &(have.clone(), host.clone())) {
        log::debug!(
            "session_reconcile: {container}: pinned image still backing off after a failed pull"
        );
        return Ok(None);
    }
    if !matches!((inputs.is_private_clone)(account), Ok(false)) {
        return Ok(None);
    }
    let (workspace, recorded_image) = match inputs.index.last_start(name) {
        Some(start) => (start.workspace, Some(start.image)),
        None => (label.to_path_buf(), image.clone()),
    };
    let Some(base) = recorded_image.or(image) else {
        return Ok(None);
    };
    let pinned = pinned_image(&base, host);
    if let Err(error) = refuse_downgrade(&pinned, &[&have, host]) {
        log::warn!("session_reconcile: {container}: not recreating for skew: {error:#}");
        return Ok(None);
    }
    if workspace.parent().is_none() || (inputs.would_create_accept)(&workspace).is_err() {
        log::warn!(
            "session_reconcile: {container}: daemon {have} is older than the host's {host}, but \
             a recreate against {} would be refused; left running",
            workspace.display()
        );
        mem.attempted = Some((have, host.clone()));
        return Ok(None);
    }
    match lifecycle.runner().inspect(&container)? {
        Some(now) if now.running && now.id == id => {}
        _ => {
            return Ok(Some(defer(
                &container,
                DeferReason::Changed,
                "it changed since the read",
                mem,
            )))
        }
    }
    if lifecycle.runner().has_active_exec(&container)? {
        let detail = "an in-flight `docker exec`; deferring and re-checking next pass";
        return Ok(Some(defer(&container, DeferReason::Busy, detail, mem)));
    }
    let is_held = || inputs.index.is_held(name);
    if is_held() {
        return Err(OperatorHeld.into());
    }
    let _teardown = match session_dispatch_lock::try_exclusive(inputs.dispatch_locks, &container) {
        Exclusive::Acquired(lock) => lock,
        Exclusive::Busy => {
            let detail = "a dispatch is starting or running; deferring and re-checking next pass";
            return Ok(Some(defer(&container, DeferReason::Busy, detail, mem)));
        }
        Exclusive::Unknown(why) => {
            let detail = format!("the dispatch lock is unusable ({why}); not stopping it");
            return Ok(Some(defer(&container, DeferReason::LockUnknown, &detail, mem)));
        }
    };
    mem.deferred = None;
    // The pinned tag must be on hand before the teardown: `docker run` would
    // pull it implicitly, but only after the seat is already gone. A failed
    // pull leaves the container running on its old daemon, and the seat has
    // not been touched, so it is retried after a backoff (rule 3, #13493):
    // the tag is usually a release workflow that has not published the
    // session image yet.
    if let Err(error) = lifecycle.runner().ensure_image(&pinned) {
        let skip = note_pull_failure(mem, (have, host.clone()));
        let failures = mem.pull.as_ref().map_or(1, |r| r.failures);
        if failures == 1 {
            log::warn!(
                "session_reconcile: {container}: not recreating for skew: {pinned} is not \
                 available ({error:#}); left running, pull retried after {skip} pass(es)"
            );
        } else {
            log::debug!(
                "session_reconcile: {container}: {pinned} still not available (pull {failures} \
                 failed: {error:#}); next pull after {skip} pass(es)"
            );
        }
        return Ok(None);
    }
    // Recorded before the teardown: a failure past this point is not retried
    // until a version changes (rule 3).
    mem.attempted = Some((have.clone(), host.clone()));
    mem.pull = None;
    lifecycle.runner().stop_and_remove(&container, STOP_GRACE)?;
    let undo = UndoLock::HeldByCaller;
    recreate_container(lifecycle, name, &workspace, Some(pinned.clone()), &is_held, undo)?;
    account_mem.awaiting_confirm = true;
    account_mem.skew.probed = None;
    log::warn!(
        "session_reconcile: {container}: its loom-daemon {have} is older than the host's {host}; \
         recreated it from {pinned} (--mount-workspace {})",
        workspace.display()
    );
    Ok(Some(Outcome::SkewRecreated {
        workspace,
        from: have,
        to: host.clone(),
    }))
}
