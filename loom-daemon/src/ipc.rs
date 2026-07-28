use crate::activity::{ActivityDb, AgentInput, AgentOutput, InputContext, InputType};
use crate::errors::DaemonError;
use crate::event_bus::EventBus;
use crate::forge_parser::parse_forge_events;
use crate::git_parser;
use crate::git_utils;
use crate::main_health_gate::WorkspaceHealthStates;
use crate::sweep_registry::{BeginCancel, SweepRegistry};
use crate::terminal::TerminalManager;
use crate::types::{CredentialPreflightReport, DaemonStatusReport, Event, Request, Response};
use crate::workspace_pool::WorkspacePool;
use crate::workspace_registry::WorkspaceRegistry;
use anyhow::Result;
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Bound on the singleton-guard liveness probe (#3806). Both the connect and
/// the `Ping`/`Pong` roundtrip are individually capped at this duration so a
/// hung or unresponsive peer can never stall daemon startup.
const LIVENESS_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

// ============================================================================
// Shutdown-intent exit codes (Issue #4054 — supervised restart primitive)
// ============================================================================
// Under the macOS launchd `KeepAlive: { SuccessfulExit: true }` contract (see
// `render_launchd_plist` in `loom-daemon-start.sh`), launchd relaunches the job
// ONLY when it exits with status 0 ("successful"), and leaves it down on any
// non-zero exit. The daemon therefore encodes *why* it is shutting down in its
// exit code so exactly one path — the restart primitive — trips a relaunch:
//
//   | Cause                         | Exit code       | launchd action    |
//   |-------------------------------|-----------------|-------------------|
//   | RestartDaemon (this primitive)| 0  EXIT_RESTART | relaunch (wanted) |
//   | SIGTERM (operator stop)       | 143 EXIT_SIGTERM| NO relaunch       |
//   | SIGINT  (interactive Ctrl-C)  | 130 EXIT_SIGINT | NO relaunch       |
//   | IPC Shutdown request          | 143 EXIT_SHUTDOWN| NO relaunch      |
//   | crash / panic                 | non-zero        | NO relaunch       |
//
// This is Curator Finding 1's remedy: because a SIGTERM'd daemon now exits
// non-zero, launchd never relaunches it during an operator stop, so "an operator
// stop stays stopped" holds WITHOUT depending on `bootout` timing (the bootout
// in `loom-daemon-stop.sh` is demoted to belt-and-braces). It also preserves the
// pre-existing no-crash-loop semantics: a crashed daemon exits non-zero and is
// not respawned, exactly as under the old `KeepAlive: false`.

/// Exit code for the supervised restart primitive: the ONLY exit that trips a
/// launchd `SuccessfulExit` relaunch.
pub const EXIT_RESTART: i32 = 0;
/// Exit code for a SIGTERM-driven operator stop (128 + SIGTERM 15). Non-zero so
/// launchd does not relaunch.
pub const EXIT_SIGTERM: i32 = 143;
/// Exit code for an interactive SIGINT / Ctrl-C (128 + SIGINT 2). Non-zero so
/// launchd does not relaunch.
pub const EXIT_SIGINT: i32 = 130;
/// Exit code for an explicit IPC `Shutdown` request. Non-zero — an explicit
/// shutdown means "stay down", so launchd must not relaunch.
pub const EXIT_SHUTDOWN: i32 = 143;

/// Detect the daemon's process supervisor from the environment (#4054).
///
/// Returns `Some("launchd")` when `LOOM_DAEMON_SUPERVISOR=launchd`
/// (case-insensitive) is present — a value `loom-daemon-start.sh` bakes into the
/// launchd plist's `EnvironmentVariables`, so it survives a relaunch. Any other
/// or absent value ⇒ `None` (the daemon is unsupervised: nohup / Linux /
/// `--foreground`), and the restart primitive must refuse to end the process
/// because nothing would bring it back.
pub fn detect_supervisor() -> Option<String> {
    match std::env::var("LOOM_DAEMON_SUPERVISOR") {
        Ok(v) if v.eq_ignore_ascii_case("launchd") => Some("launchd".to_string()),
        _ => None,
    }
}

/// Decide how to answer a `RestartDaemon` request (Issue #4054): the `Response`
/// to send back, plus whether the daemon should then end its own process (exit
/// [`EXIT_RESTART`]) for a supervised relaunch.
///
/// The daemon ends itself ONLY when [`detect_supervisor`] proves it is
/// supervised. On an unsupervised host it refuses, stays running, and returns a
/// `DaemonRestart { scheduled: false, .. }` — degrading to "log loudly, leave
/// the daemon running, do not restart" per #4017, because exiting with no
/// supervisor to relaunch it would be strictly worse than the status quo.
pub fn build_restart_decision() -> (Response, bool) {
    match detect_supervisor() {
        Some(sup) => (
            Response::DaemonRestart {
                scheduled: true,
                supervisor: Some(sup.clone()),
                message: format!(
                    "restart scheduled: exiting 0 for a {sup}-supervised relaunch. \
                     In-flight sweeps survive by design; the relaunched daemon re-reads \
                     the same launchd plist, so it comes back with exactly its start flags."
                ),
            },
            true,
        ),
        None => (
            Response::DaemonRestart {
                scheduled: false,
                supervisor: None,
                message: "refusing to restart: no supervisor detected \
                    (LOOM_DAEMON_SUPERVISOR unset). This daemon was not started under \
                    launchd (nohup / Linux / --foreground), so nothing would relaunch it \
                    if it exited. Leaving it running. Restart it manually with \
                    loom-daemon-stop.sh && loom-daemon-start.sh."
                    .to_string(),
            },
            false,
        ),
    }
}

// ============================================================================
// Scheduled drain-and-restart (Issue #4090)
// ============================================================================

/// Default bound on how long a drain waits for the sweep registry to empty
/// before it either refuses (fail-safe) or force-cancels the stragglers. A
/// sweep is ~10–20 min, so the default is generous.
pub const DEFAULT_DRAIN_TIMEOUT_SECS: u64 = 1800;

/// How often the drain supervisor re-counts the cross-root in-flight sweeps.
/// Small enough that a drain that finishes exits promptly; the zero-in-flight
/// case is handled on the very first poll (no full-interval wait).
pub const DRAIN_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Shared drain-and-restart coordination state (Issue #4090).
///
/// Owns the daemon-global drain flag OR'd into the producers' halt checks (work
/// finder, epic supervisor, role runner) and the descriptor the `DaemonStatus`
/// snapshot renders. Exactly one drain-supervisor task may be live at a time; a
/// monotonic `generation` lets a running supervisor detect it has been
/// superseded (a fresh drain) or aborted, and stop **without** exiting the
/// process.
#[derive(Debug)]
pub struct DrainState {
    /// The flag consulted by the dispatch producers. `true` ⇒ new dispatch is
    /// paused pending a supervised restart. Cloned out to each producer via
    /// [`Self::flag`].
    flag: Arc<AtomicBool>,
    /// Bumped on every accepted drain start AND on abort/timeout-resume, so a
    /// running drain-supervisor task can tell it is still the current one.
    generation: AtomicU64,
    /// Mutable descriptor of the active/last drain, for status rendering.
    inner: Mutex<DrainDescriptor>,
}

/// The rendered view of the current (or most recent) drain (Issue #4090).
#[derive(Debug, Default, Clone)]
pub struct DrainDescriptor {
    /// Whether a drain is currently in progress.
    pub active: bool,
    /// Deadline after which the drain gives up waiting.
    pub deadline: Option<chrono::DateTime<Utc>>,
    /// Whether the deadline path force-cancels stragglers (vs. refusing).
    pub force_after_timeout: bool,
    /// A short human-readable note about the last transition (timeout refusal,
    /// abort) surfaced in `loom-daemon status`.
    pub note: Option<String>,
}

/// Outcome of [`DrainState::begin`].
#[derive(Debug)]
pub enum DrainBegin {
    /// A new drain was started; the caller must spawn the supervisor task with
    /// this generation.
    Started {
        generation: u64,
        deadline: chrono::DateTime<Utc>,
    },
    /// A drain was already in progress; the request is an idempotent ack and no
    /// second supervisor should be spawned (the deadline/generation are
    /// unchanged).
    AlreadyDraining,
}

impl Default for DrainState {
    fn default() -> Self {
        Self::new()
    }
}

// Allow expect_used: a poisoned drain mutex means another thread panicked while
// holding it — unrecoverable, same crash-on-poison policy as the rest of ipc.rs.
#[allow(clippy::expect_used)]
impl DrainState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            generation: AtomicU64::new(0),
            inner: Mutex::new(DrainDescriptor::default()),
        }
    }

    /// A clone of the drain flag to hand to a dispatch producer.
    #[must_use]
    pub fn flag(&self) -> Arc<AtomicBool> {
        self.flag.clone()
    }

    /// Whether new dispatch is currently paused for a drain.
    #[must_use]
    pub fn is_draining(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    /// The current generation — the token a supervisor compares against to
    /// detect it has been superseded/aborted.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// A snapshot of the descriptor for status rendering.
    #[must_use]
    pub fn snapshot(&self) -> DrainDescriptor {
        self.inner.lock().expect("Drain mutex poisoned").clone()
    }

    /// Start a drain, or ack an already-running one (idempotent — a second drain
    /// request while DRAINING neither stacks a supervisor nor moves the
    /// deadline). Sets the drain flag on a fresh start.
    pub fn begin(&self, timeout: Duration, force_after_timeout: bool) -> DrainBegin {
        let mut inner = self.inner.lock().expect("Drain mutex poisoned");
        if inner.active {
            return DrainBegin::AlreadyDraining;
        }
        let deadline = Utc::now()
            + chrono::Duration::from_std(timeout).unwrap_or_else(|_| chrono::Duration::seconds(0));
        inner.active = true;
        inner.deadline = Some(deadline);
        inner.force_after_timeout = force_after_timeout;
        inner.note = None;
        // Set the flag while holding the descriptor lock so status can never
        // observe `flag=true` with `active=false`.
        self.flag.store(true, Ordering::Relaxed);
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        DrainBegin::Started {
            generation,
            deadline,
        }
    }

    /// Abort an in-progress drain: clear the flag, bump the generation (so the
    /// running supervisor stops without exiting), and record a note. Returns
    /// `true` when a drain was actually in progress.
    pub fn abort(&self) -> bool {
        let mut inner = self.inner.lock().expect("Drain mutex poisoned");
        if !inner.active {
            return false;
        }
        self.flag.store(false, Ordering::Relaxed);
        self.generation.fetch_add(1, Ordering::Relaxed);
        inner.active = false;
        inner.deadline = None;
        inner.note = Some("drain aborted by operator — dispatch resumed".to_string());
        true
    }

    /// The supervisor's fail-safe timeout path: clear the flag, bump the
    /// generation, and record the refusal note so status explains why the
    /// daemon stayed up.
    fn resolve_timeout(&self, note: String) {
        let mut inner = self.inner.lock().expect("Drain mutex poisoned");
        self.flag.store(false, Ordering::Relaxed);
        self.generation.fetch_add(1, Ordering::Relaxed);
        inner.active = false;
        inner.deadline = None;
        inner.note = Some(note);
    }
}

/// The three terminal/continue decisions a drain-supervisor poll can reach
/// (Issue #4090). Extracted as a pure function so the "2 → 1 → 0" and
/// timeout-vs-force logic is unit-testable without spawning a task or calling
/// `std::process::exit`.
#[derive(Debug, PartialEq, Eq)]
pub enum DrainTick {
    /// Sweeps still in flight and the deadline has not passed — keep waiting.
    Continue,
    /// Zero in-flight — restart now (exit `EXIT_RESTART`).
    Complete,
    /// Deadline passed with sweeps still in flight and no force — refuse the
    /// restart, resume dispatch, stay up.
    TimedOutRefuse,
    /// Deadline passed with sweeps still in flight and `--force-after-timeout` —
    /// cancel the stragglers, then restart.
    TimedOutForce,
}

/// Decide a single drain-supervisor poll (Issue #4090). Zero in-flight always
/// wins (even at/after the deadline: everything drained, so restart), otherwise
/// a passed deadline is refused (fail-safe) or forced.
#[must_use]
pub fn evaluate_drain_tick(in_flight: usize, past_deadline: bool, force: bool) -> DrainTick {
    if in_flight == 0 {
        DrainTick::Complete
    } else if past_deadline {
        if force {
            DrainTick::TimedOutForce
        } else {
            DrainTick::TimedOutRefuse
        }
    } else {
        DrainTick::Continue
    }
}

/// Count non-terminal (`Pending` / `Running`) sweeps across **every** managed
/// root (Issue #4090, Finding 5). Mirrors [`build_daemon_status`]'s cross-root
/// accounting so a drain never reads only the primary registry and restarts
/// while a secondary managed repo still has live sweeps.
// Allow expect_used: poisoned registry mutex ⇒ crash, same policy as elsewhere.
#[allow(clippy::expect_used)]
#[must_use]
pub fn count_in_flight_sweeps(workspace_pool: &Arc<WorkspacePool>, fallback_root: &Path) -> usize {
    let workspace_registry = WorkspaceRegistry::load_default().unwrap_or_default();
    let roots = workspace_registry.effective_roots(fallback_root);
    let mut count = 0;
    for root in &roots {
        let registry = workspace_pool.get_or_provision(root);
        let sr = registry.lock().expect("Sweep registry mutex poisoned");
        count += sr
            .list(None)
            .into_iter()
            .filter(|info| !info.state.is_terminal())
            .count();
    }
    count
}

/// Cancel every in-flight sweep across all managed roots via the existing
/// [`SweepRegistry::cancel`] path (Issue #4090, `--force-after-timeout`).
/// Returns the number cancelled. Blocking cancel is acceptable here: this runs
/// only on the rare force-timeout path, moments before the process exits.
// Allow expect_used: poisoned registry mutex ⇒ crash, same policy as elsewhere.
#[allow(clippy::expect_used)]
fn cancel_all_in_flight(workspace_pool: &Arc<WorkspacePool>, fallback_root: &Path) -> usize {
    let workspace_registry = WorkspaceRegistry::load_default().unwrap_or_default();
    let roots = workspace_registry.effective_roots(fallback_root);
    let mut cancelled = 0;
    for root in &roots {
        let registry = workspace_pool.get_or_provision(root);
        let ids: Vec<String> = {
            let sr = registry.lock().expect("Sweep registry mutex poisoned");
            sr.list(None)
                .into_iter()
                .filter(|info| !info.state.is_terminal())
                .map(|info| info.sweep_id)
                .collect()
        };
        for id in ids {
            let mut sr = registry.lock().expect("Sweep registry mutex poisoned");
            if sr.cancel(&id, Duration::from_secs(5)).is_ok() {
                cancelled += 1;
            }
        }
    }
    cancelled
}

/// Handle a `DrainAndRestartDaemon` request (Issue #4090): check supervision up
/// front (AC5 — refuse *before* pausing dispatch), then start the drain and
/// spawn its supervisor task. Returns the `DaemonDrain` response to send back.
///
/// Must be called from within a tokio runtime context (the connection handler
/// is) so it can spawn the supervisor.
fn handle_drain_request(
    drain: &Arc<DrainState>,
    workspace_pool: &Arc<WorkspacePool>,
    fallback_root: &Path,
    event_bus: &Arc<EventBus>,
    timeout_secs: Option<u64>,
    force_after_timeout: bool,
) -> Response {
    // AC5 / Finding 4: prove supervision BEFORE entering DRAINING. Draining for
    // 20 minutes and only then discovering there is no supervisor to relaunch
    // into is the worst possible ordering — and pausing dispatch then refusing
    // would be a silent outage.
    let supervisor = match detect_supervisor() {
        Some(s) => s,
        None => {
            return Response::DaemonDrain {
                accepted: false,
                supervisor: None,
                in_flight: count_in_flight_sweeps(workspace_pool, fallback_root),
                message: "refusing to drain: no supervisor detected \
                    (LOOM_DAEMON_SUPERVISOR unset). This daemon was not started under \
                    launchd, so nothing would relaunch it after a drain. Dispatch was \
                    NOT paused. Restart manually with loom-daemon-stop.sh && \
                    loom-daemon-start.sh."
                    .to_string(),
            };
        }
    };

    let in_flight = count_in_flight_sweeps(workspace_pool, fallback_root);
    let timeout = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_DRAIN_TIMEOUT_SECS));

    match drain.begin(timeout, force_after_timeout) {
        DrainBegin::Started {
            generation,
            deadline,
        } => {
            let _ = event_bus.publish_generic(
                "daemon.drain.started",
                serde_json::json!({
                    "in_flight": in_flight,
                    "timeout_secs": timeout.as_secs(),
                    "force_after_timeout": force_after_timeout,
                    "deadline": deadline,
                }),
            );
            let drain_task = drain.clone();
            let pool_task = workspace_pool.clone();
            let root_task = fallback_root.to_path_buf();
            let bus_task = event_bus.clone();
            tokio::spawn(async move {
                run_drain_supervisor(
                    drain_task,
                    pool_task,
                    root_task,
                    bus_task,
                    generation,
                    DRAIN_POLL_INTERVAL,
                )
                .await;
            });
            let msg = if in_flight == 0 {
                format!("drain scheduled ({supervisor}-supervised): 0 in-flight — restarting now.")
            } else {
                format!(
                    "drain scheduled ({supervisor}-supervised): {in_flight} in-flight sweep(s); \
                     new dispatch paused. Will restart when drained, or {} at the deadline.",
                    if force_after_timeout {
                        "cancel stragglers and restart"
                    } else {
                        "refuse and resume dispatch"
                    }
                )
            };
            Response::DaemonDrain {
                accepted: true,
                supervisor: Some(supervisor),
                in_flight,
                message: msg,
            }
        }
        DrainBegin::AlreadyDraining => Response::DaemonDrain {
            accepted: true,
            supervisor: Some(supervisor),
            in_flight,
            message: format!(
                "already draining (idempotent): {in_flight} in-flight sweep(s); the existing \
                 deadline is unchanged. Use `loom-daemon restart --abort-drain` to cancel."
            ),
        },
    }
}

