//! `loom-daemon accounts session start|stop|status|attach` (Issue #6925,
//! Epic #6896 Phase 2): a per-account **session container** lifecycle layered
//! on top of the existing Codex account-profile store
//! ([`super::account_lifecycle`]).
//!
//! This is deliberately *not* a new credential store: a session is identified
//! entirely by the account name it wraps, resolved through
//! [`super::account_registry::account_inventory`] exactly the way
//! `loom-daemon accounts status <name>` already does. What this module adds
//! is the container half — start/stop/status/attach a long-lived
//! `ghcr.io/rjwalters/loom-worker-session` container that mounts the
//! account's `CODEX_HOME` profile directory, per the mount contract
//! (`docker/worker/MOUNT-CONTRACT.md` §2/§3) and the image's own `CODEX_HOME`
//! convention (`docker/session/README.md`).
//!
//! # Workspace mount and `shell` (Issue #7389, Epic #6896 Phase 2)
//!
//! `start` also bind-mounts a workspace root at the identical absolute host
//! path (`docker/worker/MOUNT-CONTRACT.md` section 1, "path parity" --
//! load-bearing for git worktrees) and records it in a container label, so
//! `status` can report it and a later `start` against a *different*
//! workspace fails clearly instead of silently reusing the old mount.
//! `shell` is the "start-if-absent, run Codex, attach" composite the
//! operator's `codex-agent <account>` alias execs into: it launches (or
//! reuses) a tmux window running `codex` cwd'd to the mounted workspace and
//! attaches to it -- detaching leaves Codex running, and re-running `shell`
//! re-attaches to that same window instead of stacking a second `codex`
//! process.
//!
//! `<account>` accepts either the account's short profile name or its
//! registered email (see
//! [`super::account_registry::account_matches_reference`]) -- resolution to
//! the short name happens in [`find_codex_account`] before the result ever
//! reaches [`container_name`] or any `docker` call, since Docker container
//! names reject `@`.
//!
//! # Ownership rule (ADR-0017 Decision 1's Phase 2 negative consequence)
//!
//! Once a profile has been adopted by `start` (marked with the sentinel file
//! [`SESSION_MARKER_FILE`] inside the profile directory), it must refuse
//! **host-direct** `CODEX_HOME` use forever after — i.e. no ambient host CLI
//! process (`codex login`, `codex login status`) may touch the same volume
//! concurrently with the container that now owns it, since a session
//! container is the single serializing owner of that account's `auth.json`
//! refresh chain. [`super::account_lifecycle`] enforces this at its two
//! direct-`CODEX_HOME` call sites (`reauth`, and the `status`/`list` login
//! probe) via [`is_session_managed`].
//!
//! # Restart-safety (the #5119 contract, extended by ADR-0017 Decision 4)
//!
//! `stop` never sends a raw SIGKILL to a container with an in-flight `docker
//! exec`. It uses `docker stop` (SIGTERM, then a bounded grace period) only
//! after confirming — via [`ContainerRunner::has_active_exec`] — that no
//! exec'd process is currently running beyond the container's own baseline
//! (the `tini` init, the blocking `sleep infinity`, and the idle tmux
//! server/pane the session entrypoint starts). A caller that wants to
//! override this refusal passes `--force`, mirroring the daemon's existing
//! `--force-after-timeout` escape hatch on `restart --drain`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;

use super::account_lifecycle::{classify_login_status, login_state_from, LoginState, RunnerOutput};
use super::account_registry::{
    account_inventory, account_matches_reference, AccountDescriptor, AccountProvider,
};
use super::health::{self, ProbeEffect, ProbeOutcome};

/// Batched, fail-open "is this account's session container running?" read
/// (Issue #10454) — what lets selection pass over a down session account.
pub mod liveness;

/// Default image this lifecycle launches session containers from
/// (`docker/session/README.md`). Overridable per-invocation (`--image`) for
/// tests and for an operator pinning a specific published tag.
pub const DEFAULT_SESSION_IMAGE: &str = "ghcr.io/rjwalters/loom-worker-session:latest";

/// The session image's fixed `CODEX_HOME` mount point
/// (`docker/session/README.md` § "`CODEX_HOME` mount contract").
const CONTAINER_CODEX_HOME: &str = "/home/loom/.codex-profile";

/// The session image's fixed uid/gid (`docker/worker/MOUNT-CONTRACT.md` §3).
/// Advisory only (see [`uid_matches_image`]) — never a hard `start` failure,
/// since the invoking host user is not necessarily the account-profile
/// owner on every install shape.
const SESSION_IMAGE_UID: u32 = 1000;

/// Default tmux session name the session image's entrypoint creates
/// (`docker/session/entrypoint.sh`'s `$LOOM_SESSION_TMUX_NAME` default).
const DEFAULT_TMUX_SESSION_NAME: &str = "session";

/// Name of the tmux window `shell` creates/reuses for its Codex run. Fixed
/// (not per-invocation) so re-running `shell` can detect and re-attach to
/// the *same* window instead of stacking a second `codex` process.
const CODEX_WINDOW_NAME: &str = "codex";

/// Default `codex` arguments `shell` uses when the caller passes none — the
/// operator's own bare-metal invocation being replaced by this feature. The
/// ADR-0017 sandbox-posture caveats that constrain *dispatch* do not apply
/// to an operator's own interactive session.
const DEFAULT_CODEX_SHELL_ARGS: &[&str] = &["--yolo"];

/// Container label `create` sets to the parity-mounted workspace's absolute
/// host path (Issue #7389). Read back by `inspect` so `status` can report it
/// and `start` can detect a mismatched re-`start` against a different
/// workspace.
pub(crate) const WORKSPACE_LABEL: &str = "loom.workspace";

/// Container label recording the security posture a host-mode session
/// container was created with (issue #9979). `spawn-codex.sh` reads it and
/// only runs Codex with its own sandbox off inside a container that carries
/// [`SESSION_POSTURE`]; a container created before the hardening below has
/// no such label and is refused until it is recreated.
pub const SESSION_POSTURE_LABEL: &str = "loom.session-posture";

/// The current posture: the container is Codex's only boundary, so it runs
/// with every capability dropped, `no-new-privileges`, Docker's default
/// seccomp/AppArmor profile, and exactly the mounts
/// [`host_session_run_args`] lists. Bump the suffix whenever that set
/// changes in a way existing containers must be recreated to pick up.
pub const SESSION_POSTURE: &str = "container-boundary-v1";

