//! Per-seat Claude session containers (issue #14371, slice 1 of 3).
//!
//! The operator ruled (2AMLogic/2am#7380) that Claude steps run in a
//! persistent per-seat session container, one login per container, as Codex
//! does. A **seat** is a Claude account from the token pool whose container
//! `loom-claude-session-<name>` this module creates and manages, reusing the
//! Codex seam's [`ContainerRunner`] and the same hardened session image
//! (`docker/session/`, ADR-0017) rather than adding a dispatch path.
//!
//! This slice is registration + lifecycle only: `start` / `status` / `stop`.
//! Seat selection and the `docker exec` dispatch from `spawn-claude.sh` are
//! the next slice.
//!
//! # What a seat holds (and what it never does)
//!
//! * **Disjoint state.** Each seat owns `<seat root>/<name>/home`, bind-mounted
//!   at [`CONTAINER_CLAUDE_HOME`] and exported as `CLAUDE_CONFIG_DIR`, so its
//!   config, sessions and transcripts (everything Claude Code persists) are
//!   per seat. Two seats on one host share no directory.
//! * **No token on the container filesystem.** The OAuth token is *not*
//!   mounted, not passed in `docker run`, and the pool directory
//!   (`~/.loom/tokens`) is never mounted. The dispatcher passes the selected
//!   seat's token by name in the `docker exec` process environment only
//!   (slice 2). [`claude_seat_run_args`] is pure so a test can assert this.
//! * **Same posture as a Codex session container**: every capability dropped,
//!   `no-new-privileges`, no Docker socket, workspace narrowed to the
//!   registered repositories ([`super::session_mount_gate::create_roots`]).
//!
//! The Codex reconciler and `session_seats` only enumerate Codex accounts, so
//! a Claude seat is invisible to them by construction; `--restart
//! unless-stopped` brings it back after a Docker restart.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;

use super::account_registry::{
    account_inventory, account_matches_reference, AccountDescriptor, AccountProvider,
};
use super::session_image::{self, ExtraMount, SessionImageConfig};
use super::session_lifecycle::{
    gh_credential_dirs, gh_credential_owners, is_session_managed, mark_session_managed,
    ContainerRunner, ContainerState, DEFAULT_SESSION_IMAGE, EXTRA_MOUNTS_LABEL, SESSION_POSTURE,
    SESSION_POSTURE_LABEL, STOP_GRACE, WORKSPACE_LABEL,
};

/// Container-name prefix of a Claude seat: `loom-claude-session-<name>`,
/// parallel to Codex's `loom-codex-session-<name>`.
pub const CONTAINER_PREFIX: &str = "loom-claude-session-";

/// Where a seat's private Claude state is mounted inside the container. The
/// session image's user is `loom` (uid 1000, `docker/worker/MOUNT-CONTRACT.md` §3).
pub const CONTAINER_CLAUDE_HOME: &str = "/home/loom/.claude";

/// Overrides the machine-level seat-state root (default `~/.loom/claude-seats`).
/// An empty value disables seats.
pub const SEAT_ROOT_ENV: &str = "LOOM_CLAUDE_SEAT_ROOT";

/// Container label naming the runtime a session container serves.
pub const RUNTIME_LABEL: &str = "loom.session-runtime";
/// The [`RUNTIME_LABEL`] value of a Claude seat.
pub const RUNTIME: &str = "claude";

/// Container label and `containment=` telemetry value of a Claude seat, distinct
/// from the per-sweep `claude-ephemeral` shape `spawn-claude.sh` writes.
pub const CONTAINMENT_LABEL: &str = "loom.containment";
/// The [`CONTAINMENT_LABEL`] value of a Claude seat.
pub const CONTAINMENT: &str = "claude-session";

/// Subdirectory of a seat's directory that is mounted as its Claude home. The
/// adoption marker lives beside it, outside the mount.
const HOME_SUBDIR: &str = "home";

/// The machine-level seat-state root: `$LOOM_CLAUDE_SEAT_ROOT`, else
/// `~/.loom/claude-seats`; `None` when explicitly disabled (empty override).
///
/// Never inside a repository checkout: it holds a login's session state, and
/// the credential policy keeps those out of every worktree.
///
/// Under `cfg(test)` an unset variable resolves to `None` rather than the real
/// home, so a test cannot touch an operator's live seats; tests pass roots
/// explicitly.
#[must_use]
pub fn seat_root() -> Option<PathBuf> {
    match std::env::var(SEAT_ROOT_ENV) {
        Ok(value) if value.trim().is_empty() => None,
        Ok(value) => Some(PathBuf::from(value.trim())),
        #[cfg(test)]
        Err(_) => None,
        #[cfg(not(test))]
        Err(_) => dirs::home_dir().map(|home| home.join(".loom").join("claude-seats")),
    }
}

