//! Daemon-owned pid file (`<state home>/.daemon.pid`) — issue #4774.
//!
//! # Why the daemon must own this write
//!
//! Before #4774 the pid file was written in exactly one place:
//! `loom-daemon-start.sh`, at *provisioning* time, from the shell that spawned
//! the daemon (three write sites — launchd, systemd, bare-nohup). Every
//! **supervisor-triggered relaunch** bypasses that script entirely:
//!
//! - launchd `KeepAlive:{SuccessfulExit:true}` respawning the job after the
//!   `RestartDaemon` primitive (#4054) exits 0,
//! - `systemd --user` `Restart=on-success` doing the same,
//! - the in-daemon self-update loop (`loom-daemon-update.sh --no-restart` +
//!   the restart primitive),
//! - the #4232 `launchctl kickstart` path.
//!
//! All of them produce a **new daemon pid** while the pid file keeps naming
//! the *old* one. Observed 2026-07-31: `.loom/.daemon.pid` held `13724` while
//! the live daemon was `99917` — two relaunches stale. That poisons every
//! liveness cross-check that consults it: `daemon_install_state`'s #4694
//! pid-file fallback, the [`crate::health`] collector's liveness section, the
//! `loom:watch` fallback battery, and manual operator debugging. On that same
//! morning it compounded with a name-matched `pgrep` hitting a `/tmp` test
//! stub — both quick liveness primitives lied at once.
//!
//! The fix is the same shape as #4331's marker healing: heal at **one startup
//! choke point** that every relaunch path necessarily passes through. Here that
//! point is [`crate::ipc::IpcServer::run`], immediately after
//! `UnixListener::bind` succeeds — the exact instant this process is the
//! confirmed *sole* owner of the daemon socket.
//!
//! # Why after the bind, not before the singleton guard
//!
//! #4331's marker healing runs in `daemon_service.rs` *before* the singleton
//! guard is evaluated. Copying that call site verbatim would be a regression
//! here: a daemon that is about to be **refused** (a live incumbent already
//! answers on the socket) would first stomp the incumbent's legitimate pid file
//! with its own doomed pid, then bail — turning the guard's "refuse, don't
//! disturb the incumbent" contract into a data corruption. Writing after a
//! successful `bind` makes that structurally impossible: a refused daemon never
//! reaches the write.
//!
//! # Path resolution
//!
//! Mirrors `loom-daemon-start.sh`'s `PID_FILE="$DAEMON_STATE_HOME/.daemon.pid"`,
//! in precedence order:
//!
//! 1. **`LOOM_PID_FILE`** — an explicit path. `loom-daemon-start.sh` exports
//!    this before rendering the launchd plist / systemd unit, so the path the
//!    start script chose is baked into the supervisor definition and every
//!    relaunch resolves the *identical* file. This is the production path.
//! 2. **Machine mode** (`LOOM_MACHINE_CHECKOUT` set, Epic #3835 Phase 3b):
//!    `<loom_dir>/.daemon.pid`, where `<loom_dir>` is
//!    [`crate::autonomy_marker::resolve_loom_dir`] (`LOOM_SOCKET_PATH`'s parent,
//!    else `~/.loom`) — matching the start script's `DAEMON_STATE_HOME="$HOME/.loom"`
//!    while still honoring a tempdir socket for test isolation.
//! 3. **Repo mode** (`LOOM_WORKSPACE` set): `<workspace>/.loom/.daemon.pid`,
//!    matching `DAEMON_STATE_HOME="$REPO_ROOT/.loom"`.
//! 4. Otherwise `<loom_dir>/.daemon.pid`.
//!
//! Tiers 2-4 exist for daemons whose supervisor definition predates the
//! `LOOM_PID_FILE` export (an already-provisioned plist keeps its render-time
//! env until the next `loom-daemon-start.sh` run) and for direct/dev launches.
//!
//! # Never fatal
//!
//! Every failure mode here — unresolvable path, unwritable directory, a rename
//! that loses a race — is logged and ignored. A daemon without a pid file is
//! strictly better than no daemon, exactly as in [`crate::autonomy_marker`].

use std::path::{Path, PathBuf};

pub use crate::autonomy_marker::PID_FILENAME;