/// The directories a host-mode session container bind-mounts read-write
/// (issue #9979). `workspace` is the `--mount-workspace` the operator asked
/// for; `registered` is the daemon's workspace registry
/// (`~/.loom/workspaces.json`).
///
/// * A `workspace` that is itself a registered root, or a git checkout, is
///   mounted as-is: it is one repository.
/// * Otherwise `workspace` is a checkout PARENT (`~/GitHub`), and mounting it
///   whole would hand every Codex role read-write access to every directory
///   under it — including repositories that are not in the fleet at all
///   (firewalled, clean-room or personal ones). Only the registered roots
///   under it are mounted, each at its own path (path parity, so worktrees
///   under `<root>/.loom/worktrees` resolve unchanged). A root nested inside
///   another mounted root is covered by its parent's mount.
/// * A parent with no registered roots under it is refused rather than
///   mounted wholesale — failing closed is the point.
pub fn workspace_mount_roots(workspace: &Path, registered: &[PathBuf]) -> Result<Vec<PathBuf>> {
    // Registry roots are stored canonical; compare against the canonical
    // form of what the operator typed (`~/GitHub/`, a symlink, `..`).
    let canonical = crate::workspace_registry::normalize_path(workspace);
    if registered.contains(&canonical) || workspace.join(".git").exists() {
        return Ok(vec![workspace.to_path_buf()]);
    }
    let mut roots: Vec<PathBuf> = registered
        .iter()
        .filter(|root| root.starts_with(&canonical) && **root != canonical)
        .filter(|root| root.is_dir())
        .cloned()
        .collect();
    roots.sort();
    roots.dedup();
    let mut kept: Vec<PathBuf> = Vec::with_capacity(roots.len());
    for root in roots {
        if !kept.iter().any(|parent| root.starts_with(parent)) {
            kept.push(root);
        }
    }
    if kept.is_empty() {
        bail!(
            "{} is neither a git checkout nor a parent of any repository in the workspace \
             registry (~/.loom/workspaces.json); refusing to mount it whole into a session \
             container, where Codex runs with its own sandbox off (issue #9979). Register the \
             repositories first, or pass --mount-workspace <repo>",
            workspace.display()
        );
    }
    Ok(kept)
}

/// The candidate daemon roots whose App-token dirs a host-mode session
/// container may mount ([`gh_credential_dirs`]), in order, without
/// duplicates:
///
/// 1. `mount_workspace`: the `--mount-workspace` path the container gets.
/// 2. `registry_workspace`: the accounts registry's own workspace (the
///    `--workspace` daemon root whose `.loom/accounts.json` `session start`
///    updates). This is where `.loom/gh-config` lives when an operator runs
///    `cd ~/GitHub/loom && loom-daemon accounts session start <name>
///    --mount-workspace ~/GitHub` by hand. Before issue #10103 it was not
///    consulted, so a session started without `LOOM_WORKSPACE` mounted no
///    token dir and posture reported `gh=skip`.
/// 3. `loom_workspace`: the daemon's `LOOM_WORKSPACE`, when set.
#[must_use]
pub fn gh_credential_owners(
    mount_workspace: &Path,
    registry_workspace: &Path,
    loom_workspace: Option<&Path>,
) -> Vec<PathBuf> {
    let mut owners: Vec<PathBuf> = Vec::new();
    for owner in [
        Some(mount_workspace),
        Some(registry_workspace),
        loom_workspace,
    ]
    .into_iter()
    .flatten()
    {
        if !owner.as_os_str().is_empty() && !owners.iter().any(|known| known == owner) {
            owners.push(owner.to_path_buf());
        }
    }
    owners
}

/// The daemon-owned GitHub App token directories a host-mode session
/// container mounts **read-only** so `gh` inside it can authenticate (issue
/// #9979). `spawn-codex.sh` forwards the dispatch's own `GH_CONFIG_DIR` (a
/// path, never a token) into the container; that path is
/// `<daemon root>/.loom/gh-config` or `<daemon root>/.loom/gh-config-by-owner/<owner>`
/// ([`crate::credential_preflight::github_app_gh_config_dir`] and
/// `_for_owner`), and the daemon root is not always under the mounted
/// repositories (the workers' daemon runs from `~/loom-daemon`).
///
/// Only those two daemon-shaped directories are ever returned — never an
/// arbitrary `GH_CONFIG_DIR`, which in an operator's shell may be a personal
/// `gh` login. `owners` are candidate daemon roots; `env_gh_config` names one
/// more root when it has the daemon's shape. A directory inside a mounted
/// (read-write) repository is still returned: its `:ro` bind overlays the
/// repository mount (Docker mounts the deeper destination last), so the
/// token dir is read-only inside the container even when the daemon root is
/// itself a registered repository (robb-studio's `~/GitHub/loom`). Directory
/// (not file) binds, so the daemon's atomic hosts.yml refresh stays visible.
#[must_use]
pub fn gh_credential_dirs(owners: &[PathBuf], env_gh_config: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = owners.to_vec();
    if let Some(dir) = env_gh_config {
        let loom_dir = match dir.file_name().and_then(|name| name.to_str()) {
            Some("gh-config") => dir.parent(),
            _ if dir
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "gh-config-by-owner") =>
            {
                dir.parent().and_then(Path::parent)
            }
            _ => None,
        };
        if let Some(root) = loom_dir
            .filter(|loom| loom.file_name().is_some_and(|name| name == ".loom"))
            .and_then(Path::parent)
        {
            candidates.push(root.to_path_buf());
        }
    }
    // A real directory at a real `.loom/`, never through a symlink: an owner
    // root is usually inside a read-write session mount, so a session could
    // otherwise point `.loom/gh-config` (or `.loom` itself) at any host
    // directory and have the next `session start` bind it into a container.
    let real_dir = |path: &Path| {
        path.symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_dir())
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    for root in candidates {
        if !real_dir(&root.join(".loom")) {
            continue;
        }
        for dir in [
            crate::credential_preflight::github_app_gh_config_dir(&root),
            root.join(".loom").join("gh-config-by-owner"),
        ] {
            if real_dir(&dir) && !dirs.contains(&dir) {
                dirs.push(dir);
            }
        }
    }
    dirs
}

/// Refuse mount roots a host-mode session container must never receive,
/// whatever the registry says (issue #9979). With Codex's sandbox off, every
/// mounted file is readable and writable by the model, so this is an explicit
/// deny rather than a consequence of what happens to be registered:
///
/// * `/`, the home directory, or any ancestor of it — that would expose
///   `~/.ssh`, `~/.aws`, `~/.config/gh`, `~/.loom/tokens`, `~/.cloudflare`.
///   (A home directory that is a git checkout, or a registered `~`, would
///   otherwise pass [`workspace_mount_roots`] as "one repository".)
/// * any root that contains, or lies inside, a `firewalled` repository
///   (`firewall: true` in the fleet roster, [`firewalled_repo_paths`]).
pub fn check_mount_denials(
    roots: &[PathBuf],
    home: Option<&Path>,
    firewalled: &[PathBuf],
) -> Result<()> {
    let normalize = crate::workspace_registry::normalize_path;
    let home = home.map(normalize);
    for root in roots {
        let root = normalize(root);
        if root.parent().is_none() || home.as_ref().is_some_and(|h| h.starts_with(&root)) {
            bail!(
                "refusing to mount {} into a session container: it is the filesystem root, the \
                 home directory, or an ancestor of it, and Codex runs with its own sandbox off \
                 there (issue #9979). Pass --mount-workspace <checkout parent or repo>",
                root.display()
            );
        }
        for wall in firewalled {
            let wall = normalize(wall);
            if wall.starts_with(&root) || root.starts_with(&wall) {
                bail!(
                    "refusing to mount {} into a session container: it overlaps {}, which the \
                     fleet roster marks firewall: true (issue #9979)",
                    root.display(),
                    wall.display()
                );
            }
        }
    }
    Ok(())
}