/// The container name of `name`'s seat. `name` must already be a validated
/// account name (letters, digits, `.`, `_`, `-`).
#[must_use]
pub fn container_name(name: &str) -> String {
    format!("{CONTAINER_PREFIX}{name}")
}

/// A seat's own directory (holds the adoption marker and [`home_dir`]).
#[must_use]
pub fn seat_dir(root: &Path, name: &str) -> PathBuf {
    root.join(name)
}

/// The directory mounted at [`CONTAINER_CLAUDE_HOME`]: the seat's whole
/// Claude state, disjoint from every other seat's.
#[must_use]
pub fn home_dir(root: &Path, name: &str) -> PathBuf {
    seat_dir(root, name).join(HOME_SUBDIR)
}

/// `true` iff `name` has been registered as a seat (adopted by `start`).
#[must_use]
pub fn is_seat(root: &Path, name: &str) -> bool {
    is_session_managed(&seat_dir(root, name))
}

/// The `docker run` argv of a seat container. Pure, so the "no token on the
/// container filesystem" and "disjoint state" properties are unit-testable.
///
/// Mounts: the seat's own `claude_home` read-write; `roots` (the workspace
/// narrowed to registered repositories) read-write at their own host paths
/// (`MOUNT-CONTRACT.md` §1 path parity); `credentials` (the daemon's GitHub
/// App token dirs) read-only; `extra_mounts` read-only. Deliberately absent:
/// the Claude token pool, the operator's `~/.config/gh`, the Docker socket,
/// host network/PID namespaces and any added capability.
#[must_use]
pub fn claude_seat_run_args(
    container: &str,
    image: &str,
    claude_home: &Path,
    workspace: &Path,
    roots: &[PathBuf],
    credentials: &[PathBuf],
    extra_mounts: &[ExtraMount],
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        container.into(),
        "--restart".into(),
        "unless-stopped".into(),
        "--cap-drop".into(),
        "ALL".into(),
        "--security-opt".into(),
        "no-new-privileges".into(),
        "-v".into(),
        format!("{}:{CONTAINER_CLAUDE_HOME}", claude_home.display()),
        "-e".into(),
        format!("CLAUDE_CONFIG_DIR={CONTAINER_CLAUDE_HOME}"),
    ];
    for root in roots {
        let root = root.display().to_string();
        args.push("-v".into());
        args.push(format!("{root}:{root}"));
    }
    for dir in credentials {
        let dir = dir.display().to_string();
        args.push("-v".into());
        args.push(format!("{dir}:{dir}:ro"));
    }
    for mount in extra_mounts {
        args.push("--mount".into());
        args.push(format!(
            "type=bind,src={},dst={},readonly",
            mount.host.display(),
            mount.container.display()
        ));
    }
    args.extend([
        "--label".into(),
        format!("{WORKSPACE_LABEL}={}", workspace.display()),
        "--label".into(),
        format!("{SESSION_POSTURE_LABEL}={SESSION_POSTURE}"),
        "--label".into(),
        format!("{RUNTIME_LABEL}={RUNTIME}"),
        "--label".into(),
        format!("{CONTAINMENT_LABEL}={CONTAINMENT}"),
    ]);
    if !extra_mounts.is_empty() {
        args.push("--label".into());
        args.push(format!(
            "{EXTRA_MOUNTS_LABEL}={}",
            session_image::extra_mounts_label_json(extra_mounts)
        ));
    }
    args.push(image.into());
    args
}

/// Create and start a seat container with the real `docker` CLI: the
/// `ProcessContainerRunner` half of [`ContainerRunner::create_claude_seat`].
///
/// # Errors
/// A mount root is refused, the workspace registry cannot narrow the mount, or
/// `docker run` fails.
pub fn docker_create_seat(
    container: &str,
    image: &str,
    claude_home: &Path,
    workspace: &Path,
    daemon_root: &Path,
    extra_mounts: &[ExtraMount],
) -> Result<()> {
    let roots = super::session_mount_gate::create_roots(workspace)?;
    let loom_workspace = std::env::var_os("LOOM_WORKSPACE").map(PathBuf::from);
    let owners = gh_credential_owners(workspace, daemon_root, loom_workspace.as_deref());
    let env_gh = std::env::var_os("GH_CONFIG_DIR").map(PathBuf::from);
    let credentials = gh_credential_dirs(&owners, env_gh.as_deref());
    super::session_mount_gate::check_extra_mounts(extra_mounts, workspace, &roots, &credentials)?;
    let args = claude_seat_run_args(
        container,
        image,
        claude_home,
        workspace,
        &roots,
        &credentials,
        extra_mounts,
    );
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let (success, _stdout, stderr) = super::docker_cli::run_bounded(
        "docker",
        &arg_refs,
        super::docker_cli::timeout_for(&arg_refs),
    )?;
    if !success {
        bail!("docker run {image} failed: {}", stderr.trim());
    }
    Ok(())
}