/// Env override naming the pid file outright. Exported by
/// `loom-daemon-start.sh` so the start script and the daemon can never disagree
/// about where the file lives (see the module docs' precedence list).
pub const PID_FILE_ENV: &str = "LOOM_PID_FILE";

/// Resolve the pid file path from the process environment. `None` only when no
/// loom dir can be resolved at all (no `LOOM_SOCKET_PATH`, no home directory)
/// and no more specific override applies.
#[must_use]
pub fn resolve_pid_file_path() -> Option<PathBuf> {
    resolve_pid_file_path_from(
        std::env::var(PID_FILE_ENV).ok(),
        std::env::var("LOOM_MACHINE_CHECKOUT").ok(),
        std::env::var("LOOM_WORKSPACE").ok(),
        crate::autonomy_marker::resolve_loom_dir(),
    )
}

/// [`resolve_pid_file_path`]'s pure core: the precedence rules with every env
/// lookup lifted into arguments, so tests drive each tier without mutating
/// process-global env vars (which are shared across the whole test binary).
#[must_use]
pub fn resolve_pid_file_path_from(
    pid_file_env: Option<String>,
    machine_checkout: Option<String>,
    workspace: Option<String>,
    loom_dir: Option<PathBuf>,
) -> Option<PathBuf> {
    let non_empty = |s: Option<String>| s.filter(|v| !v.is_empty());

    // 1. Explicit override — the production path (start script exports it).
    if let Some(path) = non_empty(pid_file_env) {
        return Some(PathBuf::from(path));
    }
    // 2. Machine mode: state lives under the machine-level loom dir.
    if non_empty(machine_checkout).is_some() {
        return loom_dir.map(|d| d.join(PID_FILENAME));
    }
    // 3. Repo mode: state lives under `<repo>/.loom`.
    if let Some(ws) = non_empty(workspace) {
        return Some(PathBuf::from(ws).join(".loom").join(PID_FILENAME));
    }
    // 4. Fall back to the machine-level loom dir.
    loom_dir.map(|d| d.join(PID_FILENAME))
}

/// The outcome of the startup pid-file claim — one variant per branch so the
/// caller logs precisely and tests assert on the decision, not the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// The pid file now names this process. Carries the path and the pid that
    /// was previously recorded there (`None` ⇒ absent/unparseable), so the
    /// caller can log a *stale-file corrected* event rather than a silent
    /// overwrite.
    Claimed {
        path: PathBuf,
        previous: Option<u32>,
    },
    /// No pid file path could be resolved at all (no `LOOM_PID_FILE`, no
    /// workspace, no socket path, no home directory).
    Unresolvable,
    /// The write failed. Non-fatal — logged, and the daemon keeps running.
    WriteFailed { path: PathBuf, error: String },
}

/// Claim the pid file for the *current* process. Call this once, at the single
/// startup choke point every supervised relaunch passes through (after the
/// daemon socket bind succeeds — see the module docs).
#[must_use]
pub fn claim_for_current_process() -> ClaimOutcome {
    match resolve_pid_file_path() {
        Some(path) => claim_at(&path, std::process::id()),
        None => ClaimOutcome::Unresolvable,
    }
}

/// [`claim_for_current_process`]'s side-effect-scoped core: write `pid` into
/// `path`, reporting whatever pid the file named beforehand.
#[must_use]
pub fn claim_at(path: &Path, pid: u32) -> ClaimOutcome {
    let previous = read_pid_file(path);
    match write_pid_atomic(path, pid) {
        Ok(()) => ClaimOutcome::Claimed {
            path: path.to_path_buf(),
            previous,
        },
        Err(error) => ClaimOutcome::WriteFailed {
            path: path.to_path_buf(),
            error,
        },
    }
}

/// Write `<pid>\n` via a temp file + atomic rename (the same convention the
/// autonomy marker and the sweep checkpoints use), so a concurrent reader — the
/// watchdog, `status`, `health` — never observes a truncated or empty file.
///
/// Mode `0o644`: the start script writes with the invoking shell's default
/// umask, and unlike the autonomy marker (which records paths and is written
/// under `umask 077`) a pid file carries nothing sensitive.
fn write_pid_atomic(path: &Path, pid: u32) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return Err(format!("could not create {}: {e}", parent.display()));
        }
    }
    let tmp = path.with_extension(format!("pid.tmp.{}", uuid::Uuid::new_v4()));
    let contents = format!("{pid}\n");

    let write_result = {
        #[cfg(unix)]
        {
            use std::io::Write as _;
            use std::os::unix::fs::OpenOptionsExt as _;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o644)
                .open(&tmp)
                .and_then(|mut f| f.write_all(contents.as_bytes()))
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&tmp, contents.as_bytes())
        }
    };
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("could not write temp pid file {}: {e}", tmp.display()));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("could not rename pid file into place {}: {e}", path.display()));
    }
    Ok(())
}