/// The `firewall: true` repository paths in the fleet roster (`repos.yml`),
/// read from the fleet-store cache the daemon's sync already keeps — never
/// fetched here, and deny-only, so a stale cache can only under-deny, never
/// widen a mount (issue #9979). `Ok(empty)` when no store is configured or
/// nothing is cached yet; `Err` when a cached roster exists but cannot be
/// read or parsed, so a broken firewall input fails closed.
pub fn firewalled_repo_paths(workspace: &Path) -> Result<Vec<PathBuf>> {
    use crate::fleet_store::{self as store, fetch, roster};
    let effective = crate::config_resolver::resolve_effective_config(workspace);
    let Some(location) = store::resolve_location(&effective, &|k| std::env::var(k).ok())? else {
        return Ok(Vec::new());
    };
    let cache = store::default_cache_dir(&location)?;
    let Some(snapshot) = fetch::read_cache(&cache, &location)? else {
        return Ok(Vec::new());
    };
    let Some(text) = snapshot.text(store::ROSTER_PATH)? else {
        return Ok(Vec::new());
    };
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    let parsed = roster::parse(&text, &home).context("fleet roster (firewall input)")?;
    Ok(parsed
        .records
        .iter()
        .filter(|record| record.firewall)
        .map(|record| parsed.root.join(&record.dir))
        .collect())
}

/// The account profile's hook-control files — hook registration
/// (`hooks.json`), Codex's own trust state (`config.toml`) and Loom's
/// readiness receipt (`loom-codex-hooks.json`) — the same set a private-clone
/// session freezes ([`super::private_workspace::bundle::PROFILE_CONTROLS`]).
pub use super::private_workspace::bundle::PROFILE_CONTROLS;

/// Container path of one [`PROFILE_CONTROLS`] entry in a host-mode session.
#[must_use]
pub fn profile_control_destination(name: &str) -> String {
    format!("{CONTAINER_CODEX_HOME}/{name}")
}

/// Content a missing [`PROFILE_CONTROLS`] file is created with before a
/// host-mode session is created, so each one can be bound read-only (Docker
/// cannot bind a file that does not exist). Each placeholder means "nothing
/// registered, nothing trusted, nothing pinned": an empty `config.toml`, a
/// `hooks.json` with no hooks, and a receipt pinning nothing, which `verify`
/// reads as not ready. Without the bind, a session with Codex's sandbox off
/// could CREATE the absent file — a registration, receipt and trust entry of
/// its own — for the next session to read.
const PROFILE_CONTROL_PLACEHOLDERS: [(&str, &str); 3] = [
    ("hooks.json", "{\"hooks\":{}}\n"),
    ("config.toml", ""),
    ("loom-codex-hooks.json", "{}\n"),
];

/// Create every missing [`PROFILE_CONTROLS`] file in `profile` (mode 0600)
/// with its placeholder; existing files are left byte-for-byte alone.
pub fn ensure_profile_controls(profile: &Path) -> Result<()> {
    for (name, placeholder) in PROFILE_CONTROL_PLACEHOLDERS {
        let path = profile.join(name);
        if path.symlink_metadata().is_ok() {
            continue;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options
            .open(&path)
            .with_context(|| format!("creating placeholder {}", path.display()))?;
        std::io::Write::write_all(&mut file, placeholder.as_bytes())
            .with_context(|| format!("writing placeholder {}", path.display()))?;
    }
    Ok(())
}

/// The full `docker run` argv for a host-mode session container (issue
/// #9979). Pure, so the posture is unit-testable without Docker.
///
/// What the container can reach, and nothing else:
/// * `codex_home` read-write at the image's fixed `CODEX_HOME` (the
///   account's own `auth.json` refresh chain; ADR-0017 Decision 1) —
///   except each of [`PROFILE_CONTROLS`], bound **read-only over its own
///   path** from the identically-named host file, as a private-clone session
///   binds them. With Codex's sandbox off a session could otherwise rewrite
///   its own hook registration or trust state (say, change the hook's
///   matcher: the command Loom pins is unchanged, but Codex's trust hash
///   covers the whole group, so Codex skips the hook) and the NEXT session
///   would read guard-ready while running unhooked. A mount point is immune
///   to write (`EROFS`), unlink, rename and symlink-over (`EBUSY`/`EEXIST`),
///   while `auth.json` and Codex's state databases beside it stay writable.
/// * each of `roots` read-write at its own host path (see
///   [`workspace_mount_roots`]).
/// * each of `credentials` read-only at its own host path: the daemon's
///   GitHub App installation-token dirs ([`gh_credential_dirs`]).
/// * the network, through Docker's default bridge (Codex's API, GitHub).
///
/// `--restart unless-stopped` brings the container back after a Docker
/// engine restart or a host reboot (issue #10452). Before it, the policy was
/// `no`: a Docker Desktop restart on robb-studio (2026-10-05, again 10-06) or a
/// stop of every container on loom-worker-2 left each session down until an
/// operator ran `accounts session start` by hand, and every Codex-first role
/// tick routed to it exited 78 meanwhile. It never resurrects a session an
/// operator retired: [`SessionLifecycle::stop`] is `docker stop` + `docker rm`,
/// so a stopped session is gone, not merely stopped.
///
/// Deliberately absent: the Claude token pool (`~/.loom/tokens`) and the
/// operator's personal `~/.config/gh`, both mounted here before #9979. Codex
/// needs neither, and with Codex's sandbox off any file in the container is
/// readable by the model. No Docker socket, no host network or PID
/// namespace, no added capability and no seccomp/AppArmor override is ever
/// passed.
#[must_use]
pub fn host_session_run_args(
    container: &str,
    image: &str,
    codex_home: &Path,
    workspace: &Path,
    roots: &[PathBuf],
    credentials: &[PathBuf],
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
        format!("{}:{CONTAINER_CODEX_HOME}", codex_home.display()),
    ];
    for name in PROFILE_CONTROLS {
        args.push("--mount".into());
        args.push(format!(
            "type=bind,src={},dst={},readonly",
            codex_home.join(name).display(),
            profile_control_destination(name)
        ));
    }
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
    args.extend([
        "--label".into(),
        format!("{WORKSPACE_LABEL}={}", workspace.display()),
        "--label".into(),
        format!("{SESSION_POSTURE_LABEL}={SESSION_POSTURE}"),
        image.into(),
    ]);
    args
}

/// Grace period `stop` gives `docker stop` (SIGTERM) before it would
/// escalate to SIGKILL — the same shape as `docker stop`'s own `-t` timeout,
/// never bypassed by going straight to `docker kill`.
const STOP_GRACE: Duration = Duration::from_secs(15);

/// Wall-clock budget for one in-container `codex login status` probe (issue
/// #6927), bounded for the same reason the host-direct probe's
/// `STATUS_TIMEOUT` is: a hung auth probe must never wedge account selection.
/// Larger than the host-direct budget only by the `docker exec` round trip.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Cap on probe output read into memory, mirroring the host-direct probe's
/// `MAX_STATUS_BYTES` — `codex login status` prints one line, and no amount
/// of output changes the classification.
const MAX_PROBE_BYTES: usize = 4096;

/// Provenance recorded in `.loom/account-health.json` for a health change
/// driven by the in-container probe, distinguishing it from the reactive
/// `adapter_v1` terminal signals that share the same file.
pub const SESSION_PROBE_PROVENANCE: &str = "session_probe:codex_login_status";