/// What `accounts session status --provider claude` reports. Secret-free.
#[derive(Debug, Clone, Serialize)]
pub struct SeatStatus {
    pub schema_version: u32,
    pub runtime: &'static str,
    pub containment: &'static str,
    pub name: String,
    pub container_name: String,
    pub running: bool,
    pub restarting: bool,
    pub container_id: Option<String>,
    pub started_at: Option<String>,
    pub image: Option<String>,
    /// The seat's Claude home on the host (mounted at `mount_path`).
    pub claude_home: PathBuf,
    pub mount_path: &'static str,
    /// Registered as a seat (adopted by a prior `start`).
    pub seat_registered: bool,
    pub workspace: Option<PathBuf>,
}

/// Resolve `reference` (a pool account name or registered email) to its
/// Claude [`AccountDescriptor`]; the single choke point so a raw email never
/// reaches [`container_name`] or `docker`.
fn find_claude_account(workspace: &Path, reference: &str) -> Result<AccountDescriptor> {
    account_inventory(workspace, AccountProvider::Claude)?
        .into_iter()
        .find(|account| account_matches_reference(account, reference))
        .ok_or_else(|| {
            anyhow!(
                "Claude account {reference:?} is not in the token pool (checked account names \
                 and registered emails); provision it first with `loom-daemon tokens bootstrap`"
            )
        })
}

/// Lifecycle of Claude seat containers over the shared [`ContainerRunner`] seam.
pub struct ClaudeSeatLifecycle<R> {
    workspace: PathBuf,
    seat_root: PathBuf,
    runner: R,
    image: String,
    session_config: Option<SessionImageConfig>,
}

impl<R: ContainerRunner> ClaudeSeatLifecycle<R> {
    /// `image` defaults to [`DEFAULT_SESSION_IMAGE`]; the CLI resolves the
    /// configured `accounts.session.image` override (#14019) before this.
    pub fn new(
        workspace: impl Into<PathBuf>,
        seat_root: impl Into<PathBuf>,
        runner: R,
        image: Option<String>,
    ) -> Self {
        Self {
            workspace: workspace.into(),
            seat_root: seat_root.into(),
            runner,
            image: image.unwrap_or_else(|| DEFAULT_SESSION_IMAGE.to_string()),
            session_config: None,
        }
    }

    /// Apply the resolved `accounts.session` settings (#14019): the extra
    /// read-only mounts ride every create, and the digest-pinned override
    /// image is bundle-verified before any create that uses it.
    #[must_use]
    pub fn with_session_config(mut self, config: Option<SessionImageConfig>) -> Self {
        self.session_config = config;
        self
    }

    fn extra_mounts(&self) -> Vec<ExtraMount> {
        self.session_config
            .as_ref()
            .map(|config| config.extra_mounts.clone())
            .unwrap_or_default()
    }

    /// Same rule as the Codex lifecycle: the bundle check fires only when the
    /// effective image *is* the configured override (an explicit `--image`
    /// keeps its historical behaviour).
    fn verify_session_config_image(&self) -> Result<()> {
        let Some(configured) = self
            .session_config
            .as_ref()
            .and_then(|config| config.image.as_deref())
        else {
            return Ok(());
        };
        if configured != self.image.as_str() {
            return Ok(());
        }
        session_image::verify_override_bundle(&self.image, &self.runner)
            .context("refusing to create a session container from the configured override image")
    }

