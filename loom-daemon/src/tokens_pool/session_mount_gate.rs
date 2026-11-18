//! The one acceptance check for a host-mode session container's workspace
//! mounts (issue #10364 Part B): "would `session start --mount-workspace
//! <workspace>` create this container, and with which roots?"
//!
//! [`ProcessContainerRunner::create`](super::session_lifecycle::ProcessContainerRunner)
//! calls [`create_roots`] to decide what to mount, and the session
//! reconciler calls the **same function with the same argument** before it
//! tears a drifted container down. There is deliberately no second
//! implementation: when the two could answer differently (they once read the
//! firewall roster for different directories), a container was removed that
//! `create` then brought straight back, every other pass.
//!
//! Every input that cannot be read is a refusal here, never "all allowed":
//! an unreadable fleet roster is an error ([`Denials::load`]), and an
//! unreadable workspace registry is an empty one, which
//! [`workspace_mount_roots`] refuses for a checkout parent.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use super::private_workspace::bundle::CONTROL_ROOT;
use super::session_image::ExtraMount;
use super::session_lifecycle::{
    check_mount_denials, firewalled_repo_paths, workspace_mount_roots, CONTAINER_CODEX_HOME,
};

/// What a session container may not mount whatever the registry says: the
/// inputs of [`check_mount_denials`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Denials {
    pub home: Option<PathBuf>,
    /// `firewall: true` repository paths from the cached fleet roster.
    pub firewalled: Vec<PathBuf>,
}

impl Denials {
    /// The denials `session start --mount-workspace <workspace>` applies.
    ///
    /// # Errors
    /// When a cached fleet roster exists but cannot be read or parsed: the
    /// firewall verdict is then unknown, which callers must treat as neither
    /// "allowed" nor "denied".
    pub fn load(workspace: &Path) -> Result<Self> {
        Ok(Self {
            home: dirs::home_dir(),
            firewalled: firewalled_repo_paths(workspace)?,
        })
    }

    /// `Err` names the first of `roots` that is denied.
    ///
    /// # Errors
    /// When a root is `/`, the home directory or an ancestor of it, or
    /// overlaps a firewalled repository.
    pub fn check(&self, roots: &[PathBuf]) -> Result<()> {
        check_mount_denials(roots, self.home.as_deref(), &self.firewalled)
    }
}

/// The roots a container for `workspace` mounts given `registered`, or why
/// none would be accepted.
///
/// # Errors
/// When [`workspace_mount_roots`] refuses the workspace, the roster cannot be
/// read, or a root is denied.
pub fn accepted_roots(workspace: &Path, registered: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let roots = workspace_mount_roots(workspace, registered)?;
    Denials::load(workspace)?.check(&roots)?;
    Ok(roots)
}

/// [`accepted_roots`] against the daemon's workspace registry as it is on
/// disk now — exactly what `create` mounts. An unreadable registry is an
/// empty one (issue #9979), so a checkout parent fails closed.
///
/// # Errors
/// See [`accepted_roots`].
pub fn create_roots(workspace: &Path) -> Result<Vec<PathBuf>> {
    let registered = crate::workspace_registry::WorkspaceRegistry::load_default()
        .map(|registry| registry.roots())
        .unwrap_or_default();
    accepted_roots(workspace, &registered)
}