/// Default minimum age of the last conclusive probe before an account is
/// re-probed by [`refresh_session_health`]. Selection happens once per
/// dispatch; a `docker exec` per account per dispatch would be pure overhead
/// for a state that changes on the scale of hours. Override with
/// `LOOM_CODEX_SESSION_PROBE_TTL_SECS` (`0` = probe on every selection).
pub const DEFAULT_SESSION_PROBE_TTL_SECS: u64 = 300;

/// Sentinel file marking a profile directory as session-managed
/// (ownership-rule adoption, ADR-0017 Decision 1). Lives directly inside the
/// profile/`CODEX_HOME` directory, alongside `auth.json` — the same
/// convention [`super::account_lifecycle`]'s `recovery.json` already uses for
/// per-profile metadata. Never removed by `stop` (adoption is permanent,
/// independent of whether the container currently happens to be running).
pub const SESSION_MARKER_FILE: &str = ".session-managed.json";

#[derive(Debug, Clone, Serialize)]
struct SessionMarker {
    schema_version: u32,
    container_name: String,
    adopted_at_unix: u64,
}

/// `true` iff `profile` has been adopted by a prior `session start` — the
/// ownership-rule check [`super::account_lifecycle`] consults before any
/// host-direct `CODEX_HOME` use.
#[must_use]
pub fn is_session_managed(profile: &Path) -> bool {
    profile.join(SESSION_MARKER_FILE).is_file()
}

pub(crate) fn mark_session_managed(profile: &Path, container_name: &str) -> Result<()> {
    let marker_path = profile.join(SESSION_MARKER_FILE);
    if marker_path.is_file() {
        // Idempotent: `start` reusing an already-adopted profile must not
        // clobber the original adoption timestamp.
        return Ok(());
    }
    let marker = SessionMarker {
        schema_version: 1,
        container_name: container_name.to_string(),
        adopted_at_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };
    let bytes = serde_json::to_vec_pretty(&marker)?;
    let temp = profile.join(format!(".session-managed.json.tmp-{}", std::process::id()));
    std::fs::write(&temp, bytes).context("failed to stage session-managed marker")?;
    std::fs::rename(&temp, &marker_path).context("failed to commit session-managed marker")?;
    Ok(())
}

/// Secret-free reported state of a named container — never includes any
/// path contents, only identity/lifecycle facts a `docker inspect` already
/// surfaces non-secretly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerState {
    pub id: String,
    pub running: bool,
    pub started_at: Option<String>,
    pub image: Option<String>,
    /// The parity-mounted workspace host path this container was created
    /// with (Issue #7389), read back from the `loom.workspace` container
    /// label. `None` for a container created before this label existed.
    pub workspace: Option<PathBuf>,
}

/// Secret-free result of one **non-interactive** `docker exec` (issue
/// #6927). `output` is the exec'd command's own stdout (falling back to
/// stderr when stdout is empty), truncated to [`MAX_PROBE_BYTES`] — never a
/// credential and never a path's contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    pub success: bool,
    /// The container runtime itself could not be used at all (e.g. no
    /// `docker` binary on PATH) — distinct from the exec'd command failing.
    pub unavailable: bool,
    pub timed_out: bool,
    pub exit_code: Option<i32>,
    pub output: String,
}

/// Docker interaction seam, analogous to
/// [`super::account_lifecycle::CodexCommandRunner`] — a trait so tests drive
/// a fake instead of a real `docker` daemon (issue #6925 acceptance
/// criterion: "test double or local docker, not just a happy-path manual
/// run").
pub trait ContainerRunner {
    /// `None` when no container by this name exists at all (neither running
    /// nor stopped-but-not-removed).
    fn inspect(&self, container: &str) -> Result<Option<ContainerState>>;

    /// Create and start a fresh detached container named `container` from
    /// `image`, bind-mounting `codex_home` read-write at the session image's
    /// fixed `CODEX_HOME` path (`docker/session/README.md`) — the mount
    /// contract's §2 (secrets mounts: `CODEX_HOME` rw, per-account, never
    /// baked) applied concretely — and `workspace` (narrowed to its
    /// registered repositories by [`workspace_mount_roots`], issue #9979)
    /// read-write at the identical absolute host path (`docker/worker/MOUNT-CONTRACT.md` §1,
    /// "path parity"), recorded in the [`WORKSPACE_LABEL`] container label so
    /// a later [`Self::inspect`] can report it back.
    ///
    /// `daemon_root` is the accounts registry's workspace (the `--workspace`
    /// daemon root): one of the owners whose App-token dirs are mounted
    /// ([`gh_credential_owners`], issue #10103).
    fn create(
        &self,
        container: &str,
        image: &str,
        codex_home: &Path,
        workspace: &Path,
        daemon_root: &Path,
    ) -> Result<()>;

    /// `docker start` for an existing, stopped-but-not-removed container.
    fn start_existing(&self, container: &str) -> Result<()>;

    /// Whether a `docker exec`-spawned process is currently active inside
    /// the container, beyond its own baseline (`tini`, the blocking `sleep
    /// infinity`, and the idle tmux server/pane) — see this module's
    /// top-level doc for why `stop` consults this before tearing down.
    fn has_active_exec(&self, container: &str) -> Result<bool>;

    /// Graceful teardown: `docker stop` (SIGTERM, bounded `grace` wait —
    /// never a raw SIGKILL) followed by `docker rm`. A no-op (not an error)
    /// if the container is already gone.
    fn stop_and_remove(&self, container: &str, grace: Duration) -> Result<()>;

    /// Interactive `docker exec -it <container> tmux attach -t <session>`,
    /// inheriting the caller's stdio and returning its exit code.
    /// Operator-only — never called from any dispatch path (ADR-0017
    /// Decision 2).
    fn attach_interactive(&self, container: &str, tmux_session_name: &str) -> Result<i32>;

    /// Non-interactive `docker exec <container> <argv…>` with stdin closed
    /// and output captured, bounded by `timeout` (issue #6927). No TTY, no
    /// tmux, no inherited stdio — the counterpart of
    /// [`Self::attach_interactive`] for anything a machine, not an operator,
    /// needs to run inside the container. This is the same invocation shape
    /// headless dispatch uses (`docker exec <container> codex exec …`,
    /// `docker/session/README.md` § "Two ways to interact"), so the two do
    /// not diverge.
    fn exec_capture(&self, container: &str, argv: &[&str], timeout: Duration)
        -> Result<ExecOutput>;

    /// `true` iff a tmux window named `window` already exists inside
    /// `tmux_session` — the check `shell` uses to decide whether to create a
    /// fresh Codex window or simply re-attach to the one from a prior
    /// `shell` call (never stacking a second `codex` process).
    fn window_exists(&self, container: &str, tmux_session: &str, window: &str) -> Result<bool>;

    /// Create a new tmux window named `window` inside `tmux_session`, cwd'd
    /// to `cwd`, running `command` (argv) — `shell`'s "launch Codex" step.
    fn new_window(
        &self,
        container: &str,
        tmux_session: &str,
        window: &str,
        cwd: &Path,
        command: &[&str],
    ) -> Result<()>;