    /// Register `name` as a seat and launch (or reuse, or resume) its
    /// container from the configured session image. One login per container:
    /// the container is named for, and mounts only, this account's state.
    ///
    /// Re-`start` of a running seat is idempotent; a seat running against a
    /// different workspace is refused (a bind mount cannot change without
    /// recreating the container).
    ///
    /// # Errors
    /// The account is not in the Claude token pool, the image fails its bundle
    /// verification, the container is crash-looping, or Docker fails.
    pub fn start(&self, name: &str, workspace: Option<&Path>) -> Result<SeatStatus> {
        let account = find_claude_account(&self.workspace, name)?;
        let name = account.id.name.as_str();
        let container = container_name(name);
        let requested = workspace.map_or_else(|| self.workspace.clone(), Path::to_path_buf);
        match self.runner.inspect(&container)? {
            Some(state) if state.running => check_workspace_match(name, &state, &requested)?,
            Some(state) if state.restarting => bail!(
                "seat {name:?} ({container}) is restarting — Docker is backing off before \
                 restarting a crashed container. Recreate it: `loom-daemon accounts session \
                 stop {name} --provider claude --force`, then start it again"
            ),
            Some(state) => {
                check_workspace_match(name, &state, &requested)?;
                self.runner.start_existing(&container)?;
            }
            None => {
                self.verify_session_config_image()?;
                let home = ensure_seat_dirs(&self.seat_root, name)?;
                self.runner.create_claude_seat(
                    &container,
                    &self.image,
                    &home,
                    &requested,
                    &self.workspace,
                    &self.extra_mounts(),
                )?;
            }
        }
        // Idempotent; also covers resuming a container whose state directory
        // was removed, so the marker below always has somewhere to land.
        ensure_seat_dirs(&self.seat_root, name)?;
        mark_session_managed(&seat_dir(&self.seat_root, name), &container)?;
        self.status(name)
    }

    /// Report the seat's container state.
    ///
    /// # Errors
    /// The account is not in the Claude token pool, or Docker cannot be asked.
    pub fn status(&self, name: &str) -> Result<SeatStatus> {
        let account = find_claude_account(&self.workspace, name)?;
        let name = account.id.name.as_str();
        let container = container_name(name);
        let state = self.runner.inspect(&container)?;
        Ok(SeatStatus {
            schema_version: 1,
            runtime: RUNTIME,
            containment: CONTAINMENT,
            name: name.to_string(),
            container_name: container,
            running: state.as_ref().is_some_and(|s| s.running),
            restarting: state.as_ref().is_some_and(|s| s.restarting),
            container_id: state.as_ref().map(|s| s.id.clone()),
            started_at: state.as_ref().and_then(|s| s.started_at.clone()),
            image: state.as_ref().and_then(|s| s.image.clone()),
            claude_home: home_dir(&self.seat_root, name),
            mount_path: CONTAINER_CLAUDE_HOME,
            seat_registered: is_seat(&self.seat_root, name),
            workspace: state.and_then(|s| s.workspace),
        })
    }

    /// Tear the container down cleanly (`docker stop` then `rm`). Refuses an
    /// in-flight `docker exec` unless `force` — the restart-safety contract
    /// never SIGKILLs active work. The seat stays registered and its state
    /// directory is kept: a later `start` resumes the same login.
    ///
    /// # Errors
    /// An exec is in flight and `force` is unset, or Docker fails.
    pub fn stop(&self, name: &str, force: bool) -> Result<SeatStatus> {
        let account = find_claude_account(&self.workspace, name)?;
        let name = account.id.name.as_str();
        let container = container_name(name);
        if let Some(state) = self.runner.inspect(&container)? {
            if !force && state.running && self.runner.has_active_exec(&container)? {
                bail!(
                    "seat {name:?} has an in-flight `docker exec` (an active Claude step); \
                     refusing to stop it. Wait for it to finish, or pass --force"
                );
            }
            self.runner.stop_and_remove(&container, STOP_GRACE)?;
        }
        self.status(name)
    }
}

fn check_workspace_match(name: &str, state: &ContainerState, requested: &Path) -> Result<()> {
    if let Some(existing) = &state.workspace {
        if existing != requested {
            bail!(
                "seat {name:?} is already running with workspace {} mounted; stop it first \
                 (`loom-daemon accounts session stop {name} --provider claude`) before starting \
                 it against a different workspace ({})",
                existing.display(),
                requested.display()
            );
        }
    }
    Ok(())
}

/// Create the seat's directory tree (owner-only) and return the mounted home.
fn ensure_seat_dirs(root: &Path, name: &str) -> Result<PathBuf> {
    let home = home_dir(root, name);
    std::fs::create_dir_all(&home)
        .with_context(|| format!("failed to create seat state directory {}", home.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for dir in [seat_dir(root, name), home.clone()] {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("failed to restrict {}", dir.display()))?;
        }
    }
    Ok(home)
}

#[cfg(test)]
mod tests;
