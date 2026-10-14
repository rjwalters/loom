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

use anyhow::Result;

use super::session_lifecycle::{check_mount_denials, firewalled_repo_paths, workspace_mount_roots};

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