/// Parse a pid file into its recorded pid — `None` when the file is missing,
/// unreadable, or does not hold a bare integer. Says nothing about whether that
/// pid is alive.
#[must_use]
pub fn read_pid_file(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

// ============================================================================
// Shared-state ownership guard (#9816)
// ============================================================================

/// Env override acknowledging a deliberate second daemon on shared real state.
pub const ALLOW_PARALLEL_DAEMON_ENV: &str = "LOOM_ALLOW_PARALLEL_DAEMON";

/// How long a refusal is re-evaluated before it is final, so a supervisor
/// relaunch (old daemon still exiting) does not flap.
pub const OWNERSHIP_RECHECK_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);

/// Interval between re-reads inside [`OWNERSHIP_RECHECK_WINDOW`].
pub const OWNERSHIP_RECHECK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

/// Verdict of [`decide_ownership`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnershipDecision {
    /// No live incumbent holds this pid file (absent, stale, own pid, scratch).
    Proceed,
    /// A live incumbent exists but `LOOM_ALLOW_PARALLEL_DAEMON=1` acknowledges it.
    ProceedWithWarn { incumbent: u32 },
    /// A live loom-daemon already owns this (non-scratch) pid file.
    Refuse { incumbent: u32 },
}

/// Pure rule: refuse iff a different, live loom-daemon pid is recorded in a
/// non-scratch pid file and the override is not set. Every input is injected.
#[must_use]
pub fn decide_ownership(
    recorded_pid: Option<u32>,
    own_pid: u32,
    alive: bool,
    is_daemon: bool,
    is_scratch: bool,
    allow_parallel: bool,
) -> OwnershipDecision {
    if is_scratch {
        return OwnershipDecision::Proceed;
    }
    let Some(incumbent) = recorded_pid.filter(|p| *p != own_pid) else {
        return OwnershipDecision::Proceed;
    };
    if !alive || !is_daemon {
        return OwnershipDecision::Proceed;
    }
    if allow_parallel {
        OwnershipDecision::ProceedWithWarn { incumbent }
    } else {
        OwnershipDecision::Refuse { incumbent }
    }
}

/// Check the pid file at `path` against the shared-state ownership rule,
/// re-reading for up to `window` before a refusal is final. Read-only and
/// fail-open: nothing here writes, and an unreadable file proceeds.
pub async fn check_ownership_at(
    path: &Path,
    own_pid: u32,
    is_scratch: bool,
    allow_parallel: bool,
    alive: impl Fn(u32) -> bool,
    is_daemon: impl Fn(u32) -> bool,
    window: std::time::Duration,
) -> OwnershipDecision {
    let deadline = std::time::Instant::now() + window;
    loop {
        let recorded = read_pid_file(path);
        // Only probe the process table when the cheap checks already point at
        // an incumbent (`pgrep` is a subprocess).
        let candidate = recorded.filter(|p| *p != own_pid && !is_scratch);
        let (a, d) = match candidate {
            Some(p) => {
                let a = alive(p);
                (a, a && is_daemon(p))
            }
            None => (false, false),
        };
        let decision = decide_ownership(recorded, own_pid, a, d, is_scratch, allow_parallel);
        if !matches!(decision, OwnershipDecision::Refuse { .. })
            || std::time::Instant::now() >= deadline
        {
            return decision;
        }
        tokio::time::sleep(OWNERSHIP_RECHECK_INTERVAL).await;
    }
}