/// The drain-supervisor loop (Issue #4090). Polls the cross-root in-flight count
/// and owns the eventual `std::process::exit(EXIT_RESTART)`; on a fail-safe
/// timeout it clears the drain flag and stays up. Stops without exiting if it
/// has been superseded (a new drain) or aborted (generation moved on).
async fn run_drain_supervisor(
    drain: Arc<DrainState>,
    workspace_pool: Arc<WorkspacePool>,
    fallback_root: PathBuf,
    event_bus: Arc<EventBus>,
    my_generation: u64,
    poll_interval: Duration,
) {
    loop {
        // Superseded / aborted: a newer drain or an abort bumped the generation,
        // so this supervisor is stale — stop WITHOUT ending the process. This is
        // the "abort then the queue empties anyway" guard (AC6): a stale
        // supervisor must never fire a restart.
        if drain.generation() != my_generation {
            log::info!(
                "drain supervisor (gen {my_generation}) superseded/aborted (current gen {}) — \
                 stopping without restart",
                drain.generation()
            );
            return;
        }

        let in_flight = count_in_flight_sweeps(&workspace_pool, &fallback_root);
        let (past_deadline, force) = {
            let snap = drain.snapshot();
            let past = snap.deadline.is_some_and(|d| Utc::now() >= d);
            (past, snap.force_after_timeout)
        };

        match evaluate_drain_tick(in_flight, past_deadline, force) {
            DrainTick::Continue => {
                tokio::time::sleep(poll_interval).await;
            }
            DrainTick::Complete => {
                let _ = event_bus.publish_generic(
                    "daemon.drain.completed",
                    serde_json::json!({ "in_flight": 0 }),
                );
                log::warn!(
                    "drain complete — 0 in-flight sweeps; exiting {EXIT_RESTART} for a launchd \
                     KeepAlive:SuccessfulExit relaunch. No sweep was killed; no orphan left behind."
                );
                std::process::exit(EXIT_RESTART);
            }
            DrainTick::TimedOutRefuse => {
                let note = format!(
                    "drain timed out with {in_flight} sweep(s) still in flight — refused restart \
                     (no --force-after-timeout); dispatch resumed, daemon stays up"
                );
                let _ = event_bus.publish_generic(
                    "daemon.drain.timeout",
                    serde_json::json!({ "in_flight": in_flight, "forced": false }),
                );
                log::warn!("{note}");
                drain.resolve_timeout(note);
                return;
            }
            DrainTick::TimedOutForce => {
                let cancelled = cancel_all_in_flight(&workspace_pool, &fallback_root);
                let _ = event_bus.publish_generic(
                    "daemon.drain.timeout",
                    serde_json::json!({
                        "in_flight": in_flight,
                        "forced": true,
                        "cancelled": cancelled,
                    }),
                );
                log::warn!(
                    "drain timed out with {in_flight} in-flight; --force-after-timeout cancelled \
                     {cancelled} sweep(s); exiting {EXIT_RESTART} for a supervised relaunch"
                );
                std::process::exit(EXIT_RESTART);
            }
        }
    }
}

/// Returns `true` if a live `loom-daemon` is currently listening on
/// `socket_path` and actively servicing requests.
///
/// The probe connects to the socket and performs a `Ping`/`Pong` roundtrip:
///
/// - A connect failure (`ECONNREFUSED`, `ENOENT`, `ENOTSOCK`, permission
///   error, …) means the socket is absent or stale — the file may linger from
///   a crashed daemon but nothing is listening — so it is safe to remove and
///   rebind. Returns `false`.
/// - A successful connect **and** a `Pong` reply confirms a live daemon owns
///   the socket. Returns `true`; the caller must refuse to start rather than
///   unlink the path out from under the incumbent.
///
/// A connect that succeeds but never yields a `Pong` within
/// `LIVENESS_PROBE_TIMEOUT` (e.g. an accept loop wedged before it services
/// requests, or a non-daemon process squatting the path) is treated as "not a
/// live, responsive daemon" and returns `false` — refusing to ever reclaim
/// such a socket would be worse than rebinding it.
async fn socket_has_live_listener(socket_path: &Path) -> bool {
    let stream = match tokio::time::timeout(
        LIVENESS_PROBE_TIMEOUT,
        UnixStream::connect(socket_path),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        // Connect refused/absent, or the connect itself timed out — not a
        // live listener.
        _ => return false,
    };

    let probe = async move {
        let (reader, mut writer) = stream.into_split();
        // Reuse the canonical Ping request shape so the probe stays in sync
        // with the wire protocol.
        let request_json = serde_json::to_string(&Request::Ping).ok()?;
        writer.write_all(request_json.as_bytes()).await.ok()?;
        writer.write_all(b"\n").await.ok()?;
        writer.flush().await.ok()?;

        let mut lines = BufReader::new(reader).lines();
        let line = lines.next_line().await.ok()??;
        let response: Response = serde_json::from_str(&line).ok()?;
        Some(matches!(response, Response::Pong))
    };

    matches!(tokio::time::timeout(LIVENESS_PROBE_TIMEOUT, probe).await, Ok(Some(true)))
}

/// Get the current git branch for a given directory
/// Returns None if not in a git repository or if the command fails
fn get_git_branch(working_dir: Option<&String>) -> Option<String> {
    let dir = working_dir?;

    let output = Command::new("git")
        .arg("rev-parse")
        .arg("--abbrev-ref")
        .arg("HEAD")
        .current_dir(dir)
        .output()
        .ok()?;

    if output.status.success() {
        String::from_utf8(output.stdout)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        None
    }
}

pub struct IpcServer {
    socket_path: PathBuf,
    terminal_manager: Arc<Mutex<TerminalManager>>,
    activity_db: Arc<Mutex<ActivityDb>>,
    sweep_registry: Arc<Mutex<SweepRegistry>>,
    event_bus: Arc<EventBus>,
    /// Per-workspace reactive main-health halt state (#3930, was a single
    /// `MainHealthState` in #3812). Threaded into the IPC server so the
    /// `DaemonStatus` request can report each registered repo's own halt state —
    /// the same `Arc` the multi-workspace work-finder and gate loop share.
    health_states: Arc<WorkspaceHealthStates>,
    /// The per-workspace sweep-registry pool (#3929). The default workspace's
    /// registry is seeded into it, and the autonomous loops provision the other
    /// managed repos' registries on demand. Threaded into the IPC handler so a
    /// request carrying an explicit `workspace_root` can observe/address a sweep
    /// in a managed repo other than the default workspace, and so
    /// `DeregisterWorkspace` can evict the in-memory entry.
    workspace_pool: Arc<WorkspacePool>,
    /// The daemon's primary workspace root (`sweep_workspace`), used as the
    /// `effective_roots` fallback when the machine-level workspace registry is
    /// empty (#3930). In the common single-workspace case this is the only root
    /// the `DaemonStatus` per-repo breakdown enumerates.
    fallback_root: PathBuf,
    /// Startup forge-credential preflight snapshot (#4005), resolved once at
    /// daemon boot (`main.rs`, before the claim-reconciliation startup pass)
    /// and threaded in read-only so `DaemonStatus` can report it without a
    /// re-probe on every status query.
    credential_preflight: Arc<CredentialPreflightReport>,
    /// Shared drain-and-restart coordination state (#4090). The same `Arc` whose
    /// flag is OR'd into the dispatch producers' halt checks; the IPC handler
    /// sets/aborts it and the `DaemonStatus` snapshot renders it.
    drain_state: Arc<DrainState>,
}