    /// Make `window` the active window of `tmux_session`, so a subsequent
    /// [`Self::attach_interactive`] (which attaches to whichever window is
    /// currently active) lands on it. Works without an attached client —
    /// this is `shell`'s way of choosing *which* window `attach_interactive`
    /// will show.
    fn select_window(&self, container: &str, tmux_session: &str, window: &str) -> Result<()>;
}

/// Real [`ContainerRunner`] shelling out to the `docker` CLI, exactly the way
/// [`super::account_lifecycle::ProcessCodexRunner`] shells out to `codex`.
pub struct ProcessContainerRunner;

impl ProcessContainerRunner {
    fn run_capture(args: &[&str]) -> Result<(bool, String, String)> {
        let output = Command::new("docker")
            .args(args)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("failed to run `docker {}`", args.join(" ")))?;
        Ok((
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }

    /// The baseline process set the session image's entrypoint establishes:
    /// `tini` (PID 1), the entrypoint's terminal `exec sleep infinity`, the
    /// tmux server launch (`tmux new-session -d -s <name>`), and the tmux
    /// pane's own login shell. Anything else observed via `docker top` is an
    /// active `docker exec` (headless dispatch, an operator's `attach`, or a
    /// manual `codex login`).
    fn is_baseline_process(command: &str, tmux_session_name: &str) -> bool {
        let command = command.trim();
        command.starts_with("/usr/bin/tini")
            || command == "tini"
            || command == "sleep infinity"
            || command == format!("tmux new-session -d -s {tmux_session_name}")
            || command == "-bash"
            || command == "bash"
    }
}

impl ProcessContainerRunner {
    /// Docker CLI versions differ on capitalization ("Error: No such object"
    /// on older Docker, "error: no such object" on newer) — compare
    /// case-insensitively rather than chase every wording.
    fn is_missing_container_error(stderr: &str) -> bool {
        let stderr = stderr.to_ascii_lowercase();
        stderr.contains("no such object") || stderr.contains("no such container")
    }
}

impl ContainerRunner for ProcessContainerRunner {
    fn inspect(&self, container: &str) -> Result<Option<ContainerState>> {
        let label_format = format!("{{{{index .Config.Labels \"{WORKSPACE_LABEL}\"}}}}");
        let (success, stdout, stderr) = Self::run_capture(&[
            "inspect",
            "--format",
            &format!(
                "{{{{.Id}}}}\t{{{{.State.Running}}}}\t{{{{.State.StartedAt}}}}\t{{{{.Config.Image}}}}\t{label_format}"
            ),
            container,
        ])?;
        if !success {
            if Self::is_missing_container_error(&stderr) {
                return Ok(None);
            }
            bail!("docker inspect {container} failed: {}", stderr.trim());
        }
        let line = stdout.trim();
        let mut fields = line.splitn(5, '\t');
        let id = fields.next().unwrap_or_default().to_string();
        let running = fields.next() == Some("true");
        let started_at = fields.next().filter(|s| !s.is_empty()).map(str::to_string);
        let image = fields.next().filter(|s| !s.is_empty()).map(str::to_string);
        let workspace = fields.next().filter(|s| !s.is_empty()).map(PathBuf::from);
        if id.is_empty() {
            return Ok(None);
        }
        Ok(Some(ContainerState {
            id,
            running,
            started_at,
            image,
            workspace,
        }))
    }

    fn create(
        &self,
        container: &str,
        image: &str,
        codex_home: &Path,
        workspace: &Path,
        daemon_root: &Path,
    ) -> Result<()> {
        // The daemon's own workspace registry is the allow-list of what the
        // container may see (issue #9979): an unreadable registry is an
        // empty one, so a parent directory with no registered repositories
        // under it fails closed in `workspace_mount_roots`.
        let registered = crate::workspace_registry::WorkspaceRegistry::load_default()
            .map(|registry| registry.roots())
            .unwrap_or_default();
        let roots = workspace_mount_roots(workspace, &registered)?;
        let firewalled = firewalled_repo_paths(workspace)?;
        check_mount_denials(&roots, dirs::home_dir().as_deref(), &firewalled)?;
        // Daemon-owned App-token dirs for `gh` (see `gh_credential_dirs`):
        // the session workspace, the accounts registry's daemon root (#10103),
        // the daemon's own `LOOM_WORKSPACE`, and the owner of a daemon-shaped
        // `GH_CONFIG_DIR` in this process's env.
        let loom_workspace = std::env::var_os("LOOM_WORKSPACE").map(PathBuf::from);
        let owners = gh_credential_owners(workspace, daemon_root, loom_workspace.as_deref());
        let env_gh = std::env::var_os("GH_CONFIG_DIR").map(PathBuf::from);
        let credentials = gh_credential_dirs(&owners, env_gh.as_deref());
        let args =
            host_session_run_args(container, image, codex_home, workspace, &roots, &credentials);
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let (success, _stdout, stderr) = Self::run_capture(&arg_refs)?;
        if !success {
            bail!("docker run {image} failed: {}", stderr.trim());
        }
        Ok(())
    }

    fn start_existing(&self, container: &str) -> Result<()> {
        let (success, _stdout, stderr) = Self::run_capture(&["start", container])?;
        if !success {
            bail!("docker start {container} failed: {}", stderr.trim());
        }
        Ok(())
    }