/// [`check_ownership_at`] against the process environment and real probes.
/// `None` when no pid file path resolves (fail-open).
pub async fn check_ownership_for_current_process() -> Option<(PathBuf, OwnershipDecision)> {
    let path = resolve_pid_file_path()?;
    let is_scratch = crate::daemon_start::guards::is_scratch_style_path(&path.to_string_lossy());
    let allow_parallel = std::env::var(ALLOW_PARALLEL_DAEMON_ENV).is_ok_and(|v| v == "1");
    let daemons: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
    let decision = check_ownership_at(
        &path,
        std::process::id(),
        is_scratch,
        allow_parallel,
        crate::daemon_install_state::pid_alive,
        |p| {
            daemons
                .get_or_init(crate::daemon_install_state::pgrep_daemon_pids)
                .contains(&p)
        },
        OWNERSHIP_RECHECK_WINDOW,
    )
    .await;
    Some((path, decision))
}

// ============================================================================
// Stale detection (AC3)
// ============================================================================

/// A single host-local observation of the pid file: what is on disk, and
/// whether the pid it names is currently a live process. Collected by the CLI
/// (`status` / `health`) and handed to the pure [`classify`] rule below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PidFileObservation {
    /// The path observed.
    pub path: PathBuf,
    /// Whether a file exists at [`Self::path`] at observation time.
    pub present: bool,
    /// The pid it names, when present and parseable as a bare integer.
    pub recorded_pid: Option<u32>,
    /// Whether [`Self::recorded_pid`] is a live process (`false` when there is
    /// no recorded pid at all).
    pub recorded_pid_alive: bool,
}

/// Observe the pid file at `path` using the real `kill -0` liveness probe.
#[must_use]
pub fn observe(path: &Path) -> PidFileObservation {
    observe_with(path, crate::daemon_install_state::pid_alive)
}

/// [`observe`]'s injectable variant — tests pin liveness instead of depending
/// on which pids happen to exist on the host running them.
#[must_use]
pub fn observe_with(path: &Path, alive: impl Fn(u32) -> bool) -> PidFileObservation {
    let present = path.exists();
    let recorded_pid = read_pid_file(path);
    PidFileObservation {
        path: path.to_path_buf(),
        present,
        recorded_pid_alive: recorded_pid.is_some_and(alive),
        recorded_pid,
    }
}

/// The verdict on a pid file, cross-checked against the pid of the process that
/// actually answered on the daemon socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PidFileState {
    /// No pid file at the resolved path.
    Absent,
    /// A file exists but holds no parseable pid.
    Unparseable,
    /// The recorded pid names the process that answered IPC. Healthy.
    Matches(u32),
    /// The recorded pid is alive but no socket-owner pid was available to
    /// cross-check against (an unreachable daemon, or one older than #4774's
    /// `daemon_pid` status field). Not an anomaly — just unconfirmed.
    Unverified(u32),
    /// The recorded pid is **not a live process**. Stale.
    Dead(u32),
    /// The recorded pid is alive but is a *different* process than the one
    /// answering on the socket — the exact 2026-07-31 signature (`13724` on
    /// disk, `99917` answering). Stale.
    Mismatch { recorded: u32, socket_owner: u32 },
}

impl PidFileState {
    /// Whether this verdict means the file must not be trusted as a liveness
    /// signal. [`PidFileState::Absent`] is deliberately **not** stale: an
    /// absent file makes no false claim, and a daemon launched outside the
    /// managed start path legitimately has none.
    #[must_use]
    pub fn is_stale(&self) -> bool {
        matches!(
            self,
            PidFileState::Unparseable | PidFileState::Dead(_) | PidFileState::Mismatch { .. }
        )
    }

    /// Machine-readable verdict for `--json` consumers.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            PidFileState::Absent => "absent",
            PidFileState::Unparseable => "unparseable",
            PidFileState::Matches(_) => "matches",
            PidFileState::Unverified(_) => "unverified",
            PidFileState::Dead(_) => "dead",
            PidFileState::Mismatch { .. } => "mismatch",
        }
    }

    /// The operator-facing note for a *stale* verdict — `None` for the
    /// non-anomalous ones, so callers can `if let Some(note) = …` and stay
    /// silent when there is nothing wrong.
    #[must_use]
    pub fn note(&self, path: &Path) -> Option<String> {
        match self {
            PidFileState::Absent | PidFileState::Matches(_) | PidFileState::Unverified(_) => None,
            PidFileState::Unparseable => Some(format!(
                "STALE pid file {}: present but holds no parseable pid — do not trust it as a \
                 liveness signal (#4774)",
                path.display()
            )),
            PidFileState::Dead(pid) => Some(format!(
                "STALE pid file {}: records pid {pid}, which is not a live process — do not trust \
                 it as a liveness signal (#4774)",
                path.display()
            )),
            PidFileState::Mismatch {
                recorded,
                socket_owner,
            } => Some(format!(
                "STALE pid file {}: records pid {recorded}, but the daemon answering the socket is \
                 pid {socket_owner} — a supervisor relaunch left it behind (#4774); re-run \
                 `loom-daemon-start.sh` or roll the daemon to a build that self-writes it",
                path.display()
            )),
        }
    }
}

