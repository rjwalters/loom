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
    account_inventory, account_matches_reference, validate_name, AccountDescriptor, AccountProvider,
};
use super::health::{self, ProbeEffect, ProbeOutcome};

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
const WORKSPACE_LABEL: &str = "loom.workspace";

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
    /// baked) applied concretely — and `workspace` read-write at the
    /// identical absolute host path (`docker/worker/MOUNT-CONTRACT.md` §1,
    /// "path parity"), recorded in the [`WORKSPACE_LABEL`] container label so
    /// a later [`Self::inspect`] can report it back.
    fn create(
        &self,
        container: &str,
        image: &str,
        codex_home: &Path,
        workspace: &Path,
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
    ) -> Result<()> {
        let codex_mount = format!("{}:{CONTAINER_CODEX_HOME}", codex_home.display());
        let workspace_str = workspace.display().to_string();
        let workspace_mount = format!("{workspace_str}:{workspace_str}");
        let label = format!("{WORKSPACE_LABEL}={workspace_str}");
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            container.into(),
            "-v".into(),
            codex_mount,
            "-v".into(),
            workspace_mount,
            "--label".into(),
            label,
        ];
        // Best-effort bootstrap-seam mounts (`docker/worker/README.md` §
        // "Bootstrap seams"), never fatal when absent — a session container
        // still functions for `codex exec`/`shell` without them; they only
        // matter if something inside the container also wants gh/Claude
        // pool access.
        if let Some(tokens_dir) = super::paths::shared_tokens_dir() {
            if tokens_dir.is_dir() {
                let tokens_str = tokens_dir.display().to_string();
                args.push("-v".into());
                args.push(format!("{tokens_str}:{tokens_str}:ro"));
            }
        }
        if let Some(gh_config) = dirs::home_dir().map(|home| home.join(".config").join("gh")) {
            if gh_config.is_dir() {
                let gh_str = gh_config.display().to_string();
                args.push("-v".into());
                args.push(format!("{gh_str}:{gh_str}:ro"));
            }
        }
        args.push(image.into());
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
    validate_name(reference)?;
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
                self.runner
                    .create(&container, &self.image, &profile, &requested_workspace)?;
            }
        }
        mark_session_managed(&profile, &container)?;
        self.status(name)
    }

    /// See [`Self::start_with_workspace`]'s doc for why a mismatch is a hard
    /// error rather than a silent no-op or a silent remount.
    fn check_workspace_match(name: &str, state: &ContainerState, requested: &Path) -> Result<()> {
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
            Some(state) if state.running => {}
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
mod tests {
    use super::*;
    use serial_test::serial;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeRunner {
        containers: Mutex<HashMap<String, ContainerState>>,
        busy: Mutex<HashMap<String, bool>>,
        creates: Mutex<Vec<(String, String, PathBuf, PathBuf)>>,
        stops: Mutex<Vec<String>>,
        attaches: Mutex<Vec<(String, String)>>,
        execs: Mutex<Vec<(String, Vec<String>)>>,
        exec_results: Mutex<HashMap<String, ExecOutput>>,
        /// `(container, tmux_session)` -> set of window names, simulating
        /// enough of tmux's window table for `shell`'s create-vs-reuse logic
        /// (issue #7389) without needing a real tmux/docker.
        windows: Mutex<HashMap<(String, String), std::collections::HashSet<String>>>,
        new_windows: Mutex<Vec<NewWindowCall>>,
        selected_windows: Mutex<Vec<(String, String, String)>>,
    }

    /// `(container, tmux_session, window, cwd, command)` recorded by
    /// `FakeRunner::new_window`.
    type NewWindowCall = (String, String, String, PathBuf, Vec<String>);

    fn exec_ok(output: &str) -> ExecOutput {
        ExecOutput {
            success: true,
            unavailable: false,
            timed_out: false,
            exit_code: Some(0),
            output: output.into(),
        }
    }

    impl FakeRunner {
        fn set_exec_result(&self, container: &str, result: ExecOutput) {
            self.exec_results
                .lock()
                .unwrap()
                .insert(container.to_string(), result);
        }
    }

    impl FakeRunner {
        fn seed_running(&self, container: &str) {
            self.seed_running_with_workspace(container, None);
        }

        fn seed_running_with_workspace(&self, container: &str, workspace: Option<PathBuf>) {
            self.containers.lock().unwrap().insert(
                container.to_string(),
                ContainerState {
                    id: format!("{container}-id"),
                    running: true,
                    started_at: Some("2026-09-05T00:00:00Z".into()),
                    image: Some("ghcr.io/rjwalters/loom-worker-session:test".into()),
                    workspace,
                },
            );
        }

        fn set_busy(&self, container: &str, busy: bool) {
            self.busy
                .lock()
                .unwrap()
                .insert(container.to_string(), busy);
        }
    }

    impl ContainerRunner for FakeRunner {
        fn inspect(&self, container: &str) -> Result<Option<ContainerState>> {
            Ok(self.containers.lock().unwrap().get(container).cloned())
        }

        fn create(
            &self,
            container: &str,
            image: &str,
            codex_home: &Path,
            workspace: &Path,
        ) -> Result<()> {
            self.creates.lock().unwrap().push((
                container.to_string(),
                image.to_string(),
                codex_home.to_path_buf(),
                workspace.to_path_buf(),
            ));
            self.seed_running_with_workspace(container, Some(workspace.to_path_buf()));
            Ok(())
        }

        fn start_existing(&self, container: &str) -> Result<()> {
            let mut containers = self.containers.lock().unwrap();
            let state = containers
                .get_mut(container)
                .ok_or_else(|| anyhow!("no such container"))?;
            state.running = true;
            Ok(())
        }

        fn has_active_exec(&self, container: &str) -> Result<bool> {
            Ok(*self.busy.lock().unwrap().get(container).unwrap_or(&false))
        }

        fn stop_and_remove(&self, container: &str, _grace: Duration) -> Result<()> {
            self.stops.lock().unwrap().push(container.to_string());
            self.containers.lock().unwrap().remove(container);
            Ok(())
        }

        fn attach_interactive(&self, container: &str, tmux_session_name: &str) -> Result<i32> {
            self.attaches
                .lock()
                .unwrap()
                .push((container.to_string(), tmux_session_name.to_string()));
            Ok(0)
        }

        fn exec_capture(
            &self,
            container: &str,
            argv: &[&str],
            _timeout: Duration,
        ) -> Result<ExecOutput> {
            self.execs
                .lock()
                .unwrap()
                .push((container.to_string(), argv.iter().map(|arg| (*arg).to_string()).collect()));
            Ok(self
                .exec_results
                .lock()
                .unwrap()
                .get(container)
                .cloned()
                .unwrap_or_else(|| exec_ok("Logged in using ChatGPT")))
        }

        fn window_exists(&self, container: &str, tmux_session: &str, window: &str) -> Result<bool> {
            Ok(self
                .windows
                .lock()
                .unwrap()
                .get(&(container.to_string(), tmux_session.to_string()))
                .is_some_and(|windows| windows.contains(window)))
        }

        fn new_window(
            &self,
            container: &str,
            tmux_session: &str,
            window: &str,
            cwd: &Path,
            command: &[&str],
        ) -> Result<()> {
            self.new_windows.lock().unwrap().push((
                container.to_string(),
                tmux_session.to_string(),
                window.to_string(),
                cwd.to_path_buf(),
                command.iter().map(|arg| (*arg).to_string()).collect(),
            ));
            self.windows
                .lock()
                .unwrap()
                .entry((container.to_string(), tmux_session.to_string()))
                .or_default()
                .insert(window.to_string());
            Ok(())
        }

        fn select_window(&self, container: &str, tmux_session: &str, window: &str) -> Result<()> {
            self.selected_windows.lock().unwrap().push((
                container.to_string(),
                tmux_session.to_string(),
                window.to_string(),
            ));
            Ok(())
        }
    }

    fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
    }

    fn import_account(workspace: &Path, root: &Path, name: &str) {
        import_account_with_email(workspace, root, name, None);
    }

    fn import_account_with_email(workspace: &Path, root: &Path, name: &str, email: Option<&str>) {
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root);
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("auth.json");
        std::fs::write(&source, "recognizable-fake-secret").unwrap();
        let service = super::super::account_lifecycle::AccountLifecycle::new(
            workspace,
            super::super::account_lifecycle::ProcessCodexRunner,
        )
        .unwrap();
        service.import_with_email(name, &source, email).unwrap();
    }

    #[test]
    #[serial]
    fn start_creates_a_fresh_container_and_adopts_the_profile() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        let status = lifecycle.start("alice").unwrap();
        assert!(status.running);
        assert!(status.session_managed);
        assert_eq!(status.container_name, container_name("alice"));
        assert_eq!(status.mount_path, CONTAINER_CODEX_HOME);
        let creates = lifecycle.runner.creates.lock().unwrap();
        assert_eq!(creates.len(), 1);
        assert_eq!(creates[0].0, container_name("alice"));
        assert_eq!(creates[0].1, DEFAULT_SESSION_IMAGE);
        assert!(is_session_managed(&root.path().join("alice")));
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn start_is_idempotent_when_already_running() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        lifecycle.start("alice").unwrap();
        lifecycle.start("alice").unwrap();
        assert_eq!(lifecycle.runner.creates.lock().unwrap().len(), 1);
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn start_resumes_a_stopped_but_present_container() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        lifecycle.start("alice").unwrap();
        lifecycle.stop("alice", false).unwrap();
        // stop_and_remove in the fake fully removes the entry (mirrors a
        // real `docker rm`), so a subsequent `start` must go through
        // `create` again, not `start_existing` — both paths converge on the
        // same observable "running" outcome either way.
        let status = lifecycle.start("alice").unwrap();
        assert!(status.running);
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn stop_refuses_when_an_exec_is_in_flight_unless_forced() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        lifecycle.start("alice").unwrap();
        let container = container_name("alice");
        lifecycle.runner.set_busy(&container, true);

        let error = lifecycle.stop("alice", false).unwrap_err().to_string();
        assert!(error.contains("in-flight"));
        assert!(error.contains("--force"));
        assert!(lifecycle.status("alice").unwrap().running);

        let status = lifecycle.stop("alice", true).unwrap();
        assert!(!status.running);
        assert_eq!(lifecycle.runner.stops.lock().unwrap().len(), 1);
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn stop_is_idempotent_when_already_stopped() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        let status = lifecycle.stop("alice", false).unwrap();
        assert!(!status.running);
        assert!(lifecycle.runner.stops.lock().unwrap().is_empty());
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn status_reports_not_running_for_an_account_never_started() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        let status = lifecycle.status("alice").unwrap();
        assert!(!status.running);
        assert!(status.container_id.is_none());
        assert!(!status.session_managed);
        assert!(status.workspace.is_none());
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn status_reports_no_workspace_for_a_container_created_before_the_label_existed() {
        // A container `seed_running` without a workspace models a real
        // pre-#7389 session container -- `inspect` finds no `loom.workspace`
        // label, and `status` must report `None`, not synthesize one.
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        lifecycle.runner.seed_running(&container_name("alice"));
        let status = lifecycle.status("alice").unwrap();
        assert!(status.running);
        assert!(status.workspace.is_none());
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn status_and_start_reject_an_unknown_account() {
        let (workspace, root) = setup();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        assert!(lifecycle.status("ghost").is_err());
        assert!(lifecycle.start("ghost").is_err());
        assert!(lifecycle.stop("ghost", false).is_err());
        assert!(lifecycle.attach("ghost").is_err());
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn attach_refuses_when_not_running() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        let error = lifecycle.attach("alice").unwrap_err().to_string();
        assert!(error.contains("not running"));
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn attach_execs_tmux_against_the_running_container() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        lifecycle.start("alice").unwrap();
        let code = lifecycle.attach("alice").unwrap();
        assert_eq!(code, 0);
        let attaches = lifecycle.runner.attaches.lock().unwrap();
        assert_eq!(attaches.len(), 1);
        assert_eq!(attaches[0].0, container_name("alice"));
        assert_eq!(attaches[0].1, DEFAULT_TMUX_SESSION_NAME);
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    // ---- workspace mount (#7389) -------------------------------------------

    #[test]
    #[serial]
    fn start_mounts_the_requested_workspace_and_status_reports_it() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let repo = tempfile::tempdir().unwrap();
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        let status = lifecycle
            .start_with_workspace("alice", Some(repo.path()))
            .unwrap();
        assert_eq!(status.workspace.as_deref(), Some(repo.path()));
        let creates = lifecycle.runner.creates.lock().unwrap();
        assert_eq!(creates.len(), 1);
        assert_eq!(creates[0].3, repo.path());

        // `status` on its own (no workspace argument -- it never takes one)
        // reports the same mounted workspace back.
        drop(creates);
        let status = lifecycle.status("alice").unwrap();
        assert_eq!(status.workspace.as_deref(), Some(repo.path()));
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn start_without_a_workspace_defaults_to_the_lifecycles_own_workspace() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        let status = lifecycle.start("alice").unwrap();
        assert_eq!(status.workspace.as_deref(), Some(workspace.path()));
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn starting_a_running_session_against_a_different_workspace_fails_naming_the_current_one() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let first_repo = tempfile::tempdir().unwrap();
        let second_repo = tempfile::tempdir().unwrap();
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        lifecycle
            .start_with_workspace("alice", Some(first_repo.path()))
            .unwrap();

        let error = lifecycle
            .start_with_workspace("alice", Some(second_repo.path()))
            .unwrap_err()
            .to_string();
        assert!(error.contains(&first_repo.path().display().to_string()), "{error}");
        assert!(error.contains(&second_repo.path().display().to_string()), "{error}");
        // This is the workspace-mismatch message, not the exec-in-flight one.
        assert!(!error.contains("in-flight"));
        // Only the original `create` happened -- the mismatch is rejected
        // before any second `docker run`/`docker start`.
        assert_eq!(lifecycle.runner.creates.lock().unwrap().len(), 1);
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn restarting_a_stopped_session_against_a_different_workspace_also_fails() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let first_repo = tempfile::tempdir().unwrap();
        let second_repo = tempfile::tempdir().unwrap();
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        lifecycle
            .start_with_workspace("alice", Some(first_repo.path()))
            .unwrap();
        // Stop the container's own `running` flag without removing it, so
        // the next `start` takes the `start_existing` branch, not `create`.
        lifecycle
            .runner
            .containers
            .lock()
            .unwrap()
            .get_mut(&container_name("alice"))
            .unwrap()
            .running = false;

        let error = lifecycle
            .start_with_workspace("alice", Some(second_repo.path()))
            .unwrap_err()
            .to_string();
        assert!(error.contains(&first_repo.path().display().to_string()), "{error}");
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn restarting_the_same_workspace_succeeds() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let repo = tempfile::tempdir().unwrap();
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        lifecycle
            .start_with_workspace("alice", Some(repo.path()))
            .unwrap();
        // Re-running `start` against the identical workspace is the
        // ordinary idempotent path, not a mismatch.
        let status = lifecycle
            .start_with_workspace("alice", Some(repo.path()))
            .unwrap();
        assert!(status.running);
        assert_eq!(status.workspace.as_deref(), Some(repo.path()));
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    // ---- email -> short-name resolution (#7389) ----------------------------

    #[test]
    #[serial]
    fn session_lifecycle_resolves_an_account_by_registered_email() {
        let (workspace, root) = setup();
        import_account_with_email(
            workspace.path(),
            root.path(),
            "agent-1",
            Some("agent-1@2amlogic.com"),
        );
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        let status = lifecycle.start("agent-1@2amlogic.com").unwrap();
        // The resolved identity -- and therefore the container name -- is
        // always the short profile name, never the raw email.
        assert_eq!(status.name, "agent-1");
        assert_eq!(status.container_name, container_name("agent-1"));
        assert!(!status.container_name.contains('@'));
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn an_unregistered_email_is_rejected_with_a_clear_error() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        let error = lifecycle
            .start("nobody@2amlogic.com")
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not exist"), "{error}");
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    // ---- shell: start-if-absent, run Codex, attach (#7389) -----------------

    #[test]
    #[serial]
    fn shell_starts_an_absent_session_and_launches_codex_in_a_new_window() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let repo = tempfile::tempdir().unwrap();
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);

        let code = lifecycle.shell("alice", Some(repo.path()), &[]).unwrap();
        assert_eq!(code, 0);

        // The container was started (not pre-existing).
        assert_eq!(lifecycle.runner.creates.lock().unwrap().len(), 1);

        let new_windows = lifecycle.runner.new_windows.lock().unwrap();
        assert_eq!(new_windows.len(), 1);
        assert_eq!(new_windows[0].0, container_name("alice"));
        assert_eq!(new_windows[0].1, DEFAULT_TMUX_SESSION_NAME);
        assert_eq!(new_windows[0].2, CODEX_WINDOW_NAME);
        assert_eq!(new_windows[0].3, repo.path());
        // Default args: `codex --yolo` (the operator's own bare-metal
        // invocation), since no explicit codex_args were passed.
        assert_eq!(new_windows[0].4, vec!["codex".to_string(), "--yolo".to_string()]);
        drop(new_windows);

        let selected = lifecycle.runner.selected_windows.lock().unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].2, CODEX_WINDOW_NAME);
        drop(selected);

        let attaches = lifecycle.runner.attaches.lock().unwrap();
        assert_eq!(attaches.len(), 1);
        assert_eq!(attaches[0].0, container_name("alice"));
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn shell_passes_through_explicit_codex_args_instead_of_the_default() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let repo = tempfile::tempdir().unwrap();
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);

        let args = vec!["--dangerously-bypass-approvals-and-sandbox".to_string()];
        lifecycle.shell("alice", Some(repo.path()), &args).unwrap();

        let new_windows = lifecycle.runner.new_windows.lock().unwrap();
        assert_eq!(
            new_windows[0].4,
            vec![
                "codex".to_string(),
                "--dangerously-bypass-approvals-and-sandbox".to_string()
            ]
        );
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn shell_reattaches_to_an_existing_codex_window_without_stacking_a_second_process() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let repo = tempfile::tempdir().unwrap();
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);

        lifecycle.shell("alice", Some(repo.path()), &[]).unwrap();
        assert_eq!(lifecycle.runner.new_windows.lock().unwrap().len(), 1);

        // Detach (simulated by simply returning from the first `shell` call)
        // and run `shell` again against the same account/workspace.
        let code = lifecycle.shell("alice", Some(repo.path()), &[]).unwrap();
        assert_eq!(code, 0);

        // No second `docker run` (container reused) and no second
        // `new-window` call -- only a re-select + re-attach.
        assert_eq!(lifecycle.runner.creates.lock().unwrap().len(), 1);
        assert_eq!(lifecycle.runner.new_windows.lock().unwrap().len(), 1);
        assert_eq!(lifecycle.runner.selected_windows.lock().unwrap().len(), 2);
        assert_eq!(lifecycle.runner.attaches.lock().unwrap().len(), 2);
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn shell_resolves_the_account_by_email_before_any_container_call() {
        let (workspace, root) = setup();
        import_account_with_email(
            workspace.path(),
            root.path(),
            "agent-1",
            Some("agent-1@2amlogic.com"),
        );
        let repo = tempfile::tempdir().unwrap();
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);

        lifecycle
            .shell("agent-1@2amlogic.com", Some(repo.path()), &[])
            .unwrap();

        let creates = lifecycle.runner.creates.lock().unwrap();
        assert_eq!(creates.len(), 1);
        assert_eq!(creates[0].0, container_name("agent-1"));
        assert!(!creates[0].0.contains('@'));
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn adopting_two_accounts_keeps_container_names_and_markers_distinct() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        import_account(workspace.path(), root.path(), "bob");
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        lifecycle.start("alice").unwrap();
        assert!(is_session_managed(&root.path().join("alice")));
        assert!(!is_session_managed(&root.path().join("bob")));
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    fn container_name_is_namespaced_per_account() {
        assert_eq!(container_name("alice"), "loom-codex-session-alice");
        assert_ne!(container_name("alice"), container_name("bob"));
    }

    // ---- pure baseline-process classification (issue #6925 acceptance
    // criterion: unit tests, not just a manual docker run) -----------------

    #[test]
    fn baseline_processes_are_recognized() {
        for command in [
            "/usr/bin/tini -- /home/loom/.local/bin/loom-session-entrypoint.sh",
            "sleep infinity",
            "tmux new-session -d -s session",
            "-bash",
        ] {
            assert!(
                ProcessContainerRunner::is_baseline_process(command, "session"),
                "{command:?} should be classified as baseline"
            );
        }
    }

    #[test]
    fn non_baseline_processes_are_flagged_busy() {
        for command in [
            "codex exec do the thing",
            "tmux attach -t session",
            "sh -c echo hi",
        ] {
            assert!(
                !ProcessContainerRunner::is_baseline_process(command, "session"),
                "{command:?} should NOT be classified as baseline"
            );
        }
    }

    // ---- in-container auth probe (issue #6927) ----------------------------

    fn inventory(workspace: &Path) -> Vec<AccountDescriptor> {
        account_inventory(workspace, AccountProvider::Codex).unwrap()
    }

    fn health_of(workspace: &Path, name: &str) -> Option<super::super::health::AccountHealth> {
        health::account_health(
            workspace,
            &super::super::account_registry::AccountId {
                provider: AccountProvider::Codex,
                name: name.into(),
            },
        )
        .unwrap()
    }

    #[test]
    #[serial]
    fn probe_execs_codex_login_status_inside_the_container() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        lifecycle.start("alice").unwrap();

        assert_eq!(lifecycle.probe_login("alice").unwrap(), LoginState::LoggedIn);
        let execs = lifecycle.runner.execs.lock().unwrap();
        assert_eq!(execs.len(), 1);
        assert_eq!(execs[0].0, container_name("alice"));
        assert_eq!(execs[0].1, vec!["codex", "login", "status"]);
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn probe_reports_not_logged_in_from_the_container_output() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        lifecycle.start("alice").unwrap();
        lifecycle
            .runner
            .set_exec_result(&container_name("alice"), exec_ok("Not logged in"));
        assert_eq!(lifecycle.probe_login("alice").unwrap(), LoginState::NotLoggedIn);
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn probe_reports_session_unavailable_without_a_running_container() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        // Adopted (the ownership rule applies) but never started.
        mark_session_managed(&root.path().join("alice"), &container_name("alice")).unwrap();
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        assert_eq!(lifecycle.probe_login("alice").unwrap(), LoginState::SessionUnavailable);
        assert!(lifecycle.runner.execs.lock().unwrap().is_empty());
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn probe_leaves_a_host_direct_profile_to_the_host_direct_path() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        assert_eq!(lifecycle.probe_login("alice").unwrap(), LoginState::NotChecked);
        assert!(lifecycle.runner.execs.lock().unwrap().is_empty());
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn an_expired_probe_excludes_the_account_from_selection_before_any_dispatch() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        import_account(workspace.path(), root.path(), "bob");
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        lifecycle.start("alice").unwrap();
        lifecycle
            .runner
            .set_exec_result(&container_name("alice"), exec_ok("Not logged in"));

        let accounts = inventory(workspace.path());
        let outcomes = lifecycle.refresh_health_at(&accounts, 1_000).unwrap();
        // Only the adopted account is probed at all.
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].name, "alice");
        assert_eq!(outcomes[0].login_state, LoginState::NotLoggedIn);
        assert_eq!(outcomes[0].effect, Some(ProbeEffect::MarkedReauthRequired));

        let health = health_of(workspace.path(), "alice").unwrap();
        assert_eq!(health.reason, super::super::health::HealthReason::ReauthRequired);
        assert_eq!(health.signal_provenance, SESSION_PROBE_PROVENANCE);
        assert_eq!(health.last_probe, Some(1_000));
        assert!(health.cooldown_until.is_none(), "a reauth hold is sticky, never a cooldown");

        // The exclusion is the existing `select_healthy_at` filter, not new
        // ranking logic — "alice" is simply no longer a candidate.
        let selected =
            health::select_healthy_at(workspace.path(), AccountProvider::Codex, &accounts, 1_001)
                .unwrap();
        assert_eq!(selected.id.name, "bob");
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn a_healthy_probe_clears_an_existing_reauth_hold() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        lifecycle.start("alice").unwrap();
        let accounts = inventory(workspace.path());

        health::record_terminal_at(
            workspace.path(),
            &accounts[0].id,
            super::super::health::TerminalClassification::TokenExpired,
            "adapter_v1",
            1,
        )
        .unwrap();
        assert!(
            health::select_healthy_at(workspace.path(), AccountProvider::Codex, &accounts, 2)
                .is_err()
        );

        let outcomes = lifecycle.refresh_health_at(&accounts, 3).unwrap();
        assert_eq!(outcomes[0].login_state, LoginState::LoggedIn);
        assert_eq!(outcomes[0].effect, Some(ProbeEffect::ClearedReauthHold));
        assert!(
            health::select_healthy_at(workspace.path(), AccountProvider::Codex, &accounts, 4)
                .is_ok()
        );
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn an_inconclusive_probe_never_marks_an_account_expired() {
        let inconclusive = [
            // Container stopped: nothing was probed.
            None,
            // `codex` missing inside the container.
            Some(ExecOutput {
                success: false,
                unavailable: false,
                timed_out: false,
                exit_code: Some(127),
                output: "exec: \"codex\": executable file not found in $PATH".into(),
            }),
            // Probe timed out.
            Some(ExecOutput {
                success: false,
                unavailable: false,
                timed_out: true,
                exit_code: None,
                output: String::new(),
            }),
            // Ran, but said nothing either way.
            Some(ExecOutput {
                success: false,
                unavailable: false,
                timed_out: false,
                exit_code: Some(1),
                output: "unexpected".into(),
            }),
        ];
        for result in inconclusive {
            let (workspace, root) = setup();
            import_account(workspace.path(), root.path(), "alice");
            let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
            match result {
                Some(result) => {
                    lifecycle.start("alice").unwrap();
                    lifecycle
                        .runner
                        .set_exec_result(&container_name("alice"), result);
                }
                None => {
                    mark_session_managed(&root.path().join("alice"), &container_name("alice"))
                        .unwrap();
                }
            }
            let accounts = inventory(workspace.path());
            let outcomes = lifecycle.refresh_health_at(&accounts, 10).unwrap();
            assert_eq!(outcomes[0].effect, None);
            assert!(
                health_of(workspace.path(), "alice").is_none(),
                "an inconclusive probe must not write health state at all"
            );
            assert!(health::select_healthy_at(
                workspace.path(),
                AccountProvider::Codex,
                &accounts,
                11
            )
            .is_ok());
            std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
        }
    }

    #[test]
    #[serial]
    fn a_fresh_conclusive_result_suppresses_re_probing_until_the_ttl_lapses() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        lifecycle.start("alice").unwrap();
        let accounts = inventory(workspace.path());

        lifecycle.refresh_health_at(&accounts, 1_000).unwrap();
        assert_eq!(lifecycle.runner.execs.lock().unwrap().len(), 1);

        // Within the TTL: no second `docker exec`.
        let skipped = lifecycle
            .refresh_health_at(&accounts, 1_000 + DEFAULT_SESSION_PROBE_TTL_SECS - 1)
            .unwrap();
        assert_eq!(skipped[0].login_state, LoginState::NotChecked);
        assert_eq!(skipped[0].effect, None);
        assert_eq!(lifecycle.runner.execs.lock().unwrap().len(), 1);

        // Once it lapses, the account is probed again.
        lifecycle
            .refresh_health_at(&accounts, 1_000 + DEFAULT_SESSION_PROBE_TTL_SECS)
            .unwrap();
        assert_eq!(lifecycle.runner.execs.lock().unwrap().len(), 2);
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    #[serial]
    fn refresh_is_a_no_op_when_no_account_is_session_managed() {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        // Uses the REAL `ProcessContainerRunner`: the point of this test is
        // that the selection path costs zero `docker` invocations for a pool
        // that never opted into session containers — it must pass on a host
        // with no docker installed at all.
        let accounts = inventory(workspace.path());
        assert!(refresh_session_health(workspace.path(), &accounts, 1).is_empty());
        assert!(health_of(workspace.path(), "alice").is_none());
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }

    #[test]
    fn a_missing_in_container_command_is_recognized_but_a_plain_failure_is_not() {
        assert!(is_command_missing(&ExecOutput {
            success: false,
            unavailable: false,
            timed_out: false,
            exit_code: Some(127),
            output: "exec: \"codex\": executable file not found in $PATH".into(),
        }));
        assert!(!is_command_missing(&ExecOutput {
            success: false,
            unavailable: false,
            timed_out: false,
            exit_code: Some(1),
            output: "Not logged in".into(),
        }));
        // Exit 127 alone is not enough — a command may legitimately exit 127.
        assert!(!is_command_missing(&ExecOutput {
            success: false,
            unavailable: false,
            timed_out: false,
            exit_code: Some(127),
            output: "some unrelated failure".into(),
        }));
    }

    // ---- marker file mechanics --------------------------------------------

    #[test]
    fn mark_session_managed_is_idempotent_and_preserves_first_adoption() {
        let profile = tempfile::tempdir().unwrap();
        assert!(!is_session_managed(profile.path()));
        mark_session_managed(profile.path(), "loom-codex-session-alice").unwrap();
        assert!(is_session_managed(profile.path()));
        let first = std::fs::read_to_string(profile.path().join(SESSION_MARKER_FILE)).unwrap();
        // A second `start` against an already-adopted profile (e.g. a
        // different container name after manual recovery) must not
        // overwrite the original adoption record.
        mark_session_managed(profile.path(), "some-other-name").unwrap();
        let second = std::fs::read_to_string(profile.path().join(SESSION_MARKER_FILE)).unwrap();
        assert_eq!(first, second);
    }

    #[cfg(unix)]
    #[test]
    fn uid_matches_image_reports_mismatch_for_a_tempdir_owned_by_the_test_process() {
        let profile = tempfile::tempdir().unwrap();
        // The result is host-dependent (the test process's own uid), but the
        // function must never panic and must return a definite answer for an
        // existing directory.
        assert!(uid_matches_image(profile.path()).is_some());
    }
}