impl IpcServer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        socket_path: PathBuf,
        terminal_manager: Arc<Mutex<TerminalManager>>,
        activity_db: Arc<Mutex<ActivityDb>>,
        sweep_registry: Arc<Mutex<SweepRegistry>>,
        event_bus: Arc<EventBus>,
        health_states: Arc<WorkspaceHealthStates>,
        workspace_pool: Arc<WorkspacePool>,
        fallback_root: PathBuf,
        credential_preflight: CredentialPreflightReport,
        drain_state: Arc<DrainState>,
    ) -> Self {
        Self {
            socket_path,
            terminal_manager,
            activity_db,
            sweep_registry,
            event_bus,
            health_states,
            workspace_pool,
            fallback_root,
            credential_preflight: Arc::new(credential_preflight),
            drain_state,
        }
    }

    pub async fn run(&self) -> Result<()> {
        // Singleton guard (#3806): before touching the socket, probe whether a
        // live daemon is already listening on it. Starting a second daemon used
        // to unconditionally `remove_file` + rebind, silently orphaning the
        // incumbent (still running, still holding its children, but with its
        // socket unlinked). Refuse to start in that case; only a genuinely
        // stale/absent socket is removed and rebound below.
        if socket_has_live_listener(&self.socket_path).await {
            anyhow::bail!(
                "another loom-daemon is already listening on {} — refusing to start. \
                 If you intended to replace it, stop the running daemon first \
                 (e.g. `kill <pid>` or its shutdown path) and retry.",
                self.socket_path.display()
            );
        }

        // Remove old socket (best-effort; only reached when no live listener
        // answered the probe above, i.e. the file is stale or absent).
        let _ = fs::remove_file(&self.socket_path).await;

        let listener = UnixListener::bind(&self.socket_path)?;
        log::info!("IPC server listening at {}", self.socket_path.display());

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let tm = self.terminal_manager.clone();
                    let db = self.activity_db.clone();
                    let sr = self.sweep_registry.clone();
                    let bus = self.event_bus.clone();
                    let health = self.health_states.clone();
                    let pool = self.workspace_pool.clone();
                    let fallback = self.fallback_root.clone();
                    let credential_preflight = self.credential_preflight.clone();
                    let drain = self.drain_state.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_client(
                            stream,
                            tm,
                            db,
                            sr,
                            bus,
                            health,
                            pool,
                            fallback,
                            credential_preflight,
                            drain,
                        )
                        .await
                        {
                            log::error!("Client error: {e}");
                        }
                    });
                }
                Err(e) => {
                    log::error!("Accept error: {e}");
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_client(
    stream: UnixStream,
    terminal_manager: Arc<Mutex<TerminalManager>>,
    activity_db: Arc<Mutex<ActivityDb>>,
    sweep_registry: Arc<Mutex<SweepRegistry>>,
    event_bus: Arc<EventBus>,
    health_states: Arc<WorkspaceHealthStates>,
    workspace_pool: Arc<WorkspacePool>,
    fallback_root: PathBuf,
    credential_preflight: Arc<CredentialPreflightReport>,
    drain_state: Arc<DrainState>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        // Parse the incoming frame. A malformed payload (garbage JSON, a
        // missing required field, or an unknown `type` tag) is a per-request
        // protocol error, NOT a fatal connection error: emit a structured
        // error frame naming the serde failure and keep the connection usable
        // for subsequent requests rather than silently dropping the socket.
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(parse_err) => {
                let response =
                    Response::StructuredError(DaemonError::ipc_parse_error(&line, &parse_err));
                let response_json = serde_json::to_string(&response)?;
                writer.write_all(response_json.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                continue;
            }
        };
        log::debug!("Request: {request:?}");

        // SubscribeEvents is the only structurally-different request: it
        // returns a stream of `EventStream` frames on the same connection
        // rather than a single response. Once a client subscribes, the
        // connection is dedicated to the stream until the client closes
        // it (or the bus drops).
        if let Request::SubscribeEvents { topics } = request {
            stream_events(&event_bus, &mut writer, topics).await?;
            // After streaming ends (client disconnect or bus closed) the
            // connection has no more useful state — exit the loop.
            break;
        }

        // CancelSweep is handled here rather than in the synchronous
        // `handle_request` dispatcher (Issue #3807): its SIGTERM → grace-poll
        // → SIGKILL escalation must NOT hold the registry mutex across the
        // (possibly multi-second) grace window, or it would freeze every
        // other IPC request (ListSweeps / GetSweepStatus / DispatchSweep).
        // The async handler below re-acquires the lock only for the brief
        // begin / poll / finish steps and `await`s the sleep unlocked.
        if let Request::CancelSweep {
            sweep_id,
            grace_secs,
            workspace_root,
        } = request
        {
            let target =
                resolve_registry(&sweep_registry, &workspace_pool, workspace_root.as_deref());
            let response =
                cancel_sweep_nonblocking(&target, &sweep_id, Duration::from_secs(grace_secs)).await;
            let response_json = serde_json::to_string(&response)?;
            writer.write_all(response_json.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            continue;
        }

        // DaemonStatus (Issue #3891) is handled here rather than in the
        // synchronous `handle_request` dispatcher because it reads the per-repo
        // `health_states` halt flags, which the dispatcher does not receive.
        // The report is cheap to build (per-repo registry snapshots + a few pure
        // filesystem reads for the dynamic-cap inputs); per-token usage is left
        // to the CLI (a slow network probe) so this handler never blocks.
        if let Request::DaemonStatus = request {
            // Pre-warm the memoized CPU idle-fraction sample off the runtime
            // (#4031): the macOS `iostat` read sleeps ~1s, so it must never run
            // inline on a tokio worker. `build_daemon_status` then reads the
            // freshly-cached value without blocking. A memoized-fresh sample
            // (within the TTL) makes this a no-op. `spawn_blocking` join errors
            // are non-fatal — the status falls back to the last cached value.
            let _ = tokio::task::spawn_blocking(crate::cpu_headroom::refresh_cpu_util_cache).await;
            let report = build_daemon_status_with_drain(
                &workspace_pool,
                &health_states,
                &fallback_root,
                &credential_preflight,
                &drain_state,
            );
            let response = Response::DaemonStatus(report);
            let response_json = serde_json::to_string(&response)?;
            writer.write_all(response_json.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            continue;
        }

        // DrainAndRestartDaemon / AbortDrain (Issue #4090). Handled here — like
        // RestartDaemon — because the drain must ack immediately and then exit
        // *minutes* later from a background supervisor task, which the inline
        // per-connection handler cannot do. The supervisor is spawned inside
        // `handle_drain_request`; this handler just acks and moves on.
        if let Request::DrainAndRestartDaemon {
            timeout_secs,
            force_after_timeout,
        } = request
        {
            let response = handle_drain_request(
                &drain_state,
                &workspace_pool,
                &fallback_root,
                &event_bus,
                timeout_secs,
                force_after_timeout,
            );
            let response_json = serde_json::to_string(&response)?;
            writer.write_all(response_json.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
            continue;
        }

        if let Request::AbortDrain = request {
            let was_draining = drain_state.abort();
            let _ = event_bus.publish_generic(
                "daemon.drain.aborted",
                serde_json::json!({ "was_draining": was_draining }),
            );
            let message = if was_draining {
                "drain aborted — dispatch resumed; no restart will fire (even if in-flight \
                 later reaches zero)."
                    .to_string()
            } else {
                "no drain in progress — nothing to abort (no-op).".to_string()
            };
            let response = Response::DaemonDrain {
                accepted: was_draining,
                supervisor: detect_supervisor(),
                in_flight: count_in_flight_sweeps(&workspace_pool, &fallback_root),
                message,
            };
            let response_json = serde_json::to_string(&response)?;
            writer.write_all(response_json.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            continue;
        }

        // RestartDaemon (Issue #4054) is handled here rather than in the
        // synchronous `handle_request` dispatcher so the supervised path can
        // reply to the client and FLUSH before ending the process — the
        // operator / Phase-3 caller gets a clean ack, then the daemon exits 0
        // for a launchd `KeepAlive:SuccessfulExit` relaunch. On an unsupervised
        // host it returns `DaemonRestart { scheduled: false }` and keeps running
        // (do_exit == false). Mirrors the CancelSweep/DaemonStatus interception.
        if let Request::RestartDaemon = request {
            let (response, do_exit) = build_restart_decision();
            let response_json = serde_json::to_string(&response)?;
            writer.write_all(response_json.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
            if do_exit {
                log::warn!(
                    "RestartDaemon: supervised — exiting {EXIT_RESTART} for a launchd \
                     KeepAlive:SuccessfulExit relaunch. In-flight sweeps survive by design; \
                     the relaunched daemon re-reads the same plist (exactly its start flags). \
                     The stale socket is reclaimed by the relaunched daemon's singleton guard."
                );
                std::process::exit(EXIT_RESTART);
            }
            continue;
        }

        let response = handle_request(
            request,
            &terminal_manager,
            &activity_db,
            &sweep_registry,
            &event_bus,
            &workspace_pool,
        );

        let response_json = serde_json::to_string(&response)?;
        writer.write_all(response_json.as_bytes()).await?;
        writer.write_all(b"\n").await?;
    }

    Ok(())
}

/// Stream events from the bus to `writer` as long as the bus is alive
/// and the client connection is open.
///
/// This is the streaming-response path used by `Request::SubscribeEvents`.
/// Each event is encoded as a single `Response::EventStream { events }`
/// frame containing exactly one event (the `events: Vec<Event>` shape
/// gives us room to batch in a future revision without a protocol break).
///
/// Termination: the loop ends when either
///
/// - the bus is dropped (`Subscription::recv` returns `Closed`), or
/// - `writer.write_all` returns an error (the client closed the socket).
async fn stream_events(
    bus: &Arc<EventBus>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    topics: Vec<String>,
) -> Result<()> {
    use crate::event_bus::RecvError;

    let mut subscription = bus.subscribe(topics);

    loop {
        match subscription.recv().await {
            Ok(event) => {
                let frame = Response::EventStream {
                    events: vec![event],
                };
                let frame_json = serde_json::to_string(&frame)?;
                if writer.write_all(frame_json.as_bytes()).await.is_err() {
                    // Client disconnected — gracefully exit.
                    break;
                }
                if writer.write_all(b"\n").await.is_err() {
                    break;
                }
            }
            Err(RecvError::Closed) => {
                log::debug!("event stream: bus closed, ending subscription");
                break;
            }
            Err(RecvError::Empty) => {
                // recv() should never return Empty (it blocks); but if
                // the underlying receiver ever changes semantics, just
                // yield and try again.
                tokio::task::yield_now().await;
            }
        }
    }
    Ok(())
}

/// Cancel a sweep WITHOUT holding the registry mutex across the grace
/// poll/sleep window (Issue #3807).
///
/// The blocking `SweepRegistry::cancel` holds `&mut self` — and therefore the
/// `Mutex<SweepRegistry>` — for the whole SIGTERM → grace-poll → SIGKILL
/// escalation, so a `grace_secs = 30` cancel would freeze every other IPC
/// request (ListSweeps / GetSweepStatus / DispatchSweep) for up to 30s. This
/// async orchestration instead re-acquires the lock only for three brief,
/// non-blocking steps:
///
/// 1. `begin_cancel` — read pid/kind/liveness + SIGTERM the process group.
/// 2. `poll_cancel` — one liveness poll (reaps on exit, #3801), once per tick.
/// 3. `finish_cancel` — SIGKILL decision + reap + terminal transition + events.
///
/// The 100ms sleep between polls runs UNLOCKED via `tokio::time::sleep`, so the
/// registry mutex is free for other clients for the entire grace window. The
/// synchronous `SweepCancelled` response contract (`sigkill_sent`, `was_running`,
/// `pid`) is preserved — the caller still gets a completed-cancel ack.
// Allow expect_used: a poisoned registry mutex means another thread panicked
// while holding the lock — unrecoverable, so we crash (same policy as
// `handle_request`).
#[allow(clippy::expect_used)]
async fn cancel_sweep_nonblocking(
    sweep_registry: &Arc<Mutex<SweepRegistry>>,
    sweep_id: &str,
    grace: Duration,
) -> Response {
    // Step 1: begin (lock-scoped). Read state + SIGTERM, then release.
    let began = {
        let mut sr = sweep_registry
            .lock()
            .expect("Sweep registry mutex poisoned");
        sr.begin_cancel(sweep_id)
    };
    let (pid, kind, started_at) = match began {
        Ok(BeginCancel::AlreadyTerminal(outcome)) => {
            return Response::SweepCancelled {
                sweep_id: outcome.sweep_id,
                pid: outcome.pid,
                sigkill_sent: outcome.sigkill_sent,
                was_running: outcome.was_running,
            };
        }
        Ok(BeginCancel::Signalled {
            pid,
            kind,
            started_at,
        }) => (pid, kind, started_at),
        Err(e) => {
            return Response::Error {
                message: format!("cancel_sweep failed: {e}"),
            };
        }
    };

    // Step 2: poll for exit up to the grace window. Each poll takes the lock
    // only briefly; the sleep between polls is awaited UNLOCKED so concurrent
    // IPC requests are serviced promptly.
    let poll_interval = Duration::from_millis(100);
    let deadline = tokio::time::Instant::now() + grace;
    let mut exited_within_grace = {
        let mut sr = sweep_registry
            .lock()
            .expect("Sweep registry mutex poisoned");
        sr.poll_cancel(sweep_id, pid)
    };
    while !exited_within_grace && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(poll_interval).await;
        exited_within_grace = {
            let mut sr = sweep_registry
                .lock()
                .expect("Sweep registry mutex poisoned");
            sr.poll_cancel(sweep_id, pid)
        };
    }

    // Step 3: finish (lock-scoped). SIGKILL decision + reap + terminal
    // transition + event emission.
    let outcome = {
        let mut sr = sweep_registry
            .lock()
            .expect("Sweep registry mutex poisoned");
        sr.finish_cancel(sweep_id, pid, &kind, started_at, exited_within_grace)
    };
    Response::SweepCancelled {
        sweep_id: outcome.sweep_id,
        pid: outcome.pid,
        sigkill_sent: outcome.sigkill_sent,
        was_running: outcome.was_running,
    }
}

/// Build the autonomous-mode operability snapshot for a `DaemonStatus` request
/// (Issue #3891 — follow-up to #3813 Phase D).
///
/// Combines a live registry snapshot (in-flight = non-terminal sweeps) with the
/// three dynamic-cap inputs recomputed from the workspace (token-pool size, disk
/// headroom, configured ceiling) and the shared main-health-gate halt flag. The
/// `min` of the three inputs is the effective dynamic cap the work finder would
/// use on its next tick.
///
/// Per-token usage is intentionally excluded — probing each account for
/// rate-limit headers is a slow network call the CLI performs client-side (via
/// `loom-tokens check --json`), so this handler stays non-blocking.
// Allow expect_used: a poisoned registry mutex means another thread panicked
// while holding the lock — unrecoverable, so we crash (same policy as
// `handle_request`).
#[allow(clippy::expect_used)]
pub fn build_daemon_status(
    workspace_pool: &Arc<WorkspacePool>,
    health_states: &WorkspaceHealthStates,
    fallback_root: &Path,
    credential_preflight: &CredentialPreflightReport,
) -> DaemonStatusReport {
    // Enumerate every registered managed workspace (Issue #3930). An empty
    // registry yields `[fallback_root]`, so the common single-workspace case is
    // byte-for-byte the pre-#3930 behavior (one root — the daemon's own).
    let workspace_registry = WorkspaceRegistry::load_default().unwrap_or_default();
    let roots = workspace_registry.effective_roots(fallback_root);

    // Per-repo breakdown + the union of in-flight sweeps across every repo. Each
    // root reads its own registry from the pool (the fallback/default root
    // resolves to the seeded default registry, so a single-workspace daemon reads
    // exactly the same registry it did pre-#3930).
    let mut in_flight: Vec<crate::types::SweepInfo> = Vec::new();
    let mut per_repo: Vec<crate::types::RepoStatus> = Vec::with_capacity(roots.len());
    for root in &roots {
        let registry = workspace_pool.get_or_provision(root);
        let (live, quarantined_issues): (Vec<crate::types::SweepInfo>, Vec<u32>) = {
            let sr = registry.lock().expect("Sweep registry mutex poisoned");
            // In-flight = sweeps still live (Pending / Running). Terminal sweeps
            // (Exited / Crashed) linger in the registry but are not "in flight".
            let live = sr
                .list(None)
                .into_iter()
                .filter(|info| !info.state.is_terminal())
                .collect();
            // Insta-crash quarantine (#3939): surface which issues this repo is
            // currently refusing to re-dispatch, so a repo with a visible backlog
            // that is dispatching nothing is explained.
            (live, sr.quarantined_issues_sorted())
        };
        per_repo.push(crate::types::RepoStatus {
            root: root.clone(),
            priority: workspace_registry.priority_of(root),
            in_flight_count: live.len(),
            health_gate_halted: health_states.is_halted(root),
            quarantined_issues,
            health_gate_not_evaluated: health_states.is_unevaluated(root),
            // Name the actual failure class (#3974 AC2) rather than letting the
            // renderer assume "dirty tree" for every unevaluated tick.
            health_gate_not_evaluated_reason: health_states.unevaluated_summary(root),
            // Resolved daemon-side (this process's own env + this root's own
            // `.loom/config.json`), never the CLI client's environment (#4012).
            health_gate_enabled: Some(crate::main_health_gate::effective_enabled(root)),
            health_gate_verdict_at: health_states.last_verdict_at(root),
        });
        in_flight.extend(live);
    }

    // Present the per-repo breakdown in dispatch-priority order (#3946) — the
    // same order the autonomous loops drain — so the highest-priority repos are
    // listed first. Stable within a tier (tiebreak on root path) for determinism.
    per_repo.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then_with(|| a.root.cmp(&b.root))
    });

    // Dynamic-cap inputs are *machine-level* (one token pool, one scratch
    // volume), so they are computed once from the daemon's primary workspace —
    // the same basis as pre-#3930 (which read them from the default registry's
    // `workspace_root`, i.e. `fallback_root`).
    let workspace_root = fallback_root;
    let token_pool_size = crate::tokens::token_pool_size(workspace_root);
    let disk_headroom = crate::disk_headroom::disk_headroom_limit(workspace_root);
    // Hoisted above the CPU snapshot (#4032) so the resolved
    // `cpuUtilizationTarget` / `estCoresPerSweep` knobs can feed it — this read
    // was already happening six lines below; moving it up is not a new config
    // read, just a reorder so status and dispatch resolve through the same
    // env > config > default path (`resolve_cpu_utilization_target` /
    // `resolve_cpu_est_cores_per_sweep`, single-root, matching
    // `resolve_per_token_concurrency`).
    let wf_config = crate::work_finder::read_work_finder_config(workspace_root);
    let configured_max = crate::work_finder::resolve_max_concurrent_with_config(&wf_config);
    let per_token_concurrency = crate::work_finder::resolve_per_token_concurrency(&wf_config);
    let cpu_utilization_target = crate::work_finder::resolve_cpu_utilization_target(&wf_config);
    let cpu_est_cores_per_sweep = crate::work_finder::resolve_cpu_est_cores_per_sweep(&wf_config);
    // CPU headroom (#3978, measured-idle signal #4031) — a host-level resource
    // (not per-repo). The snapshot reads the memoized idle fraction (never
    // blocks; the caller pre-warms it via `spawn_blocking(refresh_cpu_util_cache)`
    // before invoking `build_daemon_status`) plus a fast fresh loadavg read.
    let cpu_snapshot =
        crate::cpu_headroom::cpu_headroom_snapshot(cpu_utilization_target, cpu_est_cores_per_sweep);
    let logical_cpus = cpu_snapshot.logical_cpus;
    let loadavg_1m = cpu_snapshot.loadavg_1m;
    let cpu_idle_fraction = cpu_snapshot.idle_fraction;
    let cpu_headroom = cpu_snapshot.cpu_headroom;

    // Token-capacity backpressure (#3902): back the token axis off from the flat
    // pool count toward the count of *healthy* accounts read from the rotation
    // ranking. When no ranking exists, `token_axis_limit` == the raw pool size,
    // so the dynamic cap is byte-for-byte the pre-#3902 value.
    let ranking = crate::capacity::read_ranking(workspace_root);
    let token_axis_limit = ranking.as_ref().map_or(token_pool_size, |r| r.available);
    let dynamic_cap = crate::work_finder::resolve_dynamic_max_concurrent(
        token_axis_limit,
        per_token_concurrency,
        disk_headroom,
        cpu_headroom,
        configured_max,
    );
    // The token axis of the cap is `healthy × per-token` (#3947), so it is the
    // binding constraint only when that *product* is the minimum across every
    // axis — disk, cpu (#3978), and the operator ceiling.
    let token_axis_effective = token_axis_limit.saturating_mul(per_token_concurrency.max(1));
    let token_bound = token_axis_effective <= disk_headroom
        && token_axis_effective <= cpu_headroom
        && token_axis_effective <= configured_max;
    // "Currently binding" vs "smallest ceiling" (#4031): the dynamic cap is the
    // minimum of several ceilings, but a ceiling only *binds* once in-flight
    // occupancy reaches it. Below the cap the limiter is work availability, not
    // any resource term — so gate the token-bound diagnosis on real occupancy.
    let capacity_bound = in_flight.len() >= dynamic_cap;
    let capacity = crate::types::CapacityReport {
        ranking_present: ranking.is_some(),
        total_accounts: ranking.as_ref().map_or(token_pool_size, |r| r.total),
        healthy_accounts: ranking.as_ref().map_or(token_pool_size, |r| r.available),
        exhausted_accounts: ranking
            .as_ref()
            .map_or(0, crate::capacity::RankingSnapshot::unhealthy),
        token_axis_limit,
        token_bound,
    };

    DaemonStatusReport {
        in_flight,
        token_pool_size,
        disk_headroom,
        cpu_headroom,
        logical_cpus,
        loadavg_1m,
        cpu_idle_fraction,
        capacity_bound,
        configured_max,
        per_token_concurrency,
        dynamic_cap,
        // Top-level halt preserves its pre-#3930 single-workspace meaning: the
        // daemon's own primary workspace. Per-repo halt is in `per_repo`.
        main_health_gate_halted: health_states.is_halted(fallback_root),
        main_health_gate_not_evaluated: health_states.is_unevaluated(fallback_root),
        main_health_gate_not_evaluated_reason: health_states.unevaluated_summary(fallback_root),
        main_health_gate_enabled: Some(crate::main_health_gate::effective_enabled(fallback_root)),
        main_health_gate_verdict_at: health_states.last_verdict_at(fallback_root),
        capacity,
        per_repo,
        // Resolved once at daemon startup (#4005), threaded in read-only —
        // never re-probed per status query.
        credential_preflight: Some(credential_preflight.clone()),
        // Drain fields default to "not draining" here; the drain-aware wrapper
        // [`build_daemon_status_with_drain`] overlays live drain state (#4090).
        // Keeping the base builder drain-agnostic preserves its many existing
        // unit-test call sites unchanged.
        draining: false,
        drain_deadline: None,
        drain_note: None,
    }
}

/// Like [`build_daemon_status`] but overlays the live drain-and-restart state
/// (Issue #4090) so `loom-daemon status` can surface `DRAINING (n remaining,
/// deadline …)`. The IPC `DaemonStatus` handler calls this; the base builder
/// stays drain-agnostic for its existing tests.
#[must_use]
pub fn build_daemon_status_with_drain(
    workspace_pool: &Arc<WorkspacePool>,
    health_states: &WorkspaceHealthStates,
    fallback_root: &Path,
    credential_preflight: &CredentialPreflightReport,
    drain: &DrainState,
) -> DaemonStatusReport {
    let mut report =
        build_daemon_status(workspace_pool, health_states, fallback_root, credential_preflight);
    let snap = drain.snapshot();
    report.draining = drain.is_draining();
    report.drain_deadline = snap.deadline;
    report.drain_note = snap.note;
    report
}

// Allow expect_used because mutex poisoning is a panic-level error that indicates
// a thread panicked while holding the lock. This is not recoverable and should crash.
// Allow too_many_lines because this is a central request dispatcher that handles all IPC commands.
/// Resolve which per-repo [`SweepRegistry`] a sweep request targets (Issue
/// #3929). When `workspace_root` is `Some(non-empty)`, the root is normalized
/// (canonicalize/absolutize — matching how `WorkspaceRegistry::add` and the
/// autonomous loops key the pool) and the pool provisions/returns that repo's
/// registry. When `None`/empty, the daemon's default-workspace registry is
/// returned, preserving pre-#3929 single-repo behavior byte-for-byte.
///
/// A `Some(root)` that equals the seeded default workspace root resolves back to
/// the same shared default registry (the pool returns the seeded instance).
fn resolve_registry(
    default: &Arc<Mutex<SweepRegistry>>,
    workspace_pool: &Arc<WorkspacePool>,
    workspace_root: Option<&str>,
) -> Arc<Mutex<SweepRegistry>> {
    match workspace_root {
        Some(root) if !root.trim().is_empty() => {
            let normalized = crate::workspace_registry::normalize_path(Path::new(root));
            workspace_pool.get_or_provision(&normalized)
        }
        _ => default.clone(),
    }
}

#[allow(clippy::expect_used, clippy::too_many_lines)]
fn handle_request(
    request: Request,
    terminal_manager: &Arc<Mutex<TerminalManager>>,
    activity_db: &Arc<Mutex<ActivityDb>>,
    sweep_registry: &Arc<Mutex<SweepRegistry>>,
    event_bus: &Arc<EventBus>,
    workspace_pool: &Arc<WorkspacePool>,
) -> Response {
    match request {
        Request::Ping => Response::Pong,

        Request::CreateTerminal {
            config_id,
            name,
            working_dir,
            role,
            instance_number,
        } => {
            let mut tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            match tm.create_terminal(&config_id, name, working_dir, role.as_ref(), instance_number)
            {
                Ok(id) => Response::TerminalCreated { id },
                Err(e) => Response::StructuredError(DaemonError::from(e)),
            }
        }

        Request::ListTerminals => {
            let mut tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            Response::TerminalList {
                terminals: tm.list_terminals(),
            }
        }

        Request::DestroyTerminal { id } => {
            let mut tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            match tm.destroy_terminal(&id) {
                Ok(()) => Response::Success,
                Err(e) => Response::StructuredError(DaemonError::from(e)),
            }
        }

        Request::SendInput { id, data } => {
            // Get terminal info to extract role and workspace context
            let mut tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");

            let terminal_info = tm.list_terminals().into_iter().find(|t| t.id == id);

            // Extract context from terminal info
            let (agent_role, working_dir, worktree_path) = if let Some(info) = terminal_info {
                (info.role, info.working_dir, info.worktree_path)
            } else {
                (None, None, None)
            };

            // Determine workspace path (prefer worktree, fallback to working_dir)
            let workspace_path = worktree_path.or(working_dir.clone());

            // Capture current git commit before sending input (for change tracking)
            let before_commit = workspace_path
                .as_ref()
                .and_then(|ws| git_utils::get_current_commit(std::path::Path::new(ws)));

            // Get git branch from workspace
            let git_branch = get_git_branch(workspace_path.as_ref());

            // Record input to activity database with full context
            let input = AgentInput {
                id: None,
                terminal_id: id.clone(),
                timestamp: Utc::now(),
                input_type: InputType::Manual, // Default to manual
                content: data.clone(),
                agent_role,
                context: InputContext {
                    workspace: workspace_path,
                    branch: git_branch,
                    ..Default::default()
                },
            };

            let input_id = if let Ok(db) = activity_db.lock() {
                match db.record_input(&input) {
                    Ok(id) => id,
                    Err(e) => {
                        log::warn!("Failed to record input to activity database: {e}");
                        0 // Use 0 as sentinel for failed recording
                    }
                }
            } else {
                0
            };

            // Send input to terminal
            match tm.send_input(&id, &data) {
                Ok(()) => Response::InputSent {
                    input_id,
                    before_commit,
                },
                Err(e) => Response::StructuredError(DaemonError::from(e)),
            }
        }

        Request::GetTerminalOutput { id, start_byte } => {
            use base64::{engine::general_purpose, Engine as _};

            // Get terminal info first (before releasing lock for output)
            let terminal_info = {
                let mut tm = terminal_manager
                    .lock()
                    .expect("Terminal manager mutex poisoned");
                tm.list_terminals().into_iter().find(|t| t.id == id)
            };

            let tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            match tm.get_terminal_output(&id, start_byte) {
                Ok((output_bytes, byte_count)) => {
                    // Record output sample to activity database if there's new data
                    if !output_bytes.is_empty() {
                        let output_str = String::from_utf8_lossy(&output_bytes).to_string();
                        // Take first 1024 characters (not bytes) to avoid slicing multi-byte UTF-8 chars
                        let preview = if output_str.chars().count() > 1024 {
                            output_str.chars().take(1024).collect::<String>()
                        } else {
                            output_str.clone()
                        };

                        let output_record = AgentOutput {
                            id: None,
                            input_id: None, // Could link to last input if tracked
                            terminal_id: id.clone(),
                            timestamp: Utc::now(),
                            content: Some(output_str.clone()),
                            content_preview: Some(preview),
                            exit_code: None,
                            metadata: None,
                        };

                        if let Ok(db) = activity_db.lock() {
                            if let Err(e) = db.record_output(&output_record) {
                                log::warn!("Failed to record output to activity database: {e}");
                            }

                            // Parse terminal output for forge events and record them
                            // TODO: Read forge_host from configuration once #3135 lands
                            let forge_host = "github.com";
                            let forge_events = parse_forge_events(&output_str, forge_host);
                            for parsed_event in forge_events {
                                let prompt_event = parsed_event.to_prompt_forge_event(None);
                                if let Err(e) = db.record_prompt_forge_event(&prompt_event) {
                                    log::warn!("Failed to record forge event: {e}");
                                } else {
                                    log::debug!(
                                        "Recorded forge event: {:?} (issue: {:?}, pr: {:?})",
                                        prompt_event.event_type,
                                        prompt_event.issue_number,
                                        prompt_event.pr_number
                                    );
                                }
                            }

                            // Parse terminal output for resource usage (token counts, costs)
                            match db.record_resource_usage_from_output(None, &output_str, None) {
                                Ok(Some(usage_id)) => {
                                    log::debug!(
                                        "Recorded resource usage (id: {usage_id}) from terminal output"
                                    );
                                }
                                Ok(None) => {
                                    // No resource usage found in output - this is normal
                                }
                                Err(e) => {
                                    log::warn!("Failed to record resource usage: {e}");
                                }
                            }

                            // Parse terminal output for quality metrics (test results, lint errors, build status)
                            // Issue #1054: Track test and quality outcomes
                            match db.record_quality_from_output(0, &output_str) {
                                Ok(Some(metrics_id)) => {
                                    log::debug!(
                                        "Recorded quality metrics (id: {metrics_id}) from terminal output"
                                    );
                                }
                                Ok(None) => {
                                    // No quality metrics found in output - this is normal
                                }
                                Err(e) => {
                                    log::warn!("Failed to record quality metrics: {e}");
                                }
                            }

                            // Parse terminal output for git commits and record changes
                            // This enables automatic prompt-to-commit correlation
                            if git_parser::contains_git_commit(&output_str) {
                                let git_commits = git_parser::parse_git_commits(&output_str);
                                for commit_event in git_commits {
                                    log::info!(
                                        "Detected git commit: {} ({:?})",
                                        commit_event.commit_hash,
                                        commit_event.commit_message
                                    );

                                    // Record the commit correlation if we have the terminal's workspace
                                    if let Some(ref info) = terminal_info {
                                        let workspace_path = info
                                            .worktree_path
                                            .as_ref()
                                            .or(info.working_dir.as_ref());

                                        if let Some(ws) = workspace_path {
                                            // Create a prompt_changes record linking to the commit
                                            // We use the commit hash as after_commit
                                            // The input_id would ideally link to the most recent input
                                            // but we don't have that context here, so we record
                                            // the commit with metrics from the parsed output
                                            let changes = crate::activity::PromptChanges {
                                                id: None,
                                                input_id: 0, // Will be correlated by timestamp
                                                before_commit: None,
                                                after_commit: Some(
                                                    commit_event.commit_hash.clone(),
                                                ),
                                                files_changed: commit_event
                                                    .files_changed
                                                    .unwrap_or(0),
                                                lines_added: commit_event.lines_added.unwrap_or(0),
                                                lines_removed: commit_event
                                                    .lines_removed
                                                    .unwrap_or(0),
                                                tests_added: 0, // Not available from commit output
                                                tests_modified: 0,
                                            };

                                            if let Err(e) = db.record_prompt_changes(&changes) {
                                                log::warn!(
                                                    "Failed to record git commit correlation: {e}"
                                                );
                                            } else {
                                                log::debug!(
                                                    "Recorded git commit {} in workspace {}",
                                                    commit_event.commit_hash,
                                                    ws
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Encode bytes as base64 for JSON transmission
                    let output = general_purpose::STANDARD.encode(&output_bytes);
                    log::debug!(
                        "GetTerminalOutput: {} raw bytes -> {} base64 chars, total byte_count={}",
                        output_bytes.len(),
                        output.len(),
                        byte_count
                    );
                    Response::TerminalOutput { output, byte_count }
                }
                Err(e) => Response::StructuredError(DaemonError::from(e)),
            }
        }

        Request::ResizeTerminal { id, cols, rows } => {
            let tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            match tm.resize_terminal(&id, cols, rows) {
                Ok(()) => Response::Success,
                Err(e) => Response::StructuredError(DaemonError::from(e)),
            }
        }

        Request::CheckSessionHealth { id } => {
            let tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            match tm.has_tmux_session(&id) {
                Ok(has_session) => Response::SessionHealth { has_session },
                Err(e) => Response::StructuredError(DaemonError::from(e)),
            }
        }

        Request::ListAvailableSessions => {
            let tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            let sessions = tm.list_available_sessions();
            Response::AvailableSessions { sessions }
        }

        Request::AttachToSession { id, session_name } => {
            let mut tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            match tm.attach_to_session(&id, session_name) {
                Ok(()) => Response::Success,
                Err(e) => Response::StructuredError(DaemonError::from(e)),
            }
        }

        Request::KillSession { session_name } => {
            let tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            match tm.kill_session(&session_name) {
                Ok(()) => Response::Success,
                Err(e) => Response::StructuredError(DaemonError::from(e)),
            }
        }

        Request::SetWorktreePath { id, worktree_path } => {
            let mut tm = terminal_manager
                .lock()
                .expect("Terminal manager mutex poisoned");
            match tm.set_worktree_path(&id, &worktree_path) {
                Ok(()) => Response::Success,
                Err(e) => Response::StructuredError(DaemonError::from(e)),
            }
        }

        Request::GetTerminalActivity { id, limit } => {
            if let Ok(db) = activity_db.lock() {
                match db.get_terminal_activity(&id, limit) {
                    Ok(entries) => Response::TerminalActivity { entries },
                    Err(e) => {
                        log::error!("Failed to get terminal activity: {e}");
                        Response::StructuredError(DaemonError::activity_query_failed(
                            "get terminal activity",
                            &e.to_string(),
                        ))
                    }
                }
            } else {
                Response::StructuredError(DaemonError::activity_db_locked())
            }
        }

        Request::CaptureGitChanges {
            input_id,
            working_dir,
            before_commit,
        } => {
            let working_path = std::path::Path::new(&working_dir);

            // Capture git changes
            if let Some(changes) =
                git_utils::capture_prompt_changes(working_path, input_id, before_commit)
            {
                // Record to database
                if let Ok(db) = activity_db.lock() {
                    match db.record_prompt_changes(&changes) {
                        Ok(_) => Response::GitChangesCaptured {
                            files_changed: changes.files_changed,
                            lines_added: changes.lines_added,
                            lines_removed: changes.lines_removed,
                        },
                        Err(e) => {
                            log::error!("Failed to record prompt changes: {e}");
                            Response::StructuredError(DaemonError::activity_query_failed(
                                "record prompt changes",
                                &e.to_string(),
                            ))
                        }
                    }
                } else {
                    Response::StructuredError(DaemonError::activity_db_locked())
                }
            } else {
                // No changes detected or not a git repo
                Response::GitChangesCaptured {
                    files_changed: 0,
                    lines_added: 0,
                    lines_removed: 0,
                }
            }
        }

        Request::GetCurrentCommit { working_dir } => {
            let working_path = std::path::Path::new(&working_dir);
            let commit = git_utils::get_current_commit(working_path);
            Response::CurrentCommit { commit }
        }

        // ====================================================================
        // Issue Claim Registry Handlers (Issue #1159)
        // ====================================================================
        Request::ClaimIssue {
            number,
            claim_type,
            terminal_id,
            label,
            agent_role,
            stale_threshold_secs,
        } => {
            if let Ok(db) = activity_db.lock() {
                match db.claim_issue(
                    number,
                    claim_type,
                    &terminal_id,
                    label.as_deref(),
                    agent_role.as_deref(),
                    stale_threshold_secs,
                ) {
                    Ok(result) => Response::ClaimResult(result),
                    Err(e) => Response::StructuredError(DaemonError::activity_query_failed(
                        "claim issue",
                        &e.to_string(),
                    )),
                }
            } else {
                Response::StructuredError(DaemonError::activity_db_locked())
            }
        }

        Request::ReleaseClaim {
            number,
            claim_type,
            terminal_id,
        } => {
            if let Ok(db) = activity_db.lock() {
                match db.release_claim(number, claim_type, terminal_id.as_deref()) {
                    Ok(released) => {
                        if released {
                            Response::Success
                        } else {
                            Response::StructuredError(
                                DaemonError::new(
                                    crate::errors::ErrorDomain::Activity,
                                    crate::errors::ErrorCode::ACTIVITY_QUERY_FAILED,
                                    "Claim not found or not owned",
                                )
                                .recoverable(false),
                            )
                        }
                    }
                    Err(e) => Response::StructuredError(DaemonError::activity_query_failed(
                        "release claim",
                        &e.to_string(),
                    )),
                }
            } else {
                Response::StructuredError(DaemonError::activity_db_locked())
            }
        }

        Request::HeartbeatClaim {
            number,
            claim_type,
            terminal_id,
        } => {
            if let Ok(db) = activity_db.lock() {
                match db.heartbeat_claim(number, claim_type, &terminal_id) {
                    Ok(updated) => {
                        if updated {
                            Response::Success
                        } else {
                            Response::StructuredError(
                                DaemonError::new(
                                    crate::errors::ErrorDomain::Activity,
                                    crate::errors::ErrorCode::ACTIVITY_QUERY_FAILED,
                                    "Claim not found or not owned",
                                )
                                .recoverable(false),
                            )
                        }
                    }
                    Err(e) => Response::StructuredError(DaemonError::activity_query_failed(
                        "update heartbeat",
                        &e.to_string(),
                    )),
                }
            } else {
                Response::StructuredError(DaemonError::activity_db_locked())
            }
        }

        Request::GetClaim { number, claim_type } => {
            if let Ok(db) = activity_db.lock() {
                match db.get_claim(number, claim_type) {
                    Ok(claim) => Response::Claim(claim),
                    Err(e) => Response::StructuredError(DaemonError::activity_query_failed(
                        "get claim",
                        &e.to_string(),
                    )),
                }
            } else {
                Response::StructuredError(DaemonError::activity_db_locked())
            }
        }

        Request::GetTerminalClaims { terminal_id } => {
            if let Ok(db) = activity_db.lock() {
                match db.get_claims_by_terminal(&terminal_id) {
                    Ok(claims) => Response::Claims(claims),
                    Err(e) => Response::StructuredError(DaemonError::activity_query_failed(
                        "get terminal claims",
                        &e.to_string(),
                    )),
                }
            } else {
                Response::StructuredError(DaemonError::activity_db_locked())
            }
        }

        Request::GetAllClaims => {
            if let Ok(db) = activity_db.lock() {
                match db.get_all_claims() {
                    Ok(claims) => Response::Claims(claims),
                    Err(e) => Response::StructuredError(DaemonError::activity_query_failed(
                        "get all claims",
                        &e.to_string(),
                    )),
                }
            } else {
                Response::StructuredError(DaemonError::activity_db_locked())
            }
        }

        Request::GetClaimsSummary {
            stale_threshold_secs,
        } => {
            if let Ok(db) = activity_db.lock() {
                let threshold = stale_threshold_secs.unwrap_or(3600);
                match db.get_claims_summary(threshold) {
                    Ok(summary) => Response::ClaimsSummary(summary),
                    Err(e) => Response::StructuredError(DaemonError::activity_query_failed(
                        "get claims summary",
                        &e.to_string(),
                    )),
                }
            } else {
                Response::StructuredError(DaemonError::activity_db_locked())
            }
        }

        Request::ReleaseStaleCliams {
            stale_threshold_secs,
        } => {
            if let Ok(db) = activity_db.lock() {
                let threshold = stale_threshold_secs.unwrap_or(3600);
                match db.release_stale_claims(threshold) {
                    Ok(count) => Response::ClaimsReleased { count },
                    Err(e) => Response::StructuredError(DaemonError::activity_query_failed(
                        "release stale claims",
                        &e.to_string(),
                    )),
                }
            } else {
                Response::StructuredError(DaemonError::activity_db_locked())
            }
        }

        Request::ReleaseTerminalClaims { terminal_id } => {
            if let Ok(db) = activity_db.lock() {
                match db.release_terminal_claims(&terminal_id) {
                    Ok(count) => Response::ClaimsReleased { count },
                    Err(e) => Response::StructuredError(DaemonError::activity_query_failed(
                        "release terminal claims",
                        &e.to_string(),
                    )),
                }
            } else {
                Response::StructuredError(DaemonError::activity_db_locked())
            }
        }

        // ====================================================================
        // Sweep Registry Handlers (Issue #3452 — Phase A of #3449)
        // ====================================================================
        Request::DispatchSweep {
            kind,
            idempotency_key,
            model,
            effort,
            depends_on,
            workspace_root,
        } => {
            let target =
                resolve_registry(sweep_registry, workspace_pool, workspace_root.as_deref());
            let mut sr = target.lock().expect("Sweep registry mutex poisoned");
            // Model resolution (issue #3944): an explicit `model` param still
            // wins, but an ABSENT one falls back to `autonomous.model` in
            // `.loom/config.json` and then the shipped non-premium default —
            // never the operator's interactive CLI default. This mirrors the
            // autonomous work-finder / epic-supervisor dispatch paths so every
            // daemon-dispatched child is pinned to an explicit model.
            let repo_root = sr.config().workspace_root.clone();
            let (resolved_model, model_source) =
                crate::sweep_registry::resolve_dispatch_model(&repo_root, model.as_deref());
            log::info!(
                "dispatch_sweep: {:?} with model={resolved_model} (source={})",
                kind,
                model_source.as_str()
            );
            match sr.dispatch(
                &kind,
                idempotency_key,
                Some(&resolved_model),
                effort.as_deref(),
                depends_on,
            ) {
                Ok(outcome) => Response::SweepDispatched {
                    sweep_id: outcome.sweep_id,
                    pid: outcome.pid,
                    token_name: outcome.token_name,
                    log_path: outcome.log_path,
                },
                Err(e) => Response::Error {
                    message: format!("dispatch_sweep failed: {e}"),
                },
            }
        }

        Request::ListSweeps {
            state_filter,
            workspace_root,
        } => {
            let target =
                resolve_registry(sweep_registry, workspace_pool, workspace_root.as_deref());
            let mut sr = target.lock().expect("Sweep registry mutex poisoned");
            // Reap-on-read (Issue #3893): reconcile liveness before listing so a
            // sweep whose child has already exited is never reported `Running`
            // just because the 30s reaper timer has not ticked yet.
            sr.reap_liveness();
            let sweeps = sr.list(state_filter.as_ref());
            Response::SweepList { sweeps }
        }

        // ====================================================================
        // Sweep Monitoring Handlers (Issue #3455 — Phase C of #3449)
        // ====================================================================
        Request::GetSweepStatus {
            sweep_id,
            workspace_root,
        } => {
            let target =
                resolve_registry(sweep_registry, workspace_pool, workspace_root.as_deref());
            let mut sr = target.lock().expect("Sweep registry mutex poisoned");
            // Reap-on-read (Issue #3893): reconcile liveness so a status query
            // reflects a child that has exited rather than a stale `Running`.
            sr.reap_liveness();
            let info = sr.get_status(&sweep_id);
            Response::SweepStatus { info }
        }

        Request::TailSweepLog {
            sweep_id,
            lines,
            workspace_root,
        } => {
            let target =
                resolve_registry(sweep_registry, workspace_pool, workspace_root.as_deref());
            let sr = target.lock().expect("Sweep registry mutex poisoned");
            match sr.tail_log(&sweep_id, lines) {
                Ok((log_path, lines)) => Response::SweepLogTail {
                    sweep_id,
                    lines,
                    log_path,
                },
                Err(e) => Response::Error {
                    message: format!("tail_sweep_log failed: {e}"),
                },
            }
        }

        Request::CancelSweep {
            sweep_id,
            grace_secs,
            workspace_root,
        } => {
            // Production traffic never reaches this arm: `handle_client`
            // intercepts `CancelSweep` and services it via the non-blocking
            // async `cancel_sweep_nonblocking` (Issue #3807) so the grace
            // window does not hold the registry mutex. This synchronous
            // fallback (holding the lock across the full grace) remains for
            // direct/unit-test callers where lock contention is irrelevant.
            let target =
                resolve_registry(sweep_registry, workspace_pool, workspace_root.as_deref());
            let mut sr = target.lock().expect("Sweep registry mutex poisoned");
            match sr.cancel(&sweep_id, std::time::Duration::from_secs(grace_secs)) {
                Ok(outcome) => Response::SweepCancelled {
                    sweep_id: outcome.sweep_id,
                    pid: outcome.pid,
                    sigkill_sent: outcome.sigkill_sent,
                    was_running: outcome.was_running,
                },
                Err(e) => Response::Error {
                    message: format!("cancel_sweep failed: {e}"),
                },
            }
        }

        Request::ClearQuarantine {
            issue,
            workspace_root,
        } => {
            // Operator-reachable insta-crash-quarantine release (Issue #3939).
            // Clears the daemon's in-memory quarantine + insta-crash tally for
            // `issue` (and restores `loom:issue` on the forge) so the work
            // finder re-qualifies it immediately instead of waiting for the TTL.
            let target =
                resolve_registry(sweep_registry, workspace_pool, workspace_root.as_deref());
            let mut sr = target.lock().expect("Sweep registry mutex poisoned");
            let was_quarantined = sr.clear_quarantine(issue);
            Response::QuarantineCleared {
                issue,
                was_quarantined,
            }
        }

        // ====================================================================
        // Event Bus Handlers (Issue #3453 — Phase B of #3449)
        // ====================================================================
        Request::PublishEvent { topic, payload } => {
            // Generic publish path used by sweep children — the topic is
            // the canonical name (e.g., "sweep.issue.123.phase") and the
            // payload is opaque JSON. See `defaults/.claude/commands/loom/
            // sweep.md` for the per-topic payload schema.
            let event = Event::Generic {
                topic: topic.clone(),
                payload,
            };
            match event_bus.publish(event) {
                Ok(receivers) => Response::EventPublished { topic, receivers },
                Err(_) => Response::EventPublished {
                    topic,
                    receivers: 0,
                },
            }
        }

        Request::SubscribeEvents { .. } => {
            // SubscribeEvents is intercepted in `handle_client` before it
            // reaches this dispatcher because it requires a streaming
            // response (not a single Response frame). If this branch is
            // ever reached, the IPC server's handle_client logic is bugged
            // — fail loud so it doesn't silently mis-route.
            Response::Error {
                message: "internal: SubscribeEvents must be handled by stream_events, not \
                          handle_request"
                    .to_string(),
            }
        }

        Request::DaemonStatus => {
            // DaemonStatus is intercepted in `handle_client` before it reaches
            // this dispatcher because it needs the `main_health_state` halt flag
            // (Issue #3891), which this synchronous dispatcher does not receive.
            // Reaching this arm means the intercept was removed — fail loud so
            // the mis-route is visible rather than silently returning a wrong
            // (halt-unaware) report.
            Response::Error {
                message: "internal: DaemonStatus must be handled by build_daemon_status in \
                          handle_client, not handle_request"
                    .to_string(),
            }
        }

        // ====================================================================
        // Workspace Registry Handlers (Issue #3926 — phase 1 of #3835)
        // ====================================================================
        Request::RegisterWorkspace {
            root,
            config_overrides,
        } => handle_register_workspace(&root, config_overrides),

        Request::DeregisterWorkspace { root } => handle_deregister_workspace(&root, workspace_pool),

        Request::ListWorkspaces => handle_list_workspaces(),

        // ====================================================================
        // Durable Watch Registry Handlers (Issue #3971)
        // ====================================================================
        Request::RegisterWatch {
            kind,
            number,
            repo,
            workspace_root,
            note,
        } => handle_register_watch(kind, number, repo, workspace_root, note),

        Request::ListWatches => handle_list_watches(),

        Request::RemoveWatch { id } => handle_remove_watch(&id),

        Request::Shutdown => {
            // Exit NON-ZERO (Issue #4054): an explicit shutdown means "stay
            // down", so under launchd `KeepAlive:SuccessfulExit` this must not
            // trip a relaunch. Only `RestartDaemon` (handled in `handle_client`)
            // exits 0. See the EXIT_* constants at the top of this module.
            log::info!("Shutdown requested (exiting {EXIT_SHUTDOWN}; not a supervised relaunch)");
            std::process::exit(EXIT_SHUTDOWN);
        }
        Request::RestartDaemon => {
            // Structurally unreachable: `handle_client` intercepts
            // `RestartDaemon` before dispatching to `handle_request` (it must
            // reply-then-exit). Answer defensively in case of a future direct
            // caller — do NOT exit here, only `handle_client` may end the
            // process for a supervised relaunch.
            build_restart_decision().0
        }
        Request::DrainAndRestartDaemon { .. } | Request::AbortDrain => {
            // Structurally unreachable: `handle_client` intercepts both drain
            // requests (#4090) before dispatching here, because a drain must ack
            // immediately and exit from a background supervisor task minutes
            // later — state the connection-scoped `handle_request` cannot own.
            Response::Error {
                message: "internal: drain requests must be handled by handle_client, not \
                          handle_request"
                    .to_string(),
            }
        }
    }
}

/// Load, mutate, and persist the machine-level workspace registry for a
/// `RegisterWorkspace` request. Both the CLI and this IPC handler operate on the
/// same `~/.loom/workspaces.json` file, so an edit through either surface is
/// visible to the other (hot-apply).
fn handle_register_workspace(root: &str, config_overrides: Option<serde_json::Value>) -> Response {
    use crate::workspace_registry::{default_registry_path, AddOutcome, WorkspaceRegistry};

    let path = match default_registry_path() {
        Ok(p) => p,
        Err(e) => {
            return Response::Error {
                message: format!("register_workspace: {e}"),
            }
        }
    };
    let mut registry = match WorkspaceRegistry::load(&path) {
        Ok(r) => r,
        Err(e) => {
            return Response::Error {
                message: format!("register_workspace: load failed: {e}"),
            }
        }
    };
    match registry.add(Path::new(root), config_overrides) {
        Ok(AddOutcome::AlreadyPresent { canonical }) => Response::WorkspaceRegistered {
            root: canonical,
            already_present: true,
            looks_like_workspace: true,
        },
        Ok(AddOutcome::Added {
            canonical,
            looks_like_workspace,
        }) => {
            if let Err(e) = registry.save(&path) {
                return Response::Error {
                    message: format!("register_workspace: save failed: {e}"),
                };
            }
            Response::WorkspaceRegistered {
                root: canonical,
                already_present: false,
                looks_like_workspace,
            }
        }
        Err(e) => Response::Error {
            message: format!("register_workspace: {e}"),
        },
    }
}

/// Load, mutate, and persist the workspace registry for a `DeregisterWorkspace`
/// request, then evict the deregistered repo's in-memory sweep registry from the
/// [`WorkspacePool`] (Issue #3929) so its background reaper stops and it does not
/// leak. The seeded default workspace is guarded inside [`WorkspacePool::evict`]
/// (a no-op there), and a live sweep child is never killed — only the in-memory
/// tracking goes away.
fn handle_deregister_workspace(root: &str, workspace_pool: &Arc<WorkspacePool>) -> Response {
    use crate::workspace_registry::{default_registry_path, normalize_path, WorkspaceRegistry};

    let path = match default_registry_path() {
        Ok(p) => p,
        Err(e) => {
            return Response::Error {
                message: format!("deregister_workspace: {e}"),
            }
        }
    };
    let mut registry = match WorkspaceRegistry::load(&path) {
        Ok(r) => r,
        Err(e) => {
            return Response::Error {
                message: format!("deregister_workspace: load failed: {e}"),
            }
        }
    };
    let canonical = normalize_path(Path::new(root));
    let was_present = registry.remove(Path::new(root));
    if was_present {
        if let Err(e) = registry.save(&path) {
            return Response::Error {
                message: format!("deregister_workspace: save failed: {e}"),
            };
        }
    }
    // Evict the in-memory pool entry (best-effort, idempotent). The pool keys on
    // the same normalized root the registry stores, and guards the seeded
    // default workspace internally.
    let evicted = workspace_pool.evict(&canonical);
    if evicted {
        log::info!(
            "deregister_workspace: evicted pooled sweep registry for {}",
            canonical.display()
        );
    }
    Response::WorkspaceDeregistered {
        root: canonical,
        was_present,
    }
}

/// Load and return the workspace registry for a `ListWorkspaces` request.
fn handle_list_workspaces() -> Response {
    use crate::workspace_registry::{default_registry_path, WorkspaceRegistry};

    let path = match default_registry_path() {
        Ok(p) => p,
        Err(e) => {
            return Response::Error {
                message: format!("list_workspaces: {e}"),
            }
        }
    };
    match WorkspaceRegistry::load(&path) {
        Ok(registry) => Response::WorkspaceList {
            workspaces: registry.workspaces,
        },
        Err(e) => Response::Error {
            message: format!("list_workspaces: load failed: {e}"),
        },
    }
}

/// Register a durable watch (Issue #3971). Operates on the machine-level watches
/// file (`~/.loom/watches.json`) directly — like [`handle_register_workspace`],
/// the daemon's monitor loop re-reads the file each tick (hot-apply), so a watch
/// added here is picked up without any in-memory registration.
fn handle_register_watch(
    kind: crate::watch_registry::WatchKind,
    number: u32,
    repo: Option<String>,
    workspace_root: Option<String>,
    note: Option<String>,
) -> Response {
    use crate::watch_registry::{default_watches_path, load, new_watch, save, with_watches_lock};

    let path = match default_watches_path() {
        Ok(p) => p,
        Err(e) => {
            return Response::Error {
                message: format!("register_watch: {e}"),
            }
        }
    };
    // The load→modify→save runs under the watches-file lock: the background
    // monitor loop is an independent concurrent writer to the same file, so an
    // unguarded read-modify-write here could be silently clobbered (Issue #3971
    // durability guarantee).
    let outcome = with_watches_lock(&path, || {
        let mut registry = load(&path);
        let (watch, was_new) = registry.add(new_watch(kind, number, repo, workspace_root, note));
        if was_new {
            save(&path, &registry)?;
        }
        Ok((watch, was_new))
    });
    match outcome {
        Ok((watch, was_new)) => Response::WatchRegistered {
            watch,
            already_present: !was_new,
        },
        Err(e) => Response::Error {
            message: format!("register_watch: save failed: {e}"),
        },
    }
}

/// List the currently-registered durable watches (Issue #3971).
fn handle_list_watches() -> Response {
    use crate::watch_registry::{default_watches_path, load};

    let path = match default_watches_path() {
        Ok(p) => p,
        Err(e) => {
            return Response::Error {
                message: format!("list_watches: {e}"),
            }
        }
    };
    Response::WatchList {
        watches: load(&path).watches,
    }
}

/// Remove a registered durable watch by id (Issue #3971).
fn handle_remove_watch(id: &str) -> Response {
    use crate::watch_registry::{default_watches_path, load, save, with_watches_lock};

    let path = match default_watches_path() {
        Ok(p) => p,
        Err(e) => {
            return Response::Error {
                message: format!("remove_watch: {e}"),
            }
        }
    };
    // Guarded by the watches-file lock — same concurrent-writer reasoning as
    // handle_register_watch (Issue #3971).
    let outcome = with_watches_lock(&path, || {
        let mut registry = load(&path);
        let was_present = registry.remove(id);
        if was_present {
            save(&path, &registry)?;
        }
        Ok(was_present)
    });
    match outcome {
        Ok(was_present) => Response::WatchRemoved {
            id: id.to_string(),
            was_present,
        },
        Err(e) => Response::Error {
            message: format!("remove_watch: save failed: {e}"),
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::activity::ActivityDb;
    use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
    use crate::types::SweepKind;
    use tempfile::tempdir;

    type TestContext = (
        Arc<Mutex<TerminalManager>>,
        Arc<Mutex<ActivityDb>>,
        Arc<Mutex<SweepRegistry>>,
        Arc<EventBus>,
    );

    /// A process-wide leaked runtime handle so [`WorkspacePool`]s can be built in
    /// synchronous `#[test]` cases (Issue #3929). Reapers spawned onto it during
    /// provisioning are harmless in tests.
    fn test_runtime_handle() -> tokio::runtime::Handle {
        use std::sync::OnceLock;
        static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
        RT.get_or_init(|| tokio::runtime::Runtime::new().unwrap())
            .handle()
            .clone()
    }

    /// A fixture credential-preflight snapshot for `build_daemon_status` tests
    /// (#4005) — these tests exercise the dynamic-cap/health-gate machinery,
    /// not credential resolution, so a fixed `Ok` snapshot keeps them focused.
    fn test_credential_preflight() -> CredentialPreflightReport {
        CredentialPreflightReport {
            ok: true,
            mechanism: "test-fixture".to_string(),
            fingerprint: None,
            message: "test fixture — not a real preflight".to_string(),
            checked_at: Utc::now(),
        }
    }

    /// A [`WorkspacePool`] for `handle_request` tests (Issue #3929). The
    /// default-workspace (`workspace_root: None`) paths these tests exercise
    /// never provision, so no task is actually spawned.
    fn test_pool() -> Arc<WorkspacePool> {
        Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()))
    }

    fn setup_test_context() -> TestContext {
        let tm = Arc::new(Mutex::new(TerminalManager::new()));
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test_activity.db");
        let db = ActivityDb::new(db_path).unwrap();
        let db = Arc::new(Mutex::new(db));
        let mut sr_config = SweepRegistryConfig::new(dir.path().to_path_buf());
        sr_config.skip_label_flip = true;
        let bus = Arc::new(EventBus::new());
        let mut registry = SweepRegistry::new(sr_config);
        registry.set_event_bus(bus.clone());
        let sr = Arc::new(Mutex::new(registry));
        // Keep dir alive so the temp directory isn't deleted
        std::mem::forget(dir);
        (tm, db, sr, bus)
    }

    // ===== Ping/Pong =====

    #[test]
    fn test_handle_request_ping() {
        let (tm, db, sr, bus) = setup_test_context();
        let response = handle_request(Request::Ping, &tm, &db, &sr, &bus, &test_pool());
        assert!(matches!(response, Response::Pong));
    }

    // ===== ListTerminals =====

    #[test]
    fn test_handle_request_list_terminals_empty() {
        let (tm, db, sr, bus) = setup_test_context();
        // Set LOOM_NO_RESTORE to prevent tmux restore attempts
        std::env::set_var("LOOM_NO_RESTORE", "1");
        let response = handle_request(Request::ListTerminals, &tm, &db, &sr, &bus, &test_pool());
        std::env::remove_var("LOOM_NO_RESTORE");
        match response {
            Response::TerminalList { terminals } => {
                assert!(terminals.is_empty());
            }
            other => panic!("Expected TerminalList, got: {other:?}"),
        }
    }

    // ===== GetCurrentCommit =====

    #[test]
    fn test_handle_request_get_current_commit_nonexistent_dir() {
        let (tm, db, sr, bus) = setup_test_context();
        let response = handle_request(
            Request::GetCurrentCommit {
                working_dir: "/nonexistent/path".to_string(),
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::CurrentCommit { commit } => {
                assert!(commit.is_none());
            }
            other => panic!("Expected CurrentCommit, got: {other:?}"),
        }
    }

    // ===== GetTerminalActivity =====

    #[test]
    fn test_handle_request_get_terminal_activity_empty() {
        let (tm, db, sr, bus) = setup_test_context();
        let response = handle_request(
            Request::GetTerminalActivity {
                id: "nonexistent".to_string(),
                limit: 10,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::TerminalActivity { entries } => {
                assert!(entries.is_empty());
            }
            other => panic!("Expected TerminalActivity, got: {other:?}"),
        }
    }

    // ===== GetAllClaims =====

    #[test]
    fn test_handle_request_get_all_claims_empty() {
        let (tm, db, sr, bus) = setup_test_context();
        let response = handle_request(Request::GetAllClaims, &tm, &db, &sr, &bus, &test_pool());
        match response {
            Response::Claims(claims) => {
                assert!(claims.is_empty());
            }
            other => panic!("Expected Claims, got: {other:?}"),
        }
    }

    // ===== GetClaimsSummary =====

    #[test]
    fn test_handle_request_get_claims_summary() {
        let (tm, db, sr, bus) = setup_test_context();
        let response = handle_request(
            Request::GetClaimsSummary {
                stale_threshold_secs: Some(3600),
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::ClaimsSummary(summary) => {
                assert_eq!(summary.total_claims, 0);
            }
            other => panic!("Expected ClaimsSummary, got: {other:?}"),
        }
    }

    // ===== CaptureGitChanges with nonexistent dir =====

    #[test]
    fn test_handle_request_capture_git_changes_no_repo() {
        let (tm, db, sr, bus) = setup_test_context();
        let response = handle_request(
            Request::CaptureGitChanges {
                input_id: 1,
                working_dir: "/nonexistent/path".to_string(),
                before_commit: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::GitChangesCaptured {
                files_changed,
                lines_added,
                lines_removed,
            } => {
                assert_eq!(files_changed, 0);
                assert_eq!(lines_added, 0);
                assert_eq!(lines_removed, 0);
            }
            other => panic!("Expected GitChangesCaptured, got: {other:?}"),
        }
    }

    // ===== get_git_branch tests =====

    #[test]
    fn test_get_git_branch_none_input() {
        assert!(get_git_branch(None).is_none());
    }

    #[test]
    fn test_get_git_branch_nonexistent_dir() {
        let dir = "/nonexistent/path".to_string();
        assert!(get_git_branch(Some(&dir)).is_none());
    }

    // ===== Sweep registry IPC handlers (Issue #3452) =====

    /// Build a SweepRegistry that won't actually launch real children.
    /// The fixture spawn binary writes its argv AND a handful of env vars
    /// (notably `LOOM_SWEEP_CLAIM_OWNED`, Issue #3823/#3967) to a sibling log
    /// and exits immediately (same pattern as the sweep_registry unit tests).
    fn setup_sweep_registry_in_tempdir(
    ) -> (Arc<Mutex<SweepRegistry>>, tempfile::TempDir, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let scripts_dir = dir.path().join(".loom").join("scripts");
        std::fs::create_dir_all(&scripts_dir).unwrap();
        let fake_bin = scripts_dir.join("spawn-claude.sh");
        let record_log = dir.path().join("ipc-fake-spawn.log");
        let script = format!(
            r#"#!/usr/bin/env bash
{{
  echo "argv: $*"
  printf 'LOOM_SWEEP_CLAIM_OWNED=%s\n' "${{LOOM_SWEEP_CLAIM_OWNED:-unset}}"
}} >> "{rec}"
exit 0
"#,
            rec = record_log.display()
        );
        std::fs::write(&fake_bin, script).unwrap();
        let mut perms = std::fs::metadata(&fake_bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_bin, perms).unwrap();

        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.spawn_bin = Some(fake_bin);
        config.skip_label_flip = true;
        // Confine the #3953 sweep journal to this test's tempdir — never the
        // real machine-level `~/.loom/sweeps.json`.
        config.journal_path = Some(dir.path().join("test-sweeps-journal.json"));
        let sr = Arc::new(Mutex::new(SweepRegistry::new(config)));
        (sr, dir, record_log)
    }

    #[test]
    fn test_handle_request_list_sweeps_empty() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
        let response = handle_request(
            Request::ListSweeps {
                state_filter: None,
                workspace_root: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::SweepList { sweeps } => {
                assert!(sweeps.is_empty());
            }
            other => panic!("Expected SweepList, got: {other:?}"),
        }
    }

    /// Issue #3929: a request carrying an explicit `workspace_root` routes to
    /// that repo's registry (via the pool), not the default workspace — and the
    /// returned `SweepInfo` carries the owning `repo`. Omitting `workspace_root`
    /// preserves default-workspace-only behavior (regression guard).
    #[test]
    #[serial_test::serial]
    fn test_sweep_requests_route_to_explicit_workspace_root() {
        let (tm, db, _, bus) = setup_test_context();

        // Default workspace (repo A) and a second managed repo (repo B), each a
        // fixture registry with a fake spawn bin + skip_label_flip.
        let (sr_default, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
        let (sr_b, dir_b, _rec_b) = setup_sweep_registry_in_tempdir();
        let root_a = crate::workspace_registry::normalize_path(dir_a.path());
        let root_b = crate::workspace_registry::normalize_path(dir_b.path());

        // A pool seeded with both registries (mirrors main's seed of the default
        // workspace, plus repo B provisioned by the autonomous loops).
        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(root_a, sr_default.clone());
        pool.seed(root_b.clone(), sr_b.clone());

        // Dispatch issue #42 into repo B explicitly.
        let dispatched = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(42),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                workspace_root: Some(dir_b.path().to_string_lossy().into_owned()),
            },
            &tm,
            &db,
            &sr_default,
            &bus,
            &pool,
        );
        let sweep_id = match dispatched {
            Response::SweepDispatched { sweep_id, .. } => sweep_id,
            other => panic!("Expected SweepDispatched, got: {other:?}"),
        };

        // repo B's registry sees the sweep, and its SweepInfo.repo names repo B.
        let listed_b = handle_request(
            Request::ListSweeps {
                state_filter: None,
                workspace_root: Some(dir_b.path().to_string_lossy().into_owned()),
            },
            &tm,
            &db,
            &sr_default,
            &bus,
            &pool,
        );
        match listed_b {
            Response::SweepList { sweeps } => {
                assert_eq!(sweeps.len(), 1, "repo B registry should hold the sweep");
                assert_eq!(
                    sweeps[0].repo.as_deref(),
                    Some(dir_b.path().display().to_string().as_str()),
                    "SweepInfo.repo must name the owning workspace root"
                );
            }
            other => panic!("Expected SweepList, got: {other:?}"),
        }

        // The default workspace (workspace_root: None) must NOT see repo B's
        // sweep — this is the identity guarantee (two repos' issue #42 differ).
        let listed_default = handle_request(
            Request::ListSweeps {
                state_filter: None,
                workspace_root: None,
            },
            &tm,
            &db,
            &sr_default,
            &bus,
            &pool,
        );
        match listed_default {
            Response::SweepList { sweeps } => {
                assert!(sweeps.is_empty(), "default workspace must not see repo B's sweep");
            }
            other => panic!("Expected SweepList, got: {other:?}"),
        }

        // GetSweepStatus is likewise workspace-scoped: found in repo B, absent
        // from the default workspace.
        let status_b = handle_request(
            Request::GetSweepStatus {
                sweep_id: sweep_id.clone(),
                workspace_root: Some(dir_b.path().to_string_lossy().into_owned()),
            },
            &tm,
            &db,
            &sr_default,
            &bus,
            &pool,
        );
        assert!(
            matches!(status_b, Response::SweepStatus { info: Some(_) }),
            "sweep is observable via repo B's registry"
        );
        let status_default = handle_request(
            Request::GetSweepStatus {
                sweep_id,
                workspace_root: None,
            },
            &tm,
            &db,
            &sr_default,
            &bus,
            &pool,
        );
        assert!(
            matches!(status_default, Response::SweepStatus { info: None }),
            "sweep is NOT observable via the default workspace"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_handle_request_dispatch_sweep_happy_path() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();

        let response = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(2024),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                workspace_root: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::SweepDispatched {
                sweep_id,
                pid,
                token_name,
                log_path,
            } => {
                assert!(sweep_id.starts_with("sweep-issue-2024-"));
                assert!(pid > 0);
                assert_eq!(token_name, "unknown");
                assert!(log_path.to_string_lossy().contains("sweep-issue-2024.log"));
            }
            other => panic!("Expected SweepDispatched, got: {other:?}"),
        }

        // Follow-up ListSweeps should see the new entry. The fake spawn exits
        // immediately, so reap-on-read (Issue #3893) promptly reconciles the
        // entry to a terminal `Exited` state rather than over-reporting it as
        // `Running` — the entry is still listed, just no longer stale-Running.
        let response = handle_request(
            Request::ListSweeps {
                state_filter: None,
                workspace_root: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::SweepList { sweeps } => {
                assert_eq!(sweeps.len(), 1);
                assert!(
                    sweeps[0].state.is_terminal(),
                    "reap-on-read should have transitioned the exited fake child \
                     out of Running (#3893); got {:?}",
                    sweeps[0].state
                );
            }
            other => panic!("Expected SweepList, got: {other:?}"),
        }
    }

    /// Issue #3967: reproduce the reported daemon-dispatched sweep self-skip at
    /// the **IPC dispatch-path level** — through `handle_request` itself, not
    /// `SweepRegistry::dispatch()` called directly (the existing
    /// `dispatch_exports_claim_ownership_marker` unit test in
    /// `sweep_registry.rs` covers that narrower scope). `handle_request`'s
    /// `Request::DispatchSweep` arm is the exact server-side code both the
    /// `loom-daemon dispatch <issue>` operator CLI (#3952) and the MCP
    /// `dispatch_sweep` tool round-trip into over the Unix socket — so a
    /// regression here would have caught the incident regardless of which of
    /// those two client surfaces initiated the request. Asserts the spawned
    /// child's env carries `LOOM_SWEEP_CLAIM_OWNED=<issue>` end-to-end, AND
    /// (#4111) that its argv carries the equivalent `--claim-owned <issue>`
    /// flag — the positional signal `/loom:sweep`'s pre-flight actually reads.
    #[test]
    #[serial_test::serial]
    fn test_handle_request_dispatch_sweep_exports_claim_ownership_marker() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, record_log) = setup_sweep_registry_in_tempdir();

        let response = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(3964),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                workspace_root: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        assert!(
            matches!(response, Response::SweepDispatched { .. }),
            "expected SweepDispatched, got: {response:?}"
        );

        // The fake spawn-claude.sh exits immediately; give it a brief window
        // to flush its record log rather than racing the write.
        let start = std::time::Instant::now();
        let mut recorded = String::new();
        while start.elapsed().as_millis() < 5000 {
            if let Ok(s) = std::fs::read_to_string(&record_log) {
                if s.contains("LOOM_SWEEP_CLAIM_OWNED=") {
                    recorded = s;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            recorded.contains("LOOM_SWEEP_CLAIM_OWNED=3964"),
            "expected the daemon-owned-child self-claim marker to reach the \
             spawned child via the IPC DispatchSweep handler; got: {recorded:?}"
        );
        // #4111: the positional argv flag must also reach the child via this
        // same IPC path.
        assert!(
            recorded.contains("--claim-owned 3964"),
            "expected --claim-owned 3964 in the spawned child's argv via the IPC \
             DispatchSweep handler (#4111); got: {recorded:?}"
        );
    }

    #[test]
    fn test_handle_request_dispatch_sweep_rejects_prset_in_phase_a() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();

        let response = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::PrSet(vec![100, 200]),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                workspace_root: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::Error { message } => {
                assert!(
                    message.contains("PrSet"),
                    "expected PrSet rejection message; got: {message}"
                );
            }
            other => panic!("Expected Error, got: {other:?}"),
        }
    }

    // ===== DispatchSweep serde compat (Issue #3477, Phase 1) =====

    /// A wire payload WITHOUT the `model` field (the pre-#3477 client shape)
    /// must deserialize with `model == None` — `#[serde(default)]` keeps
    /// existing clients compatible.
    #[test]
    fn test_dispatch_sweep_deserializes_without_model_field() {
        let json = r#"{"type":"DispatchSweep","payload":{"kind":{"type":"Issue","value":42},"idempotency_key":null}}"#;
        let request: Request = serde_json::from_str(json).expect("pre-#3477 payload must parse");
        match request {
            Request::DispatchSweep {
                kind,
                idempotency_key,
                model,
                effort,
                depends_on: _,
                workspace_root: _,
            } => {
                assert!(matches!(kind, SweepKind::Issue(42)));
                assert!(idempotency_key.is_none());
                assert!(model.is_none(), "absent model field must default to None");
                assert!(effort.is_none(), "absent effort field must default to None");
            }
            other => panic!("Expected DispatchSweep, got: {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_sweep_serde_round_trip_with_model() {
        let request = Request::DispatchSweep {
            kind: SweepKind::Issue(7),
            idempotency_key: Some("key-B".to_string()),
            model: Some("claude-sonnet-4-6".to_string()),
            effort: None,
            depends_on: None,
            workspace_root: None,
        };
        let json = serde_json::to_string(&request).expect("serialize");
        let back: Request = serde_json::from_str(&json).expect("deserialize");
        match back {
            Request::DispatchSweep {
                kind,
                idempotency_key,
                model,
                effort,
                depends_on: _,
                workspace_root: _,
            } => {
                assert!(matches!(kind, SweepKind::Issue(7)));
                assert_eq!(idempotency_key.as_deref(), Some("key-B"));
                assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
                assert!(effort.is_none());
            }
            other => panic!("Expected DispatchSweep, got: {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_sweep_serde_round_trip_without_model() {
        let request = Request::DispatchSweep {
            kind: SweepKind::Issue(8),
            idempotency_key: None,
            model: None,
            effort: None,
            depends_on: None,
            workspace_root: None,
        };
        let json = serde_json::to_string(&request).expect("serialize");
        let back: Request = serde_json::from_str(&json).expect("deserialize");
        match back {
            Request::DispatchSweep { model, .. } => assert!(model.is_none()),
            other => panic!("Expected DispatchSweep, got: {other:?}"),
        }
    }

    // ===== DispatchSweep serde compat for `effort` (Issue #3716) =====

    /// A wire payload WITHOUT the `effort` field (the pre-#3716 client shape)
    /// must deserialize with `effort == None` — `#[serde(default)]` keeps
    /// existing clients compatible.
    #[test]
    fn test_dispatch_sweep_deserializes_without_effort_field() {
        let json = r#"{"type":"DispatchSweep","payload":{"kind":{"type":"Issue","value":42},"idempotency_key":null,"model":"claude-sonnet-4-6"}}"#;
        let request: Request = serde_json::from_str(json).expect("pre-#3716 payload must parse");
        match request {
            Request::DispatchSweep { model, effort, .. } => {
                assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
                assert!(effort.is_none(), "absent effort field must default to None");
            }
            other => panic!("Expected DispatchSweep, got: {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_sweep_serde_round_trip_with_effort() {
        let request = Request::DispatchSweep {
            kind: SweepKind::Issue(9),
            idempotency_key: Some("key-E".to_string()),
            model: Some("claude-sonnet-4-6".to_string()),
            effort: Some("xhigh".to_string()),
            depends_on: None,
            workspace_root: None,
        };
        let json = serde_json::to_string(&request).expect("serialize");
        let back: Request = serde_json::from_str(&json).expect("deserialize");
        match back {
            Request::DispatchSweep { model, effort, .. } => {
                assert_eq!(model.as_deref(), Some("claude-sonnet-4-6"));
                assert_eq!(effort.as_deref(), Some("xhigh"));
            }
            other => panic!("Expected DispatchSweep, got: {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_sweep_serde_round_trip_with_empty_effort() {
        let request = Request::DispatchSweep {
            kind: SweepKind::Issue(10),
            idempotency_key: None,
            model: None,
            effort: Some(String::new()),
            depends_on: None,
            workspace_root: None,
        };
        let json = serde_json::to_string(&request).expect("serialize");
        let back: Request = serde_json::from_str(&json).expect("deserialize");
        match back {
            // Empty string round-trips as-is at the wire layer; normalization
            // to None happens spawn-side (registry) exactly like `model`.
            Request::DispatchSweep { effort, .. } => {
                assert_eq!(effort.as_deref(), Some(""));
            }
            other => panic!("Expected DispatchSweep, got: {other:?}"),
        }
    }

    // ===== DispatchSweep serde compat for `depends_on` (Issue #3729) =====

    /// A wire payload WITHOUT the `depends_on` field (the pre-#3729 client
    /// shape) must deserialize with `depends_on == None` — `#[serde(default)]`
    /// keeps existing clients compatible.
    #[test]
    fn test_dispatch_sweep_deserializes_without_depends_on_field() {
        let json = r#"{"type":"DispatchSweep","payload":{"kind":{"type":"Issue","value":42},"idempotency_key":null,"model":"claude-sonnet-4-6","effort":"xhigh"}}"#;
        let request: Request = serde_json::from_str(json).expect("pre-#3729 payload must parse");
        match request {
            Request::DispatchSweep { depends_on, .. } => {
                assert!(depends_on.is_none(), "absent depends_on must default to None");
            }
            other => panic!("Expected DispatchSweep, got: {other:?}"),
        }
    }

    #[test]
    fn test_dispatch_sweep_serde_round_trip_with_depends_on() {
        let request = Request::DispatchSweep {
            kind: SweepKind::Issue(3725),
            idempotency_key: None,
            model: None,
            effort: None,
            depends_on: Some(3726),
            workspace_root: None,
        };
        let json = serde_json::to_string(&request).expect("serialize");
        let back: Request = serde_json::from_str(&json).expect("deserialize");
        match back {
            Request::DispatchSweep { depends_on, .. } => {
                assert_eq!(depends_on, Some(3726));
            }
            other => panic!("Expected DispatchSweep, got: {other:?}"),
        }
    }

    // ===== Event bus IPC handlers (Issue #3453, Phase B) =====

    #[tokio::test]
    async fn test_handle_request_publish_event_routes_to_subscribers() {
        let (tm, db, sr, bus) = setup_test_context();
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        let response = handle_request(
            Request::PublishEvent {
                topic: "sweep.issue.123.phase".to_string(),
                payload: serde_json::json!({"phase": "builder"}),
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );

        match response {
            Response::EventPublished { topic, receivers } => {
                assert_eq!(topic, "sweep.issue.123.phase");
                assert!(receivers >= 1, "expected at least 1 receiver; got {receivers}");
            }
            other => panic!("Expected EventPublished, got: {other:?}"),
        }

        // Subscriber should now see the published event.
        let ev = sub.recv().await.unwrap();
        match ev {
            Event::Generic { topic, payload } => {
                assert_eq!(topic, "sweep.issue.123.phase");
                assert_eq!(payload, serde_json::json!({"phase": "builder"}));
            }
            other => panic!("Expected Generic event, got: {other:?}"),
        }
    }

    // ===== Sweep monitoring IPC handlers (Issue #3455, Phase C) =====

    #[test]
    fn test_handle_request_get_sweep_status_missing() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
        let response = handle_request(
            Request::GetSweepStatus {
                sweep_id: "no-such-sweep".to_string(),
                workspace_root: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::SweepStatus { info } => assert!(info.is_none()),
            other => panic!("Expected SweepStatus, got: {other:?}"),
        }
    }

    #[test]
    fn test_handle_request_tail_sweep_log_missing_sweep_returns_error() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
        let response = handle_request(
            Request::TailSweepLog {
                sweep_id: "no-such-sweep".to_string(),
                lines: 10,
                workspace_root: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::Error { message } => {
                assert!(
                    message.contains("unknown sweep_id"),
                    "expected unknown sweep_id; got: {message}"
                );
            }
            other => panic!("Expected Error, got: {other:?}"),
        }
    }

    #[test]
    fn test_handle_request_clear_quarantine_noop() {
        // Issue #3939/#3960: clearing an issue that is not quarantined is an
        // idempotent no-op success routed through the full IPC dispatcher.
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
        let response = handle_request(
            Request::ClearQuarantine {
                issue: 4242,
                workspace_root: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::QuarantineCleared {
                issue,
                was_quarantined,
            } => {
                assert_eq!(issue, 4242);
                assert!(!was_quarantined, "no entry existed -> false");
            }
            other => panic!("Expected QuarantineCleared, got: {other:?}"),
        }
    }

    #[test]
    fn test_handle_request_clear_quarantine_clears_existing() {
        // Seed a quarantine directly, then clear it via the IPC dispatcher and
        // assert the in-memory state was released (was_quarantined: true).
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
        {
            let mut reg = sr.lock().unwrap();
            reg.seed_quarantine_for_test(808);
            assert!(reg.is_quarantined(808));
        }
        let response = handle_request(
            Request::ClearQuarantine {
                issue: 808,
                workspace_root: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::QuarantineCleared {
                issue,
                was_quarantined,
            } => {
                assert_eq!(issue, 808);
                assert!(was_quarantined, "seeded entry existed -> true");
            }
            other => panic!("Expected QuarantineCleared, got: {other:?}"),
        }
        assert!(!sr.lock().unwrap().is_quarantined(808));
    }

    #[test]
    fn test_handle_request_cancel_sweep_unknown_returns_error() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
        let response = handle_request(
            Request::CancelSweep {
                sweep_id: "no-such-sweep".to_string(),
                grace_secs: 1,
                workspace_root: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::Error { message } => {
                assert!(
                    message.contains("unknown sweep_id"),
                    "expected unknown sweep_id; got: {message}"
                );
            }
            other => panic!("Expected Error, got: {other:?}"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_handle_request_get_sweep_status_returns_existing() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();

        // Dispatch a sweep to get a real entry in the registry.
        let dispatched = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(444),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                workspace_root: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        let sweep_id = match dispatched {
            Response::SweepDispatched { sweep_id, .. } => sweep_id,
            other => panic!("Expected SweepDispatched, got: {other:?}"),
        };

        let response = handle_request(
            Request::GetSweepStatus {
                sweep_id: sweep_id.clone(),
                workspace_root: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::SweepStatus { info } => {
                let info = info.expect("status should be Some");
                assert_eq!(info.sweep_id, sweep_id);
                assert!(matches!(info.kind, SweepKind::Issue(444)));
            }
            other => panic!("Expected SweepStatus, got: {other:?}"),
        }
    }

    // ===== Singleton guard liveness probe (Issue #3806) =====

    #[tokio::test]
    async fn test_socket_has_live_listener_absent_path() {
        // A path that doesn't exist at all → not live.
        let dir = tempdir().unwrap();
        let missing = dir.path().join("nope.sock");
        assert!(!socket_has_live_listener(&missing).await);
    }

    #[tokio::test]
    async fn test_socket_has_live_listener_stale_file() {
        // A regular file at the socket path (a crashed daemon's leftover) has
        // nothing listening behind it → not live, safe to remove/rebind.
        let dir = tempdir().unwrap();
        let stale = dir.path().join("stale.sock");
        std::fs::write(&stale, b"").unwrap();
        assert!(!socket_has_live_listener(&stale).await);
    }

    #[tokio::test]
    async fn test_socket_has_live_listener_non_daemon_listener() {
        // A bound UnixListener that never answers Ping (no accept/respond loop)
        // must be treated as NOT a live, responsive daemon so startup can still
        // recover rather than wedging forever.
        let dir = tempdir().unwrap();
        let sock = dir.path().join("silent.sock");
        let _listener = UnixListener::bind(&sock).unwrap();
        // We never accept()/respond, so the Ping/Pong roundtrip times out.
        assert!(!socket_has_live_listener(&sock).await);
    }

    #[tokio::test]
    async fn test_socket_has_live_listener_true_for_ponging_daemon() {
        // Stand up a minimal accept loop that answers Ping with Pong, exactly
        // like the real IPC server, and confirm the probe reports it live.
        let dir = tempdir().unwrap();
        let sock = dir.path().join("live.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        let server = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                if let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(Request::Ping) = serde_json::from_str::<Request>(&line) {
                        let json = serde_json::to_string(&Response::Pong).unwrap();
                        let _ = writer.write_all(json.as_bytes()).await;
                        let _ = writer.write_all(b"\n").await;
                        let _ = writer.flush().await;
                    }
                }
            }
        });

        assert!(socket_has_live_listener(&sock).await);
        server.abort();
    }

    // ===== Autonomous daemon status (Issue #3891) =====

    /// `Request::DaemonStatus` / `Response::DaemonStatus` must survive a serde
    /// round-trip over the wire (pattern: the existing Ping/Pong probe + the
    /// dispatch serde round-trips).
    #[test]
    fn test_daemon_status_request_response_round_trip() {
        // Request: unit variant, `{"type":"DaemonStatus"}`.
        let req = Request::DaemonStatus;
        let json = serde_json::to_string(&req).expect("serialize request");
        assert_eq!(json, r#"{"type":"DaemonStatus"}"#);
        let back: Request = serde_json::from_str(&json).expect("deserialize request");
        assert!(matches!(back, Request::DaemonStatus));

        // Response: carries the full report.
        let report = DaemonStatusReport {
            in_flight: vec![],
            token_pool_size: 4,
            disk_headroom: 10,
            cpu_headroom: 6,
            logical_cpus: 8,
            loadavg_1m: Some(1.25),
            cpu_idle_fraction: Some(0.90),
            capacity_bound: false,
            configured_max: 5,
            per_token_concurrency: 2,
            dynamic_cap: 3,
            main_health_gate_halted: true,
            main_health_gate_not_evaluated: false,
            main_health_gate_not_evaluated_reason: None,
            main_health_gate_enabled: Some(true),
            main_health_gate_verdict_at: Some(chrono::Utc::now()),
            capacity: crate::types::CapacityReport {
                ranking_present: true,
                total_accounts: 4,
                healthy_accounts: 3,
                exhausted_accounts: 1,
                token_axis_limit: 3,
                token_bound: true,
            },
            per_repo: vec![crate::types::RepoStatus {
                root: std::path::PathBuf::from("/repo/a"),
                priority: 100,
                in_flight_count: 0,
                health_gate_halted: true,
                quarantined_issues: vec![101, 202],
                health_gate_not_evaluated: false,
                health_gate_not_evaluated_reason: None,
                health_gate_enabled: Some(true),
                health_gate_verdict_at: Some(chrono::Utc::now()),
            }],
            credential_preflight: Some(test_credential_preflight()),
            draining: false,
            drain_deadline: None,
            drain_note: None,
        };
        let resp = Response::DaemonStatus(report);
        let json = serde_json::to_string(&resp).expect("serialize response");
        let back: Response = serde_json::from_str(&json).expect("deserialize response");
        match back {
            Response::DaemonStatus(r) => {
                assert_eq!(r.token_pool_size, 4);
                assert_eq!(r.disk_headroom, 10);
                assert_eq!(r.cpu_headroom, 6);
                assert_eq!(r.logical_cpus, 8);
                assert_eq!(r.loadavg_1m, Some(1.25));
                assert_eq!(r.cpu_idle_fraction, Some(0.90));
                assert!(!r.capacity_bound);
                assert_eq!(r.configured_max, 5);
                assert_eq!(r.dynamic_cap, 3);
                assert!(r.main_health_gate_halted);
                assert!(!r.main_health_gate_not_evaluated);
                assert!(r.in_flight.is_empty());
                assert!(r.capacity.ranking_present);
                assert_eq!(r.capacity.healthy_accounts, 3);
                assert_eq!(r.capacity.exhausted_accounts, 1);
                assert_eq!(r.capacity.token_axis_limit, 3);
                assert!(r.capacity.token_bound);
                assert_eq!(r.per_repo.len(), 1);
                assert_eq!(r.per_repo[0].in_flight_count, 0);
                assert!(r.per_repo[0].health_gate_halted);
                assert!(!r.per_repo[0].health_gate_not_evaluated);
                assert_eq!(r.main_health_gate_enabled, Some(true));
                assert!(r.main_health_gate_verdict_at.is_some());
                assert_eq!(r.per_repo[0].health_gate_enabled, Some(true));
                assert!(r.per_repo[0].health_gate_verdict_at.is_some());
                assert_eq!(
                    r.credential_preflight
                        .as_ref()
                        .map(|c| c.mechanism.as_str()),
                    Some("test-fixture")
                );
            }
            other => panic!("Expected DaemonStatus, got: {other:?}"),
        }
    }

    // ===== Supervised restart primitive (Issue #4054) =====

    /// `Request::RestartDaemon` / `Response::DaemonRestart` must survive a serde
    /// round-trip over the wire (same pattern as the Ping/Pong + DaemonStatus
    /// round-trips above).
    #[test]
    fn test_restart_daemon_request_response_round_trip() {
        // Request: unit variant, `{"type":"RestartDaemon"}`.
        let req = Request::RestartDaemon;
        let json = serde_json::to_string(&req).expect("serialize request");
        assert_eq!(json, r#"{"type":"RestartDaemon"}"#);
        let back: Request = serde_json::from_str(&json).expect("deserialize request");
        assert!(matches!(back, Request::RestartDaemon));

        // Response (supervised / scheduled).
        let resp = Response::DaemonRestart {
            scheduled: true,
            supervisor: Some("launchd".to_string()),
            message: "restart scheduled".to_string(),
        };
        let json = serde_json::to_string(&resp).expect("serialize response");
        let back: Response = serde_json::from_str(&json).expect("deserialize response");
        match back {
            Response::DaemonRestart {
                scheduled,
                supervisor,
                message,
            } => {
                assert!(scheduled);
                assert_eq!(supervisor.as_deref(), Some("launchd"));
                assert_eq!(message, "restart scheduled");
            }
            other => panic!("Expected DaemonRestart, got: {other:?}"),
        }

        // Response (unsupervised / refused).
        let resp = Response::DaemonRestart {
            scheduled: false,
            supervisor: None,
            message: "refused".to_string(),
        };
        let json = serde_json::to_string(&resp).expect("serialize response");
        let back: Response = serde_json::from_str(&json).expect("deserialize response");
        match back {
            Response::DaemonRestart {
                scheduled,
                supervisor,
                ..
            } => {
                assert!(!scheduled);
                assert!(supervisor.is_none());
            }
            other => panic!("Expected DaemonRestart, got: {other:?}"),
        }
    }

    /// `build_restart_decision` ends the process (do_exit == true) ONLY when the
    /// daemon proves it is launchd-supervised via `LOOM_DAEMON_SUPERVISOR`; an
    /// unsupervised host refuses and stays running. Also pins the shutdown-intent
    /// exit-code contract (#4054): only the restart primitive exits 0, so under
    /// launchd `KeepAlive:SuccessfulExit` it is the only path that relaunches.
    ///
    /// NOTE: this is the sole test touching `LOOM_DAEMON_SUPERVISOR`, so the
    /// env-var mutation cannot race another test reading it.
    #[test]
    fn test_build_restart_decision_supervisor_gated() {
        // Exit-code contract: exactly one exit-0 path.
        assert_eq!(EXIT_RESTART, 0, "restart is the only successful (relaunch) exit");
        assert_ne!(EXIT_SIGTERM, 0, "SIGTERM stop must be non-zero (no relaunch)");
        assert_ne!(EXIT_SIGINT, 0, "SIGINT/Ctrl-C must be non-zero (no relaunch)");
        assert_ne!(EXIT_SHUTDOWN, 0, "explicit Shutdown must be non-zero (no relaunch)");

        // Supervised: scheduled + do_exit.
        std::env::set_var("LOOM_DAEMON_SUPERVISOR", "launchd");
        assert_eq!(detect_supervisor().as_deref(), Some("launchd"));
        let (resp, do_exit) = build_restart_decision();
        assert!(do_exit, "supervised daemon must end its process for a relaunch");
        match resp {
            Response::DaemonRestart {
                scheduled,
                supervisor,
                ..
            } => {
                assert!(scheduled);
                assert_eq!(supervisor.as_deref(), Some("launchd"));
            }
            other => panic!("Expected DaemonRestart, got: {other:?}"),
        }

        // Case-insensitive acceptance.
        std::env::set_var("LOOM_DAEMON_SUPERVISOR", "LaunchD");
        assert_eq!(detect_supervisor().as_deref(), Some("launchd"));

        // Unsupervised (var unset): refuse, keep running.
        std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
        assert!(detect_supervisor().is_none());
        let (resp, do_exit) = build_restart_decision();
        assert!(!do_exit, "unsupervised daemon must NOT exit — nothing would relaunch it");
        match resp {
            Response::DaemonRestart {
                scheduled,
                supervisor,
                ..
            } => {
                assert!(!scheduled);
                assert!(supervisor.is_none());
            }
            other => panic!("Expected DaemonRestart, got: {other:?}"),
        }

        // An unrelated value is also unsupervised.
        std::env::set_var("LOOM_DAEMON_SUPERVISOR", "systemd");
        assert!(detect_supervisor().is_none());
        std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
    }

    /// A pre-#3902 `DaemonStatus` JSON payload (no `capacity` field, no
    /// `per_repo` field) still deserializes — `#[serde(default)]` fills the
    /// capacity section and leaves `per_repo` empty (pre-#3930 compat).
    #[test]
    fn test_daemon_status_backward_compat_missing_capacity() {
        let legacy = r#"{"in_flight":[],"token_pool_size":2,"disk_headroom":9,"configured_max":3,"dynamic_cap":2,"main_health_gate_halted":false}"#;
        let report: DaemonStatusReport =
            serde_json::from_str(legacy).expect("legacy payload deserializes");
        assert_eq!(report.token_pool_size, 2);
        assert!(!report.capacity.ranking_present);
        assert_eq!(report.capacity.healthy_accounts, 0);
        assert!(!report.capacity.token_bound);
        assert!(report.per_repo.is_empty(), "absent per_repo defaults to empty");
        assert!(
            !report.main_health_gate_not_evaluated,
            "absent main_health_gate_not_evaluated (#3950) defaults to false"
        );
        // Absent pre-#3978 fields default rather than failing to parse.
        assert_eq!(report.cpu_headroom, 0);
        assert_eq!(report.logical_cpus, 0);
        assert_eq!(report.loadavg_1m, None);
        // Absent pre-#4012 fields must default to `None` — NOT `false` — so a
        // legacy payload from an older, perfectly healthy daemon is read as
        // "unknown" rather than misreported as "gate disabled" (#4012).
        assert_eq!(
            report.main_health_gate_enabled, None,
            "absent main_health_gate_enabled (#4012) must default to None, not Some(false)"
        );
        assert_eq!(
            report.main_health_gate_verdict_at, None,
            "absent main_health_gate_verdict_at (#4012) defaults to None (reads as pending)"
        );
        // Absent pre-#4031 fields default rather than failing to parse: no
        // measured idle fraction, and "not capacity-bound".
        assert_eq!(report.cpu_idle_fraction, None);
        assert!(!report.capacity_bound);
    }

    /// `build_daemon_status` sets `capacity_bound` only when in-flight occupancy
    /// has actually reached the dynamic cap (#4031) — the "currently binding" vs
    /// "smallest ceiling" distinction. With a real token pool (cap > 0) and no
    /// in-flight sweeps, the cap is a ceiling but is NOT binding.
    #[test]
    #[serial_test::serial]
    fn test_build_daemon_status_capacity_bound_tracks_occupancy() {
        use crate::main_health_gate::WorkspaceHealthStates;
        use crate::workspace_registry::REGISTRY_PATH_ENV;

        let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
        let root = dir.path().to_path_buf();
        let empty_reg = dir.path().join("no-such-workspaces.json");
        std::env::set_var(REGISTRY_PATH_ENV, &empty_reg);
        let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
        std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");

        // Provision a two-token pool so the dynamic cap is > 0 (a real ceiling).
        let tokens_dir = root.join(".loom").join("tokens");
        std::fs::create_dir_all(&tokens_dir).unwrap();
        std::fs::write(tokens_dir.join("acct-a.token"), "sk-ant-oat01-a").unwrap();
        std::fs::write(tokens_dir.join("acct-b.token"), "sk-ant-oat01-b").unwrap();

        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(root.clone(), sr.clone());
        let health = WorkspaceHealthStates::new();

        // No sweeps in flight, cap > 0 ⇒ the cap is a ceiling but NOT binding.
        let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
        assert!(report.dynamic_cap > 0, "two tokens should yield a positive cap");
        assert!(report.in_flight.is_empty());
        assert!(
            !report.capacity_bound,
            "0 in-flight against cap {} must not be capacity-bound",
            report.dynamic_cap
        );

        // Fill the cap: dispatch sweeps until in-flight reaches the cap.
        let cap = report.dynamic_cap;
        {
            let mut reg = sr.lock().unwrap();
            for i in 0..cap {
                reg.dispatch(
                    &crate::types::SweepKind::Issue(4031 + i as u32),
                    None,
                    None,
                    None,
                    None,
                )
                .expect("dispatch");
            }
        }
        let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
        assert_eq!(report.in_flight.len(), cap);
        assert!(
            report.capacity_bound,
            "{cap} in-flight against cap {cap} must be capacity-bound",
        );

        if let Some(v) = prev_shared {
            std::env::set_var("LOOM_SHARED_TOKENS_DIR", v);
        } else {
            std::env::remove_var("LOOM_SHARED_TOKENS_DIR");
        }
    }

    /// `build_daemon_status` reflects the per-repo main-health halt flag and
    /// lists a live dispatched sweep as in-flight. Single-workspace case (empty
    /// registry): exactly one `per_repo` entry for the daemon's own workspace,
    /// byte-for-byte the pre-#3930 top-level behavior.
    #[test]
    #[serial_test::serial]
    fn test_build_daemon_status_reports_halt_and_in_flight() {
        use crate::main_health_gate::WorkspaceHealthStates;
        use crate::workspace_registry::REGISTRY_PATH_ENV;

        let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
        let root = dir.path().to_path_buf();

        // Point the workspace registry at a nonexistent file so effective_roots
        // falls back to [root] (single-workspace equivalence).
        let empty_reg = dir.path().join("no-such-workspaces.json");
        std::env::set_var(REGISTRY_PATH_ENV, &empty_reg);

        // Disable the shared machine-level pool fallback (#3940) so the
        // token-pool assertions see only the tempdir workspace, not the
        // host's real ~/.loom/tokens (empty value = operator opt-out).
        let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
        std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");

        // Seed the pool with the default registry keyed at `root`.
        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(root.clone(), sr.clone());
        let health = WorkspaceHealthStates::new();

        // Fresh state: not halted, no sweeps.
        let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
        assert!(!report.main_health_gate_halted);
        assert!(report.in_flight.is_empty());
        // The tempdir has no `.loom/tokens/`, so the pool + dynamic cap are 0.
        assert_eq!(report.token_pool_size, 0);
        assert_eq!(report.dynamic_cap, 0);
        // Per-repo breakdown: exactly one entry for the single workspace.
        assert_eq!(report.per_repo.len(), 1);
        assert_eq!(report.per_repo[0].root, root);
        assert_eq!(report.per_repo[0].in_flight_count, 0);
        assert!(!report.per_repo[0].health_gate_halted);
        // No `.loom/config.json` buildGate/autonomous block exists yet, so the
        // gate is effectively disabled for this root (#4012).
        assert_eq!(report.main_health_gate_enabled, Some(false));
        assert_eq!(report.per_repo[0].health_gate_enabled, Some(false));
        assert_eq!(report.main_health_gate_verdict_at, None);
        assert_eq!(report.per_repo[0].health_gate_verdict_at, None);

        // #4012: a root the gate loop HAS never evaluated (no verdict yet)
        // reports the same `Some(false)`/`None` pair while genuinely disabled
        // -- but once the config turns the gate on, a fresh `MainHealthState`
        // reports "enabled, pending" (verdict_at still `None`), NOT "clear".
        // This is the exact ambiguity the issue is about: `pending` and
        // `disabled` both still allow dispatch, but they must not be
        // confused with `clear` (verified green).
        std::env::remove_var(crate::main_health_gate::MAIN_HEALTH_GATE_ENABLE_ENV);
        std::fs::write(
            root.join(".loom").join("config.json"),
            r#"{"autonomous": {"mainHealthGate": {"enabled": true}}, "buildGate": {"enabled": true, "command": "true"}}"#,
        )
        .unwrap();
        let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
        assert_eq!(
            report.main_health_gate_enabled,
            Some(true),
            "config now enables the gate for this root"
        );
        assert_eq!(
            report.main_health_gate_verdict_at, None,
            "no gate run has completed yet -- must report pending, not clear"
        );
        assert!(!report.main_health_gate_halted, "pending must still read as dispatch-allowed");

        // Dispatch a sweep -> it should show up as in-flight (Running).
        {
            let mut reg = sr.lock().unwrap();
            reg.dispatch(&crate::types::SweepKind::Issue(3891), None, None, None, None)
                .expect("dispatch");
        }
        let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
        assert_eq!(report.in_flight.len(), 1);
        assert!(matches!(report.in_flight[0].kind, crate::types::SweepKind::Issue(3891)));
        assert_eq!(report.per_repo[0].in_flight_count, 1);

        // Flip the halt flag for this root -> the report tracks it (top-level and
        // per-repo).
        health.set_halted(&root, true);
        let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
        assert!(report.main_health_gate_halted);
        assert!(report.per_repo[0].health_gate_halted);
        assert!(!report.main_health_gate_not_evaluated, "no skip has happened yet");

        // A skip (dirty tree) is independent of halt (#3950 AC3): it leaves any
        // prior halt untouched but surfaces its own "not evaluated" flag, so
        // "halted (red main)" and "not evaluated (dirty tree)" can both be
        // true — a prior red run's halt persisting while a later tick can't
        // even evaluate because the tree went dirty.
        health.get_or_create(&root).note_gate_tick(
            Some((
                crate::main_health_gate::UnevaluatedClass::DirtyTree,
                "operator edit in src/main.rs",
            )),
            std::time::Duration::from_secs(3600),
        );
        let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
        assert!(report.main_health_gate_halted, "prior halt persists through a skip");
        assert!(report.main_health_gate_not_evaluated, "skip surfaces as not-evaluated");
        assert!(report.per_repo[0].health_gate_halted);
        assert!(report.per_repo[0].health_gate_not_evaluated);
        // #3974 AC2: the report names the actual failure class + detail rather
        // than leaving the renderer to assume "workspace tree is dirty".
        let reason = report
            .main_health_gate_not_evaluated_reason
            .as_deref()
            .expect("not-evaluated reason recorded");
        assert!(reason.starts_with("dirty-tree: "), "got: {reason}");
        assert!(reason.contains("src/main.rs"), "got: {reason}");
        assert_eq!(
            report.per_repo[0]
                .health_gate_not_evaluated_reason
                .as_deref(),
            Some(reason)
        );

        match prev_shared {
            Some(v) => std::env::set_var("LOOM_SHARED_TOKENS_DIR", v),
            None => std::env::remove_var("LOOM_SHARED_TOKENS_DIR"),
        }
        std::env::remove_var(REGISTRY_PATH_ENV);
    }

    /// `build_daemon_status` with two registered workspaces returns one `per_repo`
    /// entry per root with correct in-flight counts (a sweep dispatched into a
    /// non-default repo is now visible) and independent per-repo halt state
    /// (Issue #3930 — a red repo B does not mark repo A halted).
    #[test]
    #[serial_test::serial]
    fn test_build_daemon_status_multi_workspace_per_repo_breakdown() {
        use crate::main_health_gate::WorkspaceHealthStates;
        use crate::workspace_registry::{normalize_path, WorkspaceRegistry, REGISTRY_PATH_ENV};

        let (sr_a, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
        let (sr_b, dir_b, _rec_b) = setup_sweep_registry_in_tempdir();
        let root_a = dir_a.path().to_path_buf();
        let root_b = dir_b.path().to_path_buf();

        // A registry listing BOTH managed repos (roots stored canonicalized).
        let reg_path = dir_a.path().join("workspaces.json");
        let mut reg = WorkspaceRegistry::default();
        reg.add(&root_a, None).unwrap();
        reg.add(&root_b, None).unwrap();
        reg.save(&reg_path).unwrap();
        std::env::set_var(REGISTRY_PATH_ENV, &reg_path);

        // Seed the pool with both registries under their normalized (canonical)
        // roots — the same key `effective_roots` returns.
        let canon_a = normalize_path(&root_a);
        let canon_b = normalize_path(&root_b);
        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(canon_a.clone(), sr_a.clone());
        pool.seed(canon_b.clone(), sr_b.clone());

        let health = WorkspaceHealthStates::new();
        // Repo B is red; repo A green — independently.
        health.set_halted(&canon_b, true);

        // Dispatch a sweep into repo A only.
        {
            let mut reg = sr_a.lock().unwrap();
            reg.dispatch(&crate::types::SweepKind::Issue(42), None, None, None, None)
                .expect("dispatch");
        }

        let report = build_daemon_status(&pool, &health, &root_a, &test_credential_preflight());
        assert_eq!(report.per_repo.len(), 2, "both managed repos are listed");
        // Union of in-flight across repos = repo A's single sweep.
        assert_eq!(report.in_flight.len(), 1);

        let a = report
            .per_repo
            .iter()
            .find(|r| r.root == canon_a)
            .expect("repo A present");
        let b = report
            .per_repo
            .iter()
            .find(|r| r.root == canon_b)
            .expect("repo B present");
        assert_eq!(a.in_flight_count, 1, "repo A has the dispatched sweep");
        assert!(!a.health_gate_halted, "repo A is green");
        assert_eq!(b.in_flight_count, 0, "repo B has no sweeps");
        assert!(b.health_gate_halted, "repo B is red, independently of A");
        // Neither repo has a `.loom/config.json` buildGate block, so both
        // resolve as effectively disabled (#4012) — independent of the raw
        // halt flag test-injected directly on repo B above (`set_halted`
        // bypasses the gate loop's own disabled soft-fail path, so this
        // combination only arises in a test; the renderer must still prefer
        // "halted" over "disabled" when both are true, see `main.rs`).
        assert_eq!(a.health_gate_enabled, Some(false));
        assert_eq!(b.health_gate_enabled, Some(false));
        assert_eq!(a.health_gate_verdict_at, None);
        assert_eq!(b.health_gate_verdict_at, None);

        std::env::remove_var(REGISTRY_PATH_ENV);
    }

    /// If `DaemonStatus` ever reaches the synchronous dispatcher (it is meant to
    /// be intercepted in `handle_client`), it returns a loud Error sentinel.
    #[test]
    fn test_handle_request_daemon_status_short_circuits_to_error() {
        let (tm, db, sr, bus) = setup_test_context();
        let response = handle_request(Request::DaemonStatus, &tm, &db, &sr, &bus, &test_pool());
        match response {
            Response::Error { message } => {
                assert!(
                    message.contains("DaemonStatus must be handled by build_daemon_status"),
                    "expected internal-bug error message; got: {message}"
                );
            }
            other => panic!("Expected Error sentinel, got: {other:?}"),
        }
    }

    #[test]
    fn test_handle_request_subscribe_events_short_circuits_to_error() {
        // SubscribeEvents must be handled by stream_events (not the
        // dispatcher). If it ever reaches handle_request, the dispatcher
        // returns an Error sentinel so the bug is visible.
        let (tm, db, sr, bus) = setup_test_context();
        let response = handle_request(
            Request::SubscribeEvents {
                topics: vec!["sweep".to_string()],
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::Error { message } => {
                assert!(
                    message.contains("SubscribeEvents must be handled by stream_events"),
                    "expected internal-bug error message; got: {message}"
                );
            }
            other => panic!("Expected Error sentinel, got: {other:?}"),
        }
    }

    // ===== Workspace Registry (Issue #3926) =====

    /// End-to-end exercise of the Register / List / Deregister IPC handlers
    /// against a temp registry file (via `LOOM_WORKSPACES_PATH`). Serialized
    /// because it mutates the process env that resolves the registry path.
    #[test]
    #[serial_test::serial]
    fn test_workspace_registry_ipc_roundtrip() {
        let (tm, db, sr, bus) = setup_test_context();
        let dir = tempdir().unwrap();
        let registry_path = dir.path().join("workspaces.json");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let canonical = std::fs::canonicalize(&repo).unwrap();

        std::env::set_var("LOOM_WORKSPACES_PATH", &registry_path);

        // Empty registry: list returns no workspaces.
        let response = handle_request(Request::ListWorkspaces, &tm, &db, &sr, &bus, &test_pool());
        match response {
            Response::WorkspaceList { workspaces } => assert!(workspaces.is_empty()),
            other => panic!("Expected WorkspaceList, got: {other:?}"),
        }

        // Register.
        let response = handle_request(
            Request::RegisterWorkspace {
                root: repo.to_string_lossy().into_owned(),
                config_overrides: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::WorkspaceRegistered {
                root,
                already_present,
                ..
            } => {
                assert_eq!(root, canonical);
                assert!(!already_present);
            }
            other => panic!("Expected WorkspaceRegistered, got: {other:?}"),
        }

        // Re-register is idempotent (already_present = true).
        let response = handle_request(
            Request::RegisterWorkspace {
                root: repo.to_string_lossy().into_owned(),
                config_overrides: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::WorkspaceRegistered {
                already_present, ..
            } => assert!(already_present),
            other => panic!("Expected WorkspaceRegistered, got: {other:?}"),
        }

        // List now shows exactly one.
        let response = handle_request(Request::ListWorkspaces, &tm, &db, &sr, &bus, &test_pool());
        match response {
            Response::WorkspaceList { workspaces } => {
                assert_eq!(workspaces.len(), 1);
                assert_eq!(workspaces[0].root, canonical);
            }
            other => panic!("Expected WorkspaceList, got: {other:?}"),
        }

        // Deregister.
        let response = handle_request(
            Request::DeregisterWorkspace {
                root: repo.to_string_lossy().into_owned(),
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::WorkspaceDeregistered { was_present, .. } => assert!(was_present),
            other => panic!("Expected WorkspaceDeregistered, got: {other:?}"),
        }

        // Deregister again is a no-op success.
        let response = handle_request(
            Request::DeregisterWorkspace {
                root: repo.to_string_lossy().into_owned(),
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::WorkspaceDeregistered { was_present, .. } => assert!(!was_present),
            other => panic!("Expected WorkspaceDeregistered, got: {other:?}"),
        }

        std::env::remove_var("LOOM_WORKSPACES_PATH");
    }

    #[test]
    #[serial_test::serial]
    fn test_watch_registry_ipc_roundtrip() {
        use crate::watch_registry::WatchKind;

        let (tm, db, sr, bus) = setup_test_context();
        let dir = tempdir().unwrap();
        let watches_path = dir.path().join("watches.json");
        std::env::set_var(crate::watch_registry::WATCHES_PATH_ENV, &watches_path);

        // Empty registry: list returns none.
        let response = handle_request(Request::ListWatches, &tm, &db, &sr, &bus, &test_pool());
        match response {
            Response::WatchList { watches } => assert!(watches.is_empty()),
            other => panic!("Expected WatchList, got: {other:?}"),
        }

        // Register a cross-repo issue watch (the motivating #6193 case).
        let response = handle_request(
            Request::RegisterWatch {
                kind: WatchKind::Issue,
                number: 6193,
                repo: Some("rjwalters/vibesql".to_string()),
                workspace_root: None,
                note: Some("canary".to_string()),
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        let watch_id = match response {
            Response::WatchRegistered {
                watch,
                already_present,
            } => {
                assert!(!already_present);
                assert_eq!(watch.number, 6193);
                assert_eq!(watch.repo.as_deref(), Some("rjwalters/vibesql"));
                watch.id
            }
            other => panic!("Expected WatchRegistered, got: {other:?}"),
        };

        // Re-register the same target dedups (already_present = true).
        let response = handle_request(
            Request::RegisterWatch {
                kind: WatchKind::Issue,
                number: 6193,
                repo: Some("rjwalters/vibesql".to_string()),
                workspace_root: None,
                note: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::WatchRegistered {
                already_present, ..
            } => assert!(already_present),
            other => panic!("Expected WatchRegistered, got: {other:?}"),
        }

        // List shows exactly one, and it survives being re-loaded from disk
        // (the whole point — a watch outlives the registering session).
        let response = handle_request(Request::ListWatches, &tm, &db, &sr, &bus, &test_pool());
        match response {
            Response::WatchList { watches } => {
                assert_eq!(watches.len(), 1);
                assert_eq!(watches[0].id, watch_id);
            }
            other => panic!("Expected WatchList, got: {other:?}"),
        }

        // Remove by id.
        let response = handle_request(
            Request::RemoveWatch {
                id: watch_id.clone(),
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::WatchRemoved { was_present, .. } => assert!(was_present),
            other => panic!("Expected WatchRemoved, got: {other:?}"),
        }

        // Removing again is a no-op success.
        let response = handle_request(
            Request::RemoveWatch { id: watch_id },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::WatchRemoved { was_present, .. } => assert!(!was_present),
            other => panic!("Expected WatchRemoved, got: {other:?}"),
        }

        std::env::remove_var(crate::watch_registry::WATCHES_PATH_ENV);
    }

    // ===== Scheduled drain-and-restart (Issue #4090) =====

    /// The pure drain-decision function (AC2 / AC3): zero in-flight always
    /// completes (restart), even at/after the deadline; a passed deadline with
    /// sweeps still in flight refuses (fail-safe) or forces per the flag; before
    /// the deadline it keeps waiting.
    #[test]
    fn test_evaluate_drain_tick_decisions() {
        // Still in flight, deadline not reached ⇒ keep waiting.
        assert_eq!(evaluate_drain_tick(2, false, false), DrainTick::Continue);
        assert_eq!(evaluate_drain_tick(1, false, true), DrainTick::Continue);
        // Zero in-flight ⇒ complete regardless of deadline/force.
        assert_eq!(evaluate_drain_tick(0, false, false), DrainTick::Complete);
        assert_eq!(evaluate_drain_tick(0, true, false), DrainTick::Complete);
        assert_eq!(evaluate_drain_tick(0, true, true), DrainTick::Complete);
        // Deadline passed with work left: refuse (fail-safe) vs. force.
        assert_eq!(evaluate_drain_tick(3, true, false), DrainTick::TimedOutRefuse);
        assert_eq!(evaluate_drain_tick(3, true, true), DrainTick::TimedOutForce);
    }

    /// The "2 → 1 → 0" completion sequence (AC2): a supervisor stepping through
    /// a decreasing in-flight count keeps waiting until it hits exactly zero,
    /// then completes exactly once. Driven through the pure decision function so
    /// no process actually exits.
    #[test]
    fn test_drain_tick_completes_only_at_zero() {
        let mut completed = 0;
        for n in [2usize, 1, 0] {
            match evaluate_drain_tick(n, false, false) {
                DrainTick::Continue => assert!(n > 0, "must still be waiting while n>0"),
                DrainTick::Complete => {
                    assert_eq!(n, 0, "must only complete at zero");
                    completed += 1;
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
        assert_eq!(completed, 1, "completes exactly once, at n==0");
    }

    /// The `DrainState` machine: begin sets the flag and a deadline, a second
    /// begin is idempotent (does not stack / move the deadline), abort clears
    /// the flag and bumps the generation, and the timeout path clears + notes.
    #[test]
    fn test_drain_state_lifecycle() {
        let drain = DrainState::new();
        assert!(!drain.is_draining());
        assert_eq!(drain.generation(), 0);

        // begin ⇒ Started, flag set, deadline recorded, generation bumped.
        let (gen1, deadline) = match drain.begin(Duration::from_secs(120), false) {
            DrainBegin::Started {
                generation,
                deadline,
            } => (generation, deadline),
            other => panic!("expected Started, got {other:?}"),
        };
        assert!(drain.is_draining());
        assert_eq!(gen1, 1);
        assert_eq!(drain.snapshot().deadline, Some(deadline));
        assert!(!drain.snapshot().force_after_timeout);

        // A second begin while already draining is idempotent: same generation,
        // same deadline, flag still set (AC edge: second drain does not stack).
        match drain.begin(Duration::from_secs(9999), true) {
            DrainBegin::AlreadyDraining => {}
            other => panic!("expected AlreadyDraining, got {other:?}"),
        }
        assert_eq!(drain.generation(), gen1, "idempotent begin does not bump gen");
        assert_eq!(
            drain.snapshot().deadline,
            Some(deadline),
            "idempotent begin does not move the deadline"
        );

        // abort ⇒ flag cleared, generation bumped (so a live supervisor stops),
        // note recorded.
        assert!(drain.abort());
        assert!(!drain.is_draining());
        assert_eq!(drain.generation(), gen1 + 1);
        assert!(drain.snapshot().note.unwrap().contains("aborted"));
        // abort again ⇒ no-op.
        assert!(!drain.abort());

        // timeout resolution clears + notes + bumps generation.
        let gen_before = drain.generation();
        let _ = drain.begin(Duration::from_secs(1), false);
        drain.resolve_timeout("timed out".to_string());
        assert!(!drain.is_draining());
        assert_eq!(drain.snapshot().note.as_deref(), Some("timed out"));
        assert!(drain.generation() > gen_before);
    }

    /// AC5 (immediate unsupervised refusal): with no supervisor, a drain request
    /// is refused with `accepted: false` **and the drain flag is never set** —
    /// pausing dispatch then refusing would be a silent outage.
    ///
    /// NOTE: shares the `LOOM_DAEMON_SUPERVISOR` env with
    /// `test_build_restart_decision_supervisor_gated`; `#[serial]` keeps them
    /// from racing the process-global env var.
    #[test]
    #[serial_test::serial]
    fn test_drain_request_unsupervised_refuses_without_pausing() {
        std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
        let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
        let root = dir.path().to_path_buf();
        let empty_reg = dir.path().join("no-such-workspaces.json");
        std::env::set_var(crate::workspace_registry::REGISTRY_PATH_ENV, &empty_reg);
        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(root.clone(), sr);
        let bus = Arc::new(EventBus::new());
        let drain = Arc::new(DrainState::new());

        let resp = handle_drain_request(&drain, &pool, &root, &bus, Some(60), false);
        match resp {
            Response::DaemonDrain {
                accepted,
                supervisor,
                ..
            } => {
                assert!(!accepted, "unsupervised host must refuse the drain");
                assert!(supervisor.is_none());
            }
            other => panic!("expected DaemonDrain, got {other:?}"),
        }
        assert!(!drain.is_draining(), "refused drain must NOT pause dispatch");
        assert_eq!(drain.generation(), 0, "refused drain must not bump generation");

        std::env::remove_var(crate::workspace_registry::REGISTRY_PATH_ENV);
    }

    /// Cross-root in-flight counting (Finding 5): sweeps live in the SECONDARY
    /// managed repo only must still be counted, so a drain that reads them does
    /// not restart while that repo has live work. Also asserts terminal sweeps
    /// are excluded.
    #[test]
    #[serial_test::serial]
    fn test_count_in_flight_sweeps_cross_root() {
        use crate::workspace_registry::{normalize_path, WorkspaceRegistry, REGISTRY_PATH_ENV};

        let (sr_a, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
        let (sr_b, dir_b, _rec_b) = setup_sweep_registry_in_tempdir();
        let root_a = dir_a.path().to_path_buf();
        let root_b = dir_b.path().to_path_buf();

        let reg_path = dir_a.path().join("workspaces.json");
        let mut reg = WorkspaceRegistry::default();
        reg.add(&root_a, None).unwrap();
        reg.add(&root_b, None).unwrap();
        reg.save(&reg_path).unwrap();
        std::env::set_var(REGISTRY_PATH_ENV, &reg_path);

        let canon_a = normalize_path(&root_a);
        let canon_b = normalize_path(&root_b);
        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(canon_a.clone(), sr_a);
        pool.seed(canon_b.clone(), sr_b.clone());

        // No sweeps anywhere ⇒ zero.
        assert_eq!(count_in_flight_sweeps(&pool, &canon_a), 0);

        // Dispatch a live sweep into the SECONDARY repo only.
        {
            let mut reg_b = sr_b.lock().unwrap();
            reg_b
                .dispatch(&crate::types::SweepKind::Issue(4090), None, None, None, None)
                .expect("dispatch");
        }
        // Counted even though the primary (root_a) registry is empty — a drain
        // reading only the primary would wrongly see zero and restart.
        assert_eq!(
            count_in_flight_sweeps(&pool, &canon_a),
            1,
            "a live sweep in the secondary repo must be counted"
        );

        std::env::remove_var(REGISTRY_PATH_ENV);
    }

    /// A pre-#4090 `DaemonStatus` JSON payload (no `drain` fields) still
    /// deserializes — `#[serde(default)]` fills `draining: false` and leaves the
    /// deadline/note `None` (mirrors the `capacity_bound` compat rationale).
    #[test]
    fn test_daemon_status_backward_compat_missing_drain_fields() {
        let legacy = r#"{"in_flight":[],"token_pool_size":2,"disk_headroom":9,"configured_max":3,"dynamic_cap":2,"main_health_gate_halted":false}"#;
        let report: DaemonStatusReport =
            serde_json::from_str(legacy).expect("legacy payload deserializes");
        assert!(!report.draining, "absent draining (#4090) defaults to false");
        assert_eq!(report.drain_deadline, None);
        assert_eq!(report.drain_note, None);
    }

    /// The new drain fields round-trip through serde, and
    /// `build_daemon_status_with_drain` overlays the live drain state onto the
    /// base report (AC4).
    #[test]
    #[serial_test::serial]
    fn test_build_daemon_status_with_drain_overlays_state() {
        let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
        let root = dir.path().to_path_buf();
        let empty_reg = dir.path().join("no-such-workspaces.json");
        std::env::set_var(crate::workspace_registry::REGISTRY_PATH_ENV, &empty_reg);
        let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
        std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");

        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(root.clone(), sr);
        let health = WorkspaceHealthStates::new();
        let drain = DrainState::new();

        // No drain ⇒ overlay is a no-op.
        let report = build_daemon_status_with_drain(
            &pool,
            &health,
            &root,
            &test_credential_preflight(),
            &drain,
        );
        assert!(!report.draining);
        assert_eq!(report.drain_deadline, None);

        // Begin a drain ⇒ overlay reports draining + deadline.
        let deadline = match drain.begin(Duration::from_secs(300), false) {
            DrainBegin::Started { deadline, .. } => deadline,
            other => panic!("expected Started, got {other:?}"),
        };
        let report = build_daemon_status_with_drain(
            &pool,
            &health,
            &root,
            &test_credential_preflight(),
            &drain,
        );
        assert!(report.draining);
        assert_eq!(report.drain_deadline, Some(deadline));

        // Round-trips over the wire.
        let json = serde_json::to_string(&report).unwrap();
        let back: DaemonStatusReport = serde_json::from_str(&json).unwrap();
        assert!(back.draining);
        assert_eq!(back.drain_deadline, Some(deadline));

        match prev_shared {
            Some(v) => std::env::set_var("LOOM_SHARED_TOKENS_DIR", v),
            None => std::env::remove_var("LOOM_SHARED_TOKENS_DIR"),
        }
        std::env::remove_var(crate::workspace_registry::REGISTRY_PATH_ENV);
    }

    /// Wire-compat (Finding 3): the new `DrainAndRestartDaemon` variant
    /// round-trips, and the untouched `RestartDaemon` unit variant STILL
    /// serializes to exactly `{"type":"RestartDaemon"}`. The
    /// `test_restart_daemon_request_response_round_trip` assertion above must
    /// also keep passing unmodified.
    #[test]
    fn test_drain_request_wire_compat() {
        // RestartDaemon is unchanged — byte-for-byte the pre-#4090 shape.
        assert_eq!(
            serde_json::to_string(&Request::RestartDaemon).unwrap(),
            r#"{"type":"RestartDaemon"}"#
        );

        // The new variant round-trips with its payload.
        let req = Request::DrainAndRestartDaemon {
            timeout_secs: Some(600),
            force_after_timeout: true,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        match back {
            Request::DrainAndRestartDaemon {
                timeout_secs,
                force_after_timeout,
            } => {
                assert_eq!(timeout_secs, Some(600));
                assert!(force_after_timeout);
            }
            other => panic!("expected DrainAndRestartDaemon, got {other:?}"),
        }

        // AbortDrain round-trips (unit-with-payload-none shape).
        let json = serde_json::to_string(&Request::AbortDrain).unwrap();
        assert_eq!(json, r#"{"type":"AbortDrain"}"#);
        let back: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Request::AbortDrain));

        // The DaemonDrain response round-trips.
        let resp = Response::DaemonDrain {
            accepted: true,
            supervisor: Some("launchd".to_string()),
            in_flight: 3,
            message: "draining".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();
        match back {
            Response::DaemonDrain {
                accepted,
                supervisor,
                in_flight,
                ..
            } => {
                assert!(accepted);
                assert_eq!(supervisor.as_deref(), Some("launchd"));
                assert_eq!(in_flight, 3);
            }
            other => panic!("expected DaemonDrain, got {other:?}"),
        }
    }

    /// Abort clears the flag and bumps the generation, so a supervisor holding
    /// the old generation stops without exiting — even if in-flight later
    /// reaches zero (AC6, the "abort then the queue empties anyway" race). We
    /// assert the generation contract the async supervisor relies on rather than
    /// spawning it (its Complete branch calls `process::exit`).
    #[test]
    fn test_abort_supersedes_running_supervisor_generation() {
        let drain = DrainState::new();
        let gen = match drain.begin(Duration::from_secs(300), false) {
            DrainBegin::Started { generation, .. } => generation,
            other => panic!("expected Started, got {other:?}"),
        };
        // A supervisor captured `gen`; abort moves the generation on.
        assert!(drain.abort());
        assert_ne!(
            drain.generation(),
            gen,
            "abort must bump the generation so the running supervisor detects supersession"
        );
        assert!(!drain.is_draining(), "abort resumes dispatch");
    }
}