/// The pure cross-check rule: an observation plus the pid of whoever answered
/// the daemon socket (`None` when unreachable / pre-#4774 daemon).
#[must_use]
pub fn classify(obs: &PidFileObservation, socket_owner_pid: Option<u32>) -> PidFileState {
    let Some(recorded) = obs.recorded_pid else {
        return if obs.present {
            PidFileState::Unparseable
        } else {
            PidFileState::Absent
        };
    };
    match socket_owner_pid {
        Some(owner) if owner == recorded => PidFileState::Matches(recorded),
        Some(owner) => PidFileState::Mismatch {
            recorded,
            socket_owner: owner,
        },
        // No socket owner to compare against: aliveness is the only signal
        // left. A dead recorded pid is still positive evidence of staleness.
        None if obs.recorded_pid_alive => PidFileState::Unverified(recorded),
        None => PidFileState::Dead(recorded),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;

    // ---------------- path resolution ----------------

    #[test]
    fn explicit_env_override_wins_over_every_other_tier() {
        let resolved = resolve_pid_file_path_from(
            Some("/explicit/loom.pid".to_string()),
            Some("/machine/checkout".to_string()),
            Some("/repo".to_string()),
            Some(PathBuf::from("/home/.loom")),
        );
        assert_eq!(resolved, Some(PathBuf::from("/explicit/loom.pid")));
    }

    #[test]
    fn empty_env_values_are_ignored_not_honored() {
        // An exported-but-empty `LOOM_PID_FILE` must not resolve to `""`.
        let resolved = resolve_pid_file_path_from(
            Some(String::new()),
            Some(String::new()),
            Some("/repo".to_string()),
            Some(PathBuf::from("/home/.loom")),
        );
        assert_eq!(resolved, Some(PathBuf::from("/repo/.loom/.daemon.pid")));
    }

    #[test]
    fn machine_mode_resolves_under_the_machine_loom_dir() {
        // Machine mode keeps runtime state in the machine-level loom dir even
        // though a repo workspace is also set (start script's
        // `DAEMON_STATE_HOME="$HOME/.loom"`).
        let resolved = resolve_pid_file_path_from(
            None,
            Some("/machine/checkout".to_string()),
            Some("/repo".to_string()),
            Some(PathBuf::from("/home/.loom")),
        );
        assert_eq!(resolved, Some(PathBuf::from("/home/.loom/.daemon.pid")));
    }

    #[test]
    fn repo_mode_resolves_under_the_workspace_loom_dir() {
        let resolved = resolve_pid_file_path_from(
            None,
            None,
            Some("/repo".to_string()),
            Some(PathBuf::from("/home/.loom")),
        );
        assert_eq!(resolved, Some(PathBuf::from("/repo/.loom/.daemon.pid")));
    }

    #[test]
    fn falls_back_to_the_loom_dir_when_nothing_else_is_set() {
        let resolved =
            resolve_pid_file_path_from(None, None, None, Some(PathBuf::from("/home/.loom")));
        assert_eq!(resolved, Some(PathBuf::from("/home/.loom/.daemon.pid")));
    }

    #[test]
    fn unresolvable_when_no_loom_dir_and_no_overrides() {
        assert_eq!(resolve_pid_file_path_from(None, None, None, None), None);
    }

    // ---------------- claiming ----------------

    #[test]
    fn claim_writes_the_pid_and_reports_no_previous_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PID_FILENAME);
        let outcome = claim_at(&path, 4242);
        assert_eq!(
            outcome,
            ClaimOutcome::Claimed {
                path: path.clone(),
                previous: None
            }
        );
        assert_eq!(fs::read_to_string(&path).unwrap().trim(), "4242");
    }

    #[test]
    fn claim_overwrites_a_stale_pid_and_reports_the_previous_one() {
        // The core #4774 scenario: a supervisor relaunch finds the prior
        // process's pid on disk and must replace it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PID_FILENAME);
        fs::write(&path, "13724\n").unwrap();
        let outcome = claim_at(&path, 99917);
        assert_eq!(
            outcome,
            ClaimOutcome::Claimed {
                path: path.clone(),
                previous: Some(13724)
            }
        );
        assert_eq!(fs::read_to_string(&path).unwrap().trim(), "99917");
    }

    #[test]
    fn claim_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(".loom").join(PID_FILENAME);
        assert!(matches!(claim_at(&path, 7), ClaimOutcome::Claimed { .. }));
        assert_eq!(read_pid_file(&path), Some(7));
    }

    #[test]
    fn claim_leaves_no_temp_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PID_FILENAME);
        assert!(matches!(claim_at(&path, 11), ClaimOutcome::Claimed { .. }));
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != PID_FILENAME)
            .collect();
        assert!(leftovers.is_empty(), "atomic write leaked: {leftovers:?}");
    }

    #[test]
    fn write_failure_is_reported_not_panicked() {
        // A path whose parent cannot be created (a *file* stands where the
        // directory would go) exercises the non-fatal error branch.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, "not a directory").unwrap();
        let path = blocker.join(PID_FILENAME);
        assert!(matches!(claim_at(&path, 5), ClaimOutcome::WriteFailed { .. }));
    }

    // ---------------- shared-state ownership (#9816) ----------------

    const OWN: u32 = 100;
    const OTHER: u32 = 200;

    #[test]
    fn live_foreign_daemon_on_real_state_is_refused() {
        assert_eq!(
            decide_ownership(Some(OTHER), OWN, true, true, false, false),
            OwnershipDecision::Refuse { incumbent: OTHER }
        );
    }

    #[test]
    fn ownership_negative_controls_proceed() {
        // dead pid, own pid, absent/unparseable file, live non-daemon pid
        assert_eq!(
            decide_ownership(Some(OTHER), OWN, false, false, false, false),
            OwnershipDecision::Proceed
        );
        assert_eq!(
            decide_ownership(Some(OWN), OWN, true, true, false, false),
            OwnershipDecision::Proceed
        );
        assert_eq!(
            decide_ownership(None, OWN, false, false, false, false),
            OwnershipDecision::Proceed
        );
        assert_eq!(
            decide_ownership(Some(OTHER), OWN, true, false, false, false),
            OwnershipDecision::Proceed
        );
    }

    #[test]
    fn scratch_pid_file_never_refuses() {
        assert_eq!(
            decide_ownership(Some(OTHER), OWN, true, true, true, false),
            OwnershipDecision::Proceed
        );
    }

    #[test]
    fn override_proceeds_with_warn() {
        assert_eq!(
            decide_ownership(Some(OTHER), OWN, true, true, false, true),
            OwnershipDecision::ProceedWithWarn { incumbent: OTHER }
        );
    }

    #[tokio::test]
    async fn two_daemons_one_real_pid_file_second_is_refused_and_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PID_FILENAME);
        // First daemon claims the file.
        let _ = claim_at(&path, OTHER);
        let zero = std::time::Duration::ZERO;
        // Scratch predicate injected: the tempdir is scratch-style for real.
        let second = check_ownership_at(&path, OWN, false, false, |_| true, |_| true, zero).await;
        assert_eq!(second, OwnershipDecision::Refuse { incumbent: OTHER });
        assert_eq!(read_pid_file(&path), Some(OTHER));

        // Override starts it, with a warning decision.
        let allowed = check_ownership_at(&path, OWN, false, true, |_| true, |_| true, zero).await;
        assert_eq!(allowed, OwnershipDecision::ProceedWithWarn { incumbent: OTHER });
    }

    #[tokio::test]
    async fn two_scratch_rooted_daemons_both_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PID_FILENAME);
        let _ = claim_at(&path, OTHER);
        let zero = std::time::Duration::ZERO;
        let d = check_ownership_at(&path, OWN, true, false, |_| true, |_| true, zero).await;
        assert_eq!(d, OwnershipDecision::Proceed);
    }

    #[tokio::test]
    async fn relaunch_handoff_within_window_proceeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PID_FILENAME);
        let _ = claim_at(&path, OTHER);
        let alive = std::sync::atomic::AtomicBool::new(true);
        let calls = std::sync::atomic::AtomicU32::new(0);
        let d = check_ownership_at(
            &path,
            OWN,
            false,
            false,
            |_| {
                // Incumbent exits after the first probe.
                if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 1 {
                    alive.store(false, std::sync::atomic::Ordering::SeqCst);
                }
                alive.load(std::sync::atomic::Ordering::SeqCst)
            },
            |_| true,
            std::time::Duration::from_secs(2),
        )
        .await;
        assert_eq!(d, OwnershipDecision::Proceed);
    }

    #[tokio::test]
    async fn unreadable_pid_file_fails_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing").join(PID_FILENAME);
        let zero = std::time::Duration::ZERO;
        let d = check_ownership_at(&path, OWN, false, false, |_| true, |_| true, zero).await;
        assert_eq!(d, OwnershipDecision::Proceed);
    }

    // ---------------- stale detection ----------------

    fn obs(path: &Path, recorded: Option<u32>, alive: bool) -> PidFileObservation {
        PidFileObservation {
            path: path.to_path_buf(),
            present: recorded.is_some(),
            recorded_pid: recorded,
            recorded_pid_alive: alive,
        }
    }

    #[test]
    fn observation_reads_disk_and_liveness() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PID_FILENAME);
        fs::write(&path, "321\n").unwrap();
        let observed = observe_with(&path, |pid| pid == 321);
        assert!(observed.present);
        assert_eq!(observed.recorded_pid, Some(321));
        assert!(observed.recorded_pid_alive);
    }

    #[test]
    fn absent_file_is_not_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PID_FILENAME);
        let observed = observe_with(&path, |_| true);
        let state = classify(&observed, Some(99917));
        assert_eq!(state, PidFileState::Absent);
        assert!(!state.is_stale());
        assert_eq!(state.note(&path), None);
    }

    #[test]
    fn garbage_contents_are_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PID_FILENAME);
        fs::write(&path, "not-a-pid").unwrap();
        let state = classify(&observe_with(&path, |_| true), Some(1));
        assert_eq!(state, PidFileState::Unparseable);
        assert!(state.is_stale());
        assert!(state.note(&path).is_some());
    }

    #[test]
    fn matching_socket_owner_is_healthy() {
        let path = PathBuf::from("/tmp/x/.daemon.pid");
        let state = classify(&obs(&path, Some(99917), true), Some(99917));
        assert_eq!(state, PidFileState::Matches(99917));
        assert!(!state.is_stale());
        assert_eq!(state.note(&path), None);
    }

    #[test]
    fn different_socket_owner_is_a_mismatch() {
        // The 2026-07-31 incident, exactly.
        let path = PathBuf::from("/tmp/x/.daemon.pid");
        let state = classify(&obs(&path, Some(13724), true), Some(99917));
        assert_eq!(
            state,
            PidFileState::Mismatch {
                recorded: 13724,
                socket_owner: 99917
            }
        );
        assert!(state.is_stale());
        let note = state.note(&path).unwrap();
        assert!(note.contains("13724") && note.contains("99917"), "note: {note}");
    }

    #[test]
    fn a_mismatch_wins_even_when_the_recorded_pid_is_dead() {
        // Both anomalies at once: the socket-owner comparison is the stronger
        // statement, so it is what the operator is told.
        let path = PathBuf::from("/tmp/x/.daemon.pid");
        let state = classify(&obs(&path, Some(13724), false), Some(99917));
        assert!(matches!(state, PidFileState::Mismatch { .. }));
    }

    #[test]
    fn dead_recorded_pid_without_a_socket_owner_is_stale() {
        let path = PathBuf::from("/tmp/x/.daemon.pid");
        let state = classify(&obs(&path, Some(13724), false), None);
        assert_eq!(state, PidFileState::Dead(13724));
        assert!(state.is_stale());
    }

    #[test]
    fn live_recorded_pid_without_a_socket_owner_is_unverified_not_stale() {
        let path = PathBuf::from("/tmp/x/.daemon.pid");
        let state = classify(&obs(&path, Some(13724), true), None);
        assert_eq!(state, PidFileState::Unverified(13724));
        assert!(!state.is_stale());
        assert_eq!(state.note(&path), None);
    }
}