    fn has_active_exec(&self, container: &str) -> Result<bool> {
        let (success, stdout, stderr) = Self::run_capture(&["top", container, "-o", "pid,args"])?;
        if !success {
            bail!("docker top {container} failed: {}", stderr.trim());
        }
        let mut lines = stdout.lines();
        lines.next(); // header row (PID / COMMAND)
        for line in lines {
            let command = line
                .split_once(char::is_whitespace)
                .map_or("", |(_, rest)| rest);
            if !Self::is_baseline_process(command, DEFAULT_TMUX_SESSION_NAME) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn stop_and_remove(&self, container: &str, grace: Duration) -> Result<()> {
        let timeout = grace.as_secs().to_string();
        let (success, _stdout, stderr) = Self::run_capture(&["stop", "-t", &timeout, container])?;
        if !success && !Self::is_missing_container_error(&stderr) {
            bail!("docker stop {container} failed: {}", stderr.trim());
        }
        let (success, _stdout, stderr) = Self::run_capture(&["rm", container])?;
        if !success && !Self::is_missing_container_error(&stderr) {
            bail!("docker rm {container} failed: {}", stderr.trim());
        }
        Ok(())
    }

    fn attach_interactive(&self, container: &str, tmux_session_name: &str) -> Result<i32> {
        let status = Command::new("docker")
            .args([
                "exec",
                "-it",
                container,
                "tmux",
                "attach",
                "-t",
                tmux_session_name,
            ])
            .status()
            .with_context(|| format!("failed to attach to session container {container}"))?;
        Ok(status.code().unwrap_or(-1))
    }

    fn exec_capture(
        &self,
        container: &str,
        argv: &[&str],
        timeout: Duration,
    ) -> Result<ExecOutput> {
        let mut command = Command::new("docker");
        command
            .arg("exec")
            .arg(container)
            .args(argv)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ExecOutput {
                    success: false,
                    unavailable: true,
                    timed_out: false,
                    exit_code: None,
                    output: "docker CLI is not installed or not on PATH".into(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait()? {
                let mut bytes = Vec::new();
                if let Some(stdout) = child.stdout.take() {
                    stdout
                        .take(MAX_PROBE_BYTES as u64)
                        .read_to_end(&mut bytes)?;
                }
                if bytes.is_empty() {
                    if let Some(stderr) = child.stderr.take() {
                        stderr
                            .take(MAX_PROBE_BYTES as u64)
                            .read_to_end(&mut bytes)?;
                    }
                }
                return Ok(ExecOutput {
                    success: status.success(),
                    unavailable: false,
                    timed_out: false,
                    exit_code: status.code(),
                    output: String::from_utf8_lossy(&bytes).into_owned(),
                });
            }
            if started.elapsed() >= timeout {
                // Kills the local `docker exec` client, not necessarily the
                // process it started inside the container — which is why this
                // seam is only ever used for read-only, self-terminating
                // commands. A `docker top` observer may briefly still see
                // that process, which correctly keeps `stop` in its
                // refuse-without-`--force` state until it exits.
                let _ = child.kill();
                let _ = child.wait();
                return Ok(ExecOutput {
                    success: false,
                    unavailable: false,
                    timed_out: true,
                    exit_code: None,
                    output: String::new(),
                });
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn window_exists(&self, container: &str, tmux_session: &str, window: &str) -> Result<bool> {
        let target = format!("{tmux_session}:");
        let (success, stdout, stderr) = Self::run_capture(&[
            "exec",
            container,
            "tmux",
            "list-windows",
            "-t",
            &target,
            "-F",
            "#{window_name}",
        ])?;
        if !success {
            // A missing session/container reads the same as "no window yet"
            // -- `shell`'s caller has already ensured the container is
            // running before this is called, so this only fires for a
            // container whose tmux server has not created the session yet,
            // which `new_window` targeting `tmux_session` will fix.
            if Self::is_missing_container_error(&stderr) {
                return Ok(false);
            }
            bail!("docker exec {container} tmux list-windows failed: {}", stderr.trim());
        }
        Ok(stdout.lines().any(|line| line.trim() == window))
    }

    fn new_window(
        &self,
        container: &str,
        tmux_session: &str,
        window: &str,
        cwd: &Path,
        command: &[&str],
    ) -> Result<()> {
        let target = format!("{tmux_session}:");
        let cwd_str = cwd.display().to_string();
        let mut args = vec![
            "exec",
            container,
            "tmux",
            "new-window",
            "-t",
            &target,
            "-n",
            window,
            "-c",
            &cwd_str,
        ];
        args.extend_from_slice(command);
        let (success, _stdout, stderr) = Self::run_capture(&args)?;
        if !success {
            bail!("docker exec {container} tmux new-window failed: {}", stderr.trim());
        }
        Ok(())
    }

    fn select_window(&self, container: &str, tmux_session: &str, window: &str) -> Result<()> {
        let target = format!("{tmux_session}:{window}");
        let (success, _stdout, stderr) =
            Self::run_capture(&["exec", container, "tmux", "select-window", "-t", &target])?;
        if !success {
            bail!("docker exec {container} tmux select-window failed: {}", stderr.trim());
        }
        Ok(())
    }
}

/// `true` when a `docker exec` failed because the *command* does not exist
/// inside the container (docker's own 126/127 convention), rather than
/// because the command ran and reported something.
fn is_command_missing(exec: &ExecOutput) -> bool {
    if !matches!(exec.exit_code, Some(126 | 127)) {
        return false;
    }
    let output = exec.output.to_ascii_lowercase();
    output.contains("executable file not found")
        || output.contains("no such file or directory")
        || output.contains("command not found")
}

/// Probe a session container's own Codex auth state: `docker exec
/// <container> codex login status` (issue #6927).
///
/// This is the session-managed replacement for the host-direct
/// `codex login status` probe that ADR-0017 Decision 1 forbids once a profile
/// is adopted — and it is compatible with that rule rather than an exception
/// to it, on three counts:
///
/// 1. It runs **inside** the container that owns the `CODEX_HOME` volume, so
///    the host never opens the profile directly; the single serializing owner
///    stays the single owner.
/// 2. `codex login status` is read-only — it reports the auth state, it never
///    starts a device-code flow or rewrites the refresh chain — so it cannot
///    clobber a refresh the container is performing concurrently.
/// 3. It is bounded ([`PROBE_TIMEOUT`]) and non-interactive (no TTY, stdin
///    closed), so it can never block waiting for an operator the way an
///    interactive `codex login` would.
///
/// `Ok(None)` means no probe was possible at all — the container is not
/// running, or the container runtime is unavailable. That is deliberately not
/// reported as "logged out": an account whose container is merely stopped has
/// not lost its credentials, and must not be excluded from selection on the
/// strength of a probe that never ran.
pub fn probe_container_login_status<R: ContainerRunner + ?Sized>(
    runner: &R,
    container: &str,
) -> Result<Option<RunnerOutput>> {
    match runner.inspect(container)? {
        Some(state) if state.running => {}
        _ => return Ok(None),
    }
    let exec = runner.exec_capture(container, &["codex", "login", "status"], PROBE_TIMEOUT)?;
    if exec.unavailable {
        return Ok(None);
    }
    Ok(Some(RunnerOutput {
        success: exec.success,
        // `codex` missing *inside* the container is a real, actionable
        // finding about that container's image — reported as `CliMissing`,
        // exactly as a missing host `codex` is on the host-direct path.
        unavailable: is_command_missing(&exec),
        timed_out: exec.timed_out,
        exit_code: exec.exit_code,
        summary: classify_login_status(exec.success, &exec.output).into(),
    }))
}

/// Human/JSON-reportable snapshot [`SessionLifecycle::status`] returns.
#[derive(Debug, Clone, Serialize)]
pub struct SessionStatus {
    pub schema_version: u32,
    pub workspace_mode: &'static str,
    pub name: String,
    pub container_name: String,
    pub running: bool,
    pub container_id: Option<String>,
    pub started_at: Option<String>,
    pub image: Option<String>,
    pub codex_home: PathBuf,
    pub mount_path: &'static str,
    pub session_managed: bool,
    /// The parity-mounted workspace this container was started with (Issue
    /// #7389). `None` for a container never started under this feature.
    pub workspace: Option<PathBuf>,
}

/// The container-naming convention this lifecycle owns end to end: every
/// method below resolves a bare account `name` to this same container name,
/// so a caller never needs to know it.
#[must_use]
pub fn container_name(name: &str) -> String {
    format!("loom-codex-session-{name}")
}

/// Resolve `reference` (a short profile name or a registered email — issue
/// #7389) to its [`AccountDescriptor`]. This is the single choke point every
/// method on [`SessionLifecycle`] goes through, so a raw email can never
/// reach [`container_name`] or any `docker` call unresolved: every caller
/// below uses the returned descriptor's `id.name`, never the raw `reference`
/// argument, when building a container name.
fn find_codex_account(workspace: &Path, reference: &str) -> Result<AccountDescriptor> {
    // Deliberately NOT `validate_name(reference)` here (pre-existing bug,
    // fixed in passing because it otherwise blocks this module's own test
    // suite): `reference` may legitimately be a registered email under the
    // #7389 email -> short-name resolution this function exists for, and
    // `validate_name` unconditionally rejects `@` (Docker container names
    // permit no other characters) — so validating the raw reference made
    // every email lookup fail before `account_matches_reference` below ever
    // ran. Safe to skip: nothing here uses `reference` for a filesystem or
    // `docker` call — every caller uses the RESOLVED descriptor's
    // `account.id.name` (itself already validated at registration time,
    // per `validate_name`'s other call sites), never the raw argument.
    account_inventory(workspace, AccountProvider::Codex)?
        .into_iter()
        .find(|account| account_matches_reference(account, reference))
        .ok_or_else(|| {
            anyhow!("Codex account {reference:?} does not exist (checked profile names and registered emails)")
        })
}

pub struct SessionLifecycle<R> {
    workspace: PathBuf,
    runner: R,
    image: String,
}

impl<R: ContainerRunner> SessionLifecycle<R> {
    pub fn new(workspace: impl Into<PathBuf>, runner: R, image: Option<String>) -> Self {
        Self {
            workspace: workspace.into(),
            runner,
            image: image.unwrap_or_else(|| DEFAULT_SESSION_IMAGE.to_string()),
        }
    }

    /// Launch (or reuse, if already running; resume, if stopped-but-present)
    /// the account's session container with no explicit workspace override
    /// (defaults to this [`SessionLifecycle`]'s own resolved `workspace`) —
    /// see [`Self::start_with_workspace`] for the full contract.
    pub fn start(&self, name: &str) -> Result<SessionStatus> {
        self.start_with_workspace(name, None)
    }

    /// Launch (or reuse, if already running; resume, if stopped-but-present)
    /// the account's session container, then adopt the profile under the
    /// ownership rule. `workspace` is bind-mounted read-write at the
    /// identical absolute host path (`docker/worker/MOUNT-CONTRACT.md` §1)
    /// and recorded in a container label; `None` defaults to this
    /// [`SessionLifecycle`]'s own resolved `workspace` (issue #7389's
    /// "default: the repo root the daemon resolves today").
    ///
    /// Starting (or resuming) an existing container against a *different*
    /// workspace than the one it already has mounted fails clearly, naming
    /// the currently-mounted workspace — a bind mount cannot be changed by
    /// `docker start` without recreating the container, and silently
    /// serving the old mount would be worse than refusing.
    pub fn start_with_workspace(
        &self,
        name: &str,
        workspace: Option<&Path>,
    ) -> Result<SessionStatus> {
        let account = find_codex_account(&self.workspace, name)?;
        let name = account.id.name.as_str();
        let profile = account.credential_reference;
        let container = container_name(name);
        let requested_workspace = workspace
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.workspace.clone());
        match self.runner.inspect(&container)? {
            Some(state) if state.running => {
                // Already running: reuse it (idempotent `start`).
                Self::check_workspace_match(name, &state, &requested_workspace)?;
            }
            Some(state) => {
                Self::check_workspace_match(name, &state, &requested_workspace)?;
                self.runner.start_existing(&container)?;
            }
            None => {
                ensure_profile_controls(&profile)?;
                self.runner.create(
                    &container,
                    &self.image,
                    &profile,
                    &requested_workspace,
                    &self.workspace,
                )?;
            }
        }
        mark_session_managed(&profile, &container)?;
        self.status(name)
    }

    fn require_host_mode(state: &ContainerState) -> Result<()> {
        if state.workspace.as_deref() == Some(Path::new(super::private_workspace::REPO)) {
            bail!("private-clone sessions require the private workspace lifecycle and exclusive account lease; unleased host-mode access refused");
        }
        Ok(())
    }

    /// See [`Self::start_with_workspace`]'s doc for why a mismatch is a hard
    /// error rather than a silent no-op or a silent remount.
    fn check_workspace_match(name: &str, state: &ContainerState, requested: &Path) -> Result<()> {
        Self::require_host_mode(state)?;
        if let Some(existing) = &state.workspace {
            if existing != requested {
                bail!(
                    "session {name:?} is already running with workspace {} mounted; stop it \
                     first (`loom-daemon accounts session stop {name}`) before starting it \
                     against a different workspace ({})",
                    existing.display(),
                    requested.display()
                );
            }
        }
        Ok(())
    }

    /// Tear down the container cleanly. Refuses (unless `force`) when an
    /// in-flight `docker exec` is detected, per this module's restart-safety
    /// doc comment. Idempotent: a session that is already stopped/absent is
    /// success, not an error.
    pub fn stop(&self, name: &str, force: bool) -> Result<SessionStatus> {
        let account = find_codex_account(&self.workspace, name)?;
        let name = account.id.name.as_str();
        let container = container_name(name);
        if let Some(state) = self.runner.inspect(&container)? {
            Self::require_host_mode(&state)?;
            if state.running && !force && self.runner.has_active_exec(&container)? {
                bail!(
                    "session {name:?} has an in-flight `docker exec`; refusing to stop without \
                     --force (a hard stop here would SIGKILL active work, violating the #5119 \
                     restart-safety contract). Retry once the exec finishes, or pass --force to \
                     override."
                );
            }
            self.runner.stop_and_remove(&container, STOP_GRACE)?;
        }
        let _ = &account; // profile currently unused beyond existence-check; kept for symmetry/logging hooks
        self.status(name)
    }

    /// Report running/stopped and basic health (container id, uptime, mount
    /// paths).
    pub fn status(&self, name: &str) -> Result<SessionStatus> {
        let account = find_codex_account(&self.workspace, name)?;
        let name = account.id.name.as_str();
        let profile = account.credential_reference;
        let container = container_name(name);
        let state = self.runner.inspect(&container)?;
        Ok(SessionStatus {
            schema_version: 1,
            workspace_mode: "host-mounted",
            name: name.to_string(),
            container_name: container,
            running: state.as_ref().is_some_and(|s| s.running),
            container_id: state.as_ref().map(|s| s.id.clone()),
            started_at: state.as_ref().and_then(|s| s.started_at.clone()),
            image: state.as_ref().and_then(|s| s.image.clone()),
            session_managed: is_session_managed(&profile),
            mount_path: CONTAINER_CODEX_HOME,
            workspace: state.as_ref().and_then(|s| s.workspace.clone()),
            codex_home: profile,
        })
    }

    /// Attach to the container's tmux server for interactive `codex login` /
    /// inspection. Operator-only: this is never the dispatch path (headless
    /// dispatch is a later Phase 2 issue's plain `docker exec`, unrelated to
    /// this method).
    pub fn attach(&self, name: &str) -> Result<i32> {
        let account = find_codex_account(&self.workspace, name)?;
        let name = account.id.name.as_str();
        let container = container_name(name);
        match self.runner.inspect(&container)? {
            Some(state) if state.running => Self::require_host_mode(&state)?,
            _ => bail!(
                "session {name:?} is not running; run `loom-daemon accounts session start \
                 {name}` first"
            ),
        }
        self.runner
            .attach_interactive(&container, DEFAULT_TMUX_SESSION_NAME)
    }

    /// "Start-if-absent, run Codex, attach" composite (issue #7389) —
    /// what `codex-agent <account>` execs into. Starts (or resumes/reuses)
    /// the session container against `workspace` (same default as
    /// [`Self::start_with_workspace`]), then launches `codex` in a tmux
    /// window cwd'd to the mounted workspace and attaches. `codex_args`
    /// defaults to [`DEFAULT_CODEX_SHELL_ARGS`] (`--yolo`) when empty.
    ///
    /// Re-running `shell` while a prior invocation's window is still alive
    /// re-attaches to that *same* window instead of stacking a second
    /// `codex` process — detaching (`Ctrl-b d`) leaves Codex running.
    pub fn shell(
        &self,
        name: &str,
        workspace: Option<&Path>,
        codex_args: &[String],
    ) -> Result<i32> {
        let status = self.start_with_workspace(name, workspace)?;
        let container = container_name(&status.name);
        let mounted_workspace = status.workspace.ok_or_else(|| {
            anyhow!("session {name:?} has no mounted workspace to launch Codex against")
        })?;
        let already_exists =
            self.runner
                .window_exists(&container, DEFAULT_TMUX_SESSION_NAME, CODEX_WINDOW_NAME)?;
        if !already_exists {
            let owned_args: Vec<&str> = if codex_args.is_empty() {
                DEFAULT_CODEX_SHELL_ARGS.to_vec()
            } else {
                codex_args.iter().map(String::as_str).collect()
            };
            let mut command: Vec<&str> = vec!["codex"];
            command.extend(owned_args);
            self.runner.new_window(
                &container,
                DEFAULT_TMUX_SESSION_NAME,
                CODEX_WINDOW_NAME,
                &mounted_workspace,
                &command,
            )?;
        }
        self.runner
            .select_window(&container, DEFAULT_TMUX_SESSION_NAME, CODEX_WINDOW_NAME)?;
        self.runner
            .attach_interactive(&container, DEFAULT_TMUX_SESSION_NAME)
    }

    /// In-container auth-state probe for one account (issue #6927), reported
    /// as the same [`LoginState`] a host-direct probe produces — a
    /// session-managed account that is logged in must read exactly like a
    /// host-direct one; only the transport differs.
    ///
    /// A profile that has *not* been adopted is reported as
    /// [`LoginState::NotChecked`]: the host-direct probe owns that case
    /// (`loom-daemon accounts status <name>`), and this method must not
    /// invent an answer for it.
    pub fn probe_login(&self, name: &str) -> Result<LoginState> {
        let account = find_codex_account(&self.workspace, name)?;
        self.probe_account(&account)
    }

    fn probe_account(&self, account: &AccountDescriptor) -> Result<LoginState> {
        if !is_session_managed(&account.credential_reference) {
            return Ok(LoginState::NotChecked);
        }
        let container = container_name(&account.id.name);
        Ok(match probe_container_login_status(&self.runner, &container)? {
            Some(output) => login_state_from(&output),
            None => LoginState::SessionUnavailable,
        })
    }

    /// Probe every enabled, session-managed Codex account in `inventory` and
    /// feed each conclusive result into the account-health state
    /// [`super::health::select_healthy_at`] already filters on — the
    /// proactive half of issue #6927's acceptance criteria.
    ///
    /// Only *conclusive* results move health: a container that is stopped, a
    /// `docker`/`codex` binary that is missing, a timeout, or unparseable
    /// output all leave the record untouched. "We could not tell" is never
    /// evidence that an account's refresh chain died, and excluding an
    /// account on that basis would take a whole pool offline the first time
    /// the container runtime hiccups.
    pub fn refresh_health_at(
        &self,
        inventory: &[AccountDescriptor],
        now: u64,
    ) -> Result<Vec<SessionHealthOutcome>> {
        let ttl = env_u64("LOOM_CODEX_SESSION_PROBE_TTL_SECS", DEFAULT_SESSION_PROBE_TTL_SECS);
        let mut outcomes = Vec::new();
        for account in inventory.iter().filter(|account| {
            account.id.provider == AccountProvider::Codex
                && account.enabled
                && is_session_managed(&account.credential_reference)
        }) {
            let recent = health::account_health(&self.workspace, &account.id)?
                .and_then(|entry| entry.last_probe)
                .is_some_and(|last| ttl > 0 && now.saturating_sub(last) < ttl);
            if recent {
                outcomes.push(SessionHealthOutcome {
                    name: account.id.name.clone(),
                    login_state: LoginState::NotChecked,
                    effect: None,
                });
                continue;
            }
            let login_state = self.probe_account(account)?;
            let outcome = match login_state {
                LoginState::LoggedIn => Some(ProbeOutcome::LoggedIn),
                LoginState::NotLoggedIn => Some(ProbeOutcome::NotLoggedIn),
                _ => None,
            };
            let effect = match outcome {
                Some(outcome) => Some(health::record_probe_at(
                    &self.workspace,
                    &account.id,
                    outcome,
                    SESSION_PROBE_PROVENANCE,
                    now,
                )?),
                None => None,
            };
            outcomes.push(SessionHealthOutcome {
                name: account.id.name.clone(),
                login_state,
                effect,
            });
        }
        Ok(outcomes)
    }
}

/// What one account's proactive probe found and what it changed. `effect` is
/// `None` when nothing was recorded — either the probe was inconclusive, or
/// it was skipped because a conclusive result is still fresh (in which case
/// `login_state` is [`LoginState::NotChecked`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionHealthOutcome {
    pub name: String,
    pub login_state: LoginState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effect: Option<ProbeEffect>,
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn probe_enabled() -> bool {
    !matches!(
        std::env::var("LOOM_CODEX_SESSION_PROBE")
            .unwrap_or_default()
            .as_str(),
        "0" | "false" | "no"
    )
}

/// Best-effort proactive auth-state refresh over `inventory`, for callers on
/// the account-selection path (issue #6927).
///
/// Deliberately infallible: a probe is an *optimization* over discovering a
/// dead refresh chain by dispatching into it, so a probe that cannot run must
/// never be the reason a dispatch cannot run. It is also a complete no-op —
/// zero `docker` invocations — when no enabled account is session-managed,
/// which is every pool that has not opted into session containers, and when
/// `LOOM_CODEX_SESSION_PROBE` is set to `0`/`false`/`no`.
pub fn refresh_session_health(
    workspace: &Path,
    inventory: &[AccountDescriptor],
    now: u64,
) -> Vec<SessionHealthOutcome> {
    if !probe_enabled() {
        return Vec::new();
    }
    let any_session_managed = inventory.iter().any(|account| {
        account.id.provider == AccountProvider::Codex
            && account.enabled
            && is_session_managed(&account.credential_reference)
    });
    if !any_session_managed {
        return Vec::new();
    }
    SessionLifecycle::new(workspace, ProcessContainerRunner, None)
        .refresh_health_at(inventory, now)
        .unwrap_or_default()
}

/// Advisory-only uid check against the session image's fixed uid
/// (`docker/worker/MOUNT-CONTRACT.md` §3) — surfaced in [`SessionStatus`]
/// callers may want to render, never a hard `start` failure (the invoking
/// host user is not necessarily the fleet-provisioned uid on every install
/// shape, e.g. a developer's laptop).
#[cfg(unix)]
#[must_use]
pub fn uid_matches_image(profile: &Path) -> Option<bool> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(profile)
        .ok()
        .map(|meta| meta.uid() == SESSION_IMAGE_UID)
}

#[cfg(not(unix))]
#[must_use]
pub fn uid_matches_image(_profile: &Path) -> Option<bool> {
    None
}

#[cfg(test)]
#[path = "session_lifecycle_tests.rs"]
mod tests;