/// Refuse the configured extra read-only mounts (issue #14019) a session
/// container must never receive, extending the one mount denylist
/// ([`check_mount_denials`]) rather than adding a second:
///
/// * **Host source** — passes the same denials the workspace roots do
///   ([`check_mount_denials`]): never `/`, the home directory, an ancestor
///   of it, or anything overlapping a `firewall: true` repository. The bind
///   is read-only, but a read-only copy of `~/.ssh` is still a copy.
/// * **Container target** — never shadows, and is never shadowed-by-or-
///   nested-in, the workspace mounts (path parity), the `CODEX_HOME`
///   profile (whose three control files are read-only mount points), the
///   sealed control bundle ([`CONTROL_ROOT`]), or a credential path.
/// * **Among themselves** — no two extra mounts overlap: Docker mounts
///   them in argv order, so a nested pair would make one of the two
///   silently not what the operator asked for.
///
/// # Errors
/// The first offending mount, naming it.
pub fn check_extra_mount_denials(
    mounts: &[ExtraMount],
    home: Option<&Path>,
    firewalled: &[PathBuf],
    workspace_roots: &[PathBuf],
    credential_dirs: &[PathBuf],
) -> Result<()> {
    let normalize = crate::workspace_registry::normalize_path;
    let sources: Vec<PathBuf> = mounts.iter().map(|mount| mount.host.clone()).collect();
    check_mount_denials(&sources, home, firewalled)?;
    let mut forbidden: Vec<PathBuf> = workspace_roots
        .iter()
        .chain(credential_dirs.iter())
        .map(|root| normalize(root))
        .collect();
    forbidden.push(PathBuf::from(CONTAINER_CODEX_HOME));
    forbidden.push(PathBuf::from(CONTROL_ROOT));
    let mut destinations: Vec<PathBuf> = Vec::with_capacity(mounts.len());
    for mount in mounts {
        // The raw reference must be absolute: `normalize_path` would
        // silently anchor a relative one to the daemon's own cwd.
        if !mount.container.to_string_lossy().starts_with('/') {
            bail!(
                "refusing the extra read-only mount {} -> {}: the container target must be an \
                 absolute path other than / (issue #14019)",
                mount.host.display(),
                mount.container.display()
            );
        }
        let target = normalize(Path::new(&mount.container));
        if target == Path::new("/") {
            bail!(
                "refusing the extra read-only mount {} -> {}: the container target must be an \
                 absolute path other than / (issue #14019)",
                mount.host.display(),
                mount.container.display()
            );
        }
        for shield in &forbidden {
            if target.starts_with(shield) || shield.starts_with(&target) {
                bail!(
                    "refusing the extra read-only mount {} -> {}: it shadows or shadows-on \
                     {}, which a session container must not have remounted (issue #14019)",
                    mount.host.display(),
                    mount.container.display(),
                    shield.display()
                );
            }
        }
        destinations.push(target);
    }
    for (index, target) in destinations.iter().enumerate() {
        for other in &destinations[index + 1..] {
            if target.starts_with(other) || other.starts_with(target) {
                bail!(
                    "refusing overlapping extra read-only mounts at {} and {}; Docker would \
                     mount them in argv order and one of the two would not be what the \
                     operator asked for (issue #14019)",
                    target.display(),
                    other.display()
                );
            }
        }
    }
    Ok(())
}

/// [`check_extra_mount_denials`] with the denials a real `create` would
/// apply ([`Denials::load`]) — the exact gate
/// [`super::session_lifecycle::ProcessContainerRunner::create`] runs before
/// building its `docker run` argv, so a mount that would be refused never
/// reaches Docker.
///
/// # Errors
/// See [`check_extra_mount_denials`]; also when the cached fleet roster
/// exists but cannot be read (fail closed, like [`Denials::load`]).
pub fn check_extra_mounts(
    mounts: &[ExtraMount],
    workspace: &Path,
    roots: &[PathBuf],
    credentials: &[PathBuf],
) -> Result<()> {
    let denials = Denials::load(workspace)?;
    check_extra_mount_denials(
        mounts,
        denials.home.as_deref(),
        &denials.firewalled,
        roots,
        credentials,
    )
}

/// [`check_extra_mounts`] for a private-clone seat (#14451): the host source
/// passes the same denials, and the container target may not shadow or nest
/// inside any of `reserved_targets` — the seat's own `/workspace`, `CODEX_HOME`
/// profile and forge-configuration mounts (the sealed bundle root is always
/// reserved). A private seat mounts none of the host-mode workspace roots or
/// credential directories, so those do not apply.
///
/// # Errors
/// See [`check_extra_mount_denials`]; also when the cached fleet roster exists
/// but cannot be read (fail closed).
pub fn check_private_extra_mounts(
    mounts: &[ExtraMount],
    workspace: &Path,
    reserved_targets: &[PathBuf],
) -> Result<()> {
    let denials = Denials::load(workspace)?;
    check_extra_mount_denials(
        mounts,
        denials.home.as_deref(),
        &denials.firewalled,
        reserved_targets,
        &[],
    )
}
