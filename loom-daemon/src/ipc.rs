use crate::activity::{ActivityDb, AgentInput, AgentOutput, InputContext, InputType};
use crate::config_resolver;
use crate::errors::DaemonError;
use crate::event_bus::EventBus;
use crate::forge_parser::parse_forge_events;
use crate::git_parser;
use crate::git_utils;
use crate::main_health_gate::WorkspaceHealthStates;
use crate::role_validation;
use crate::sweep_registry::{
    poll_and_classify_spawned_child, BeginCancel, BeginIssueDispatch, SweepRegistry,
};
use crate::terminal::TerminalManager;
use crate::types::{CredentialPreflightReport, DaemonStatusReport, Event, Request, Response};
use crate::workspace_pool::WorkspacePool;
use crate::workspace_registry::WorkspaceRegistry;
use anyhow::Result;
use chrono::Utc;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::Instant;
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
//   | startup failure (e.g. #3806)  | 1 EXIT_STARTUP_FAILURE | NO relaunch |
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
/// Exit code for a startup failure the daemon reports and terminates on itself
/// (#4531) — most visibly the singleton guard's "another loom-daemon is already
/// listening" refusal below.
///
/// Deliberately `1`, the exact value `std::process::ExitCode::FAILURE` carries:
/// `main` previously let such errors propagate out of `#[tokio::main]` and relied
/// on `Termination for Result` to print `Error: {err:?}` and exit `1`. That path
/// is correct but **not prompt** — the generated wrapper drops the `Runtime`
/// after `block_on` returns, and `Runtime::drop` blocks until every in-flight
/// `spawn_blocking` task finishes, which on a host with real work configured
/// stalled the refusing process for ~10s (and looked like an indefinite hang
/// under a shorter timeout). `main` now prints the same message and exits with
/// this code directly, so the observable contract (message + status) is
/// unchanged while termination becomes immediate. Non-zero ⇒ launchd does not
/// relaunch, which is what a refusal wants.
pub const EXIT_STARTUP_FAILURE: i32 = 1;

/// Detect the daemon's process supervisor from the environment (#4054, #4267).
///
/// Returns `Some("launchd")` when `LOOM_DAEMON_SUPERVISOR=launchd`
/// (case-insensitive) is present — a value `loom-daemon-start.sh` bakes into the
/// launchd plist's `EnvironmentVariables`, so it survives a relaunch. Likewise
/// returns `Some("systemd")` when `LOOM_DAEMON_SUPERVISOR=systemd`
/// (case-insensitive) is present — a value the systemd unit's `Environment=`
/// bakes in, relying on `Restart=on-success` to relaunch the daemon after the
/// clean `EXIT_RESTART` exit. Any other or absent value ⇒ `None` (the daemon is
/// unsupervised: nohup / Linux without a recognized supervisor / `--foreground`),
/// and the restart primitive must refuse to end the process because nothing
/// would bring it back.
pub fn detect_supervisor() -> Option<String> {
    match std::env::var("LOOM_DAEMON_SUPERVISOR") {
        Ok(v) if v.eq_ignore_ascii_case("launchd") => Some("launchd".to_string()),
        Ok(v) if v.eq_ignore_ascii_case("systemd") => Some("systemd".to_string()),
        _ => None,
    }
}

/// Compose the "restart scheduled" ack message for a supervised relaunch, worded
/// PER-SUPERVISOR because the two recognized supervisors treat in-flight sweep /
/// role children FUNDAMENTALLY DIFFERENTLY when the daemon exits (#5119):
///
/// * **launchd** — the daemon's children reparent to `pid 1` on its exit and keep
///   running, so in-flight sweeps GENUINELY survive the process boundary (verified
///   repeatedly on macOS — #5081). The relaunched daemon re-adopts them from the
///   forge/checkpoints.
/// * **systemd** — the daemon's children run INSIDE the service's cgroup, so
///   systemd's stop job signals them by construction the moment the main process
///   exits. Under the canonical `KillMode=mixed` unit (#4862) the remaining cgroup
///   processes get a `SIGKILL` immediately after the main process exits; under an
///   older `KillMode=control-group` unit they get a `SIGTERM` and then a `SIGKILL`
///   at `TimeoutStopSec`. Either way in-flight sweeps and role runs are TERMINATED,
///   not preserved — which is exactly what happened on loom-worker-1 on 2026-08-03
///   (3 role runs + 1 sweep killed).
///   The pre-#5119 message printed "In-flight sweeps survive by design" on every
///   platform — a macOS-only truth that was actively false on systemd, where a
///   `restart` landing on a busy host destroyed the very work it claimed to
///   protect. The systemd wording now states plainly that in-flight work is lost
///   and points at `--drain` (which empties the sweep registry BEFORE exiting, so
///   the cgroup is empty when the stop job runs) as the preserving alternative.
///
///   CAVEAT (issue #6129, not yet reconciled into the wording below): "run
///   INSIDE the service's cgroup" is only true for a child this daemon execs
///   DIRECTLY. When `spawn-claude.sh`'s CPU-quota mechanism (#5111,
///   default-on whenever a `systemd --user` manager is reachable) wraps that
///   child in `systemd-run --user --scope`, the wrapped process ends up in
///   an INDEPENDENT scope parented to the user manager — a sibling cgroup,
///   not a descendant of this unit's own — so this unit's stop job cannot
///   reach it at all and it behaves like the launchd case instead (survives,
///   silently, with no forge-visible owner). The 2026-08-13 loom-worker-2
///   incident this issue documents is exactly that: `systemctl --user stop
///   loom-daemon` left role-agent scopes running. Which of the two shapes
///   applies to a given child is invisible from here (the wrapping decision
///   is entirely inside `spawn-claude.sh`, an opaque subprocess boundary),
///   so the message below is deliberately NOT rewritten to guess — an
///   operator who needs a DEFINITE answer either way should use
///   `loom-daemon-quiesce.sh` (which enumerates real scopes/processes
///   instead of asserting a platform-wide claim) rather than trust this
///   message's systemd branch as gospel.
///
/// `in_flight` is the current non-terminal sweep count (normally the cross-root
/// [`count_in_flight_sweeps`]) — it makes the systemd warning *specific* about how
/// much work this exit is about to destroy. Role runs have **no registry entry to
/// count** (the #4090 residual), so the wording names them explicitly rather than
/// pretending the number covers them: `0 sweep(s)` never means "nothing to lose".
///
/// Pure (no env / no I/O beyond the caller-supplied arguments) so both wordings are
/// unit-testable without a live supervisor.
#[must_use]
pub fn restart_scheduled_message(supervisor: &str, in_flight: usize) -> String {
    if supervisor == "launchd" {
        // Preserved semantics: sweeps DO survive on launchd (children reparent
        // to pid 1). Wording kept close to the historical message so operators
        // and existing playbooks still recognize it — and deliberately does NOT
        // name a count, because nothing here is at risk.
        "restart scheduled: exiting 0 for a launchd-supervised relaunch. \
         In-flight sweeps survive by design (their child processes reparent to \
         launchd and keep running); the relaunched daemon re-reads the same launchd \
         plist, so it comes back with exactly its start flags."
            .to_string()
    } else {
        // systemd (and any other cgroup-scoped supervisor): be HONEST that the
        // stop job reaps the cgroup. Do NOT claim sweeps survive.
        format!(
            "restart scheduled: exiting 0 for a {supervisor}-supervised relaunch. \
             WARNING: in-flight sweeps and role runs do NOT survive on {supervisor} — \
             they run inside this service's cgroup, so the stop job terminates them \
             (SIGKILL under KillMode=mixed; SIGTERM then a SIGKILL at TimeoutStopSec \
             under an older KillMode=control-group) as this process exits. \
             {in_flight} sweep(s) are in flight right now, plus any role runs (which \
             have no registry entry to count, so this number never means \"nothing to \
             lose\"). The relaunched daemon re-reads its {supervisor} unit's \
             configuration, so it comes back with exactly its start flags, but any \
             work that was mid-flight is lost. To preserve it, use \
             `loom-daemon restart --drain`, which waits for in-flight sweeps to finish \
             before exiting so the cgroup is empty when the stop job runs."
        )
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
///
/// `in_flight` (Issue #5119) is the current cross-root non-terminal sweep
/// count, used only to make the scheduled-restart message honest about what the
/// exit is about to do to that work — see [`restart_scheduled_message`].
pub fn build_restart_decision(in_flight: usize) -> (Response, bool) {
    match detect_supervisor() {
        Some(sup) => (
            Response::DaemonRestart {
                scheduled: true,
                supervisor: Some(sup.clone()),
                message: restart_scheduled_message(&sup, in_flight),
            },
            true,
        ),
        None => (
            Response::DaemonRestart {
                scheduled: false,
                supervisor: None,
                message: "refusing to restart: no supervisor detected \
                    (LOOM_DAEMON_SUPERVISOR unset). This daemon was not started under \
                    a recognized supervisor (nohup / Linux / --foreground), so nothing \
                    would relaunch it if it exited. Leaving it running. Restart it \
                    manually with loom-daemon-stop.sh && loom-daemon-start.sh. If this IS \
                    a systemd --user service (e.g. a fleet worker provisioned before #4640), \
                    retrofit it instead of restarting manually: mkdir -p \
                    ~/.config/systemd/user/loom-daemon.service.d && printf \
                    '[Service]\\nEnvironment=LOOM_DAEMON_SUPERVISOR=systemd\\nRestart=on-success\\n' \
                    > ~/.config/systemd/user/loom-daemon.service.d/supervisor.conf && \
                    systemctl --user daemon-reload."
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

/// Multiplier applied to the requested drain timeout to size the **total**
/// paused-dispatch budget a *retained* ("pending") roll may spend across all of
/// its automatic re-arms (Issue #6007).
///
/// Sizing the budget from the operator's own `--timeout` — rather than from a
/// flat constant — keeps a deliberately short drain short: `--timeout 60` buys a
/// 240s budget, not four hours.
pub const DRAIN_PENDING_BUDGET_MULTIPLIER: u64 = 4;

/// Absolute cap on the pending-roll budget, however large a `--timeout` was
/// requested (Issue #6007). A host must never stop taking work for longer than
/// this on account of a version roll.
pub const MAX_DRAIN_PENDING_BUDGET_SECS: u64 = 4 * 3600;

/// Cap on any single re-armed retry window (Issue #6007) — the windows widen
/// geometrically, and this stops the widening.
pub const MAX_DRAIN_RETRY_WINDOW_SECS: u64 = 2 * 3600;

/// A retry window shorter than this is not worth re-arming: the remaining budget
/// is spent, so the roll is abandoned instead (Issue #6007).
pub const MIN_DRAIN_RETRY_WINDOW_SECS: u64 = 60;

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
    /// `true` when this drain's terminal action is "exit and stay down"
    /// rather than "exit for a supervised relaunch" (Issue #4343 — `fleet
    /// drain`'s teardown use case). See [`Request::DrainAndRestartDaemon`]'s
    /// `then_exit` field.
    pub then_exit: bool,
    /// When this drain started — the anchor for the pending-roll budget
    /// (Issue #6007).
    pub started_at: Option<chrono::DateTime<Utc>>,
    /// The drain timeout this drain was *requested* with. Retained (rather than
    /// only being folded into `deadline`) so a pending roll can size its retry
    /// windows and its total budget from the operator's own number (#6007).
    pub base_timeout: Duration,
    /// How many deadline refusals this drain has already survived (#6007). `0`
    /// for a drain that has not yet reached its first deadline.
    pub refusals: u32,
    /// `true` while the roll intent is **retained** across a deadline refusal:
    /// new dispatch stays paused and the restart re-arms itself the moment
    /// in-flight next reaches zero (Issue #6007). This is the state that keeps a
    /// busy host converging on a new binary without an operator re-issuing
    /// `restart --drain` with a bigger `--timeout`.
    pub roll_pending: bool,
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
    AlreadyDraining {
        /// The **active** drain's actual terminal action after this request was
        /// applied — `true` ⇒ it will exit and stay down, `false` ⇒ it will exit
        /// for a supervised relaunch. Never a blind echo of the request
        /// (Issue #4521): the caller must render its ack from this, or it will
        /// promise a teardown that never happens.
        active_then_exit: bool,
        /// `true` when this request *escalated* an in-progress relaunch-drain to
        /// stay-down (the one-way `then_exit` transition — see
        /// [`DrainState::begin`]).
        escalated: bool,
        /// `true` when this request escalated a **pending roll** to
        /// `--force-after-timeout` and pulled its re-armed deadline in to now
        /// (Issue #6007 — see [`DrainState::begin`]). Only ever `true` while
        /// `roll_pending`, so #4521's "the active drain's deadline/force flag
        /// stay pinned" invariant is untouched for a first-attempt drain.
        force_escalated: bool,
    },
}

/// What a pending-roll deadline refusal decided to do (Issue #6007). Pure
/// counterpart of [`drain_refusal_decision`], so the widen-then-give-up policy
/// is unit-testable without driving a real supervisor to a real deadline.
#[derive(Debug, PartialEq, Eq)]
pub enum RefusalDecision {
    /// Retain the roll: keep dispatch paused and re-arm the deadline `window`
    /// from now.
    Defer { window: Duration },
    /// The paused-dispatch budget is spent — discard the roll intent and resume
    /// dispatch (the pre-#6007 terminal behavior).
    Abandon,
}

/// The outcome [`DrainState::refuse_roll_deadline`] applied (Issue #6007).
#[derive(Debug, PartialEq, Eq)]
pub enum RollRefusal {
    /// The roll survived the refusal: dispatch is still paused, the deadline was
    /// re-armed `window` out, and the supervisor keeps polling.
    Deferred {
        /// 1-based retry counter (`1` for the first refusal).
        attempt: u32,
        /// The re-armed window.
        window: Duration,
        /// Time since the drain began.
        elapsed: Duration,
        /// Total paused-dispatch budget for this roll.
        budget: Duration,
    },
    /// The budget is spent: the flag was cleared, the generation bumped, and the
    /// roll intent discarded.
    Abandoned {
        /// How many times the roll was re-armed before giving up.
        attempts: u32,
        /// Time since the drain began.
        elapsed: Duration,
        /// Total paused-dispatch budget that was available.
        budget: Duration,
    },
}

/// Which fail-safe path a [`DrainTick::TimedOutRefuse`] tick takes (Issue #6007).
#[derive(Debug, PartialEq, Eq)]
pub enum RefusalPath {
    /// **Relaunch (roll) drains**: retain the intent — keep dispatch paused and
    /// re-arm the deadline ([`DrainState::refuse_roll_deadline`]).
    RetainRoll,
    /// **Then-exit (teardown) drains**: resume dispatch immediately and discard
    /// the intent — the pre-#6007 behavior, kept byte-for-byte because
    /// `fleet drain` orchestrates teardowns over SSH and detects a remote refusal
    /// by observing `drain.draining == false` on a still-reachable daemon.
    ResumeDispatch,
}

/// Pick the fail-safe path for a refused deadline (Issue #6007). Extracted as a
/// pure function so the roll-vs-teardown split is a test assertion rather than a
/// branch only reachable by driving a real supervisor to a real deadline.
#[must_use]
pub fn drain_refusal_path(then_exit: bool) -> RefusalPath {
    if then_exit {
        RefusalPath::ResumeDispatch
    } else {
        RefusalPath::RetainRoll
    }
}

/// Total paused-dispatch budget a retained ("pending") roll may spend, derived
/// from the operator's requested drain timeout (Issue #6007).
#[must_use]
pub fn drain_pending_budget(base: Duration) -> Duration {
    let scaled = base
        .as_secs()
        .saturating_mul(DRAIN_PENDING_BUDGET_MULTIPLIER);
    Duration::from_secs(scaled.min(MAX_DRAIN_PENDING_BUDGET_SECS))
}

/// Decide what a deadline refusal on a **relaunch (roll)** drain should do
/// (Issue #6007): re-arm a widened window, or give up because the total
/// paused-dispatch budget is spent.
///
/// The windows widen geometrically from the operator's own `--timeout`
/// (`base * 2^attempt`), each capped at [`MAX_DRAIN_RETRY_WINDOW_SECS`] and at
/// whatever budget remains — this is the operator's manual
/// "re-run with a larger `--timeout`" workaround, automated. When less than
/// [`MIN_DRAIN_RETRY_WINDOW_SECS`] of budget remains there is nothing useful
/// left to wait for, so the roll is abandoned and dispatch resumes rather than
/// starving the host of work indefinitely.
#[must_use]
pub fn drain_refusal_decision(
    base: Duration,
    refusals_so_far: u32,
    elapsed: Duration,
) -> RefusalDecision {
    let budget = drain_pending_budget(base);
    let remaining = budget.saturating_sub(elapsed).as_secs();
    if remaining < MIN_DRAIN_RETRY_WINDOW_SECS {
        return RefusalDecision::Abandon;
    }
    // `min(16)` only guards the shift; the widened value is capped immediately
    // below anyway.
    let widened = base
        .as_secs()
        .saturating_mul(1u64 << refusals_so_far.saturating_add(1).min(16));
    let window = widened
        .min(MAX_DRAIN_RETRY_WINDOW_SECS)
        .min(remaining)
        .max(MIN_DRAIN_RETRY_WINDOW_SECS);
    RefusalDecision::Defer {
        window: Duration::from_secs(window),
    }
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
    ///
    /// **`then_exit` on the already-draining path (Issue #4521 — design
    /// decision).** `timeout`/`force_after_timeout` stay pinned to the active
    /// drain (a later idempotent ack must not move a deadline someone is already
    /// waiting on), but `then_exit` is **escalated one-way**:
    /// `relaunch → stay-down`, never the reverse.
    ///
    /// Rationale: the two options were (a) refuse the escalation and tell the
    /// operator to `--abort-drain` and re-issue, or (b) escalate in place. (a)
    /// is racy in exactly the case that matters — an operator tearing a host
    /// down while an auto-update roll-drain (`then_exit=false`,
    /// `auto_update.rs`) is in flight would have to abort and re-issue, and the
    /// roll can complete *between* those two commands, relaunching the daemon on
    /// a host that is about to be powered off. (b) is monotonic and safe: exiting
    /// and staying down is strictly the more conservative terminal action, and
    /// the operator's teardown intent is honored on the first command. The
    /// reverse direction is deliberately **not** applied — a roll trigger
    /// arriving during an operator teardown drain must never silently downgrade
    /// the teardown into a relaunch.
    ///
    /// The escalation is observed by the already-running supervisor because it
    /// re-reads `then_exit` from this descriptor at its terminal tick rather
    /// than using a value captured at spawn (see [`run_drain_supervisor`]).
    ///
    /// **`force_after_timeout` on the already-draining path (Issue #6007).** It
    /// stays pinned exactly as #4521 specified — *except* while the drain is a
    /// **pending roll** (`roll_pending`), where it escalates one-way
    /// (`refuse → force`) and pulls the re-armed deadline in to now. Rationale:
    /// once a roll is pending, the only deadline left is one *this daemon* chose
    /// as a retry window, not one the operator is waiting on, and the pending
    /// note tells the operator to run exactly
    /// `restart --drain --force-after-timeout` to force through. Pinning the flag
    /// there would make that documented command a silent no-op. A first-attempt
    /// drain is untouched: its operator-set deadline and flag stay pinned.
    pub fn begin(
        &self,
        timeout: Duration,
        force_after_timeout: bool,
        then_exit: bool,
    ) -> DrainBegin {
        let mut inner = self.inner.lock().expect("Drain mutex poisoned");
        if inner.active {
            let escalated = then_exit && !inner.then_exit;
            // #6007: gated on `roll_pending` — see the doc comment above.
            let force_escalated =
                force_after_timeout && !inner.force_after_timeout && inner.roll_pending;
            if escalated {
                inner.then_exit = true;
            }
            if force_escalated {
                inner.force_after_timeout = true;
                // Act on the escalation now rather than at the end of a retry
                // window this daemon picked: the next supervisor tick
                // (≤ DRAIN_POLL_INTERVAL) reaches TimedOutForce.
                inner.deadline = Some(Utc::now());
            }
            match (escalated, force_escalated) {
                (true, false) => {
                    inner.note = Some(
                        "in-progress drain escalated to then-exit — will stop and stay down \
                         (was: exit for a supervised relaunch)"
                            .to_string(),
                    );
                }
                (false, true) => {
                    inner.note = Some(
                        "pending roll escalated to --force-after-timeout — the remaining \
                         in-flight sweep(s) will be cancelled and the restart will fire on the \
                         next supervisor tick"
                            .to_string(),
                    );
                }
                (true, true) => {
                    inner.note = Some(
                        "in-progress drain escalated to then-exit AND to \
                         --force-after-timeout — the remaining in-flight sweep(s) will be \
                         cancelled, then the daemon will stop and stay down"
                            .to_string(),
                    );
                }
                (false, false) => {}
            }
            return DrainBegin::AlreadyDraining {
                active_then_exit: inner.then_exit,
                escalated,
                force_escalated,
            };
        }
        let deadline = Utc::now()
            + chrono::Duration::from_std(timeout).unwrap_or_else(|_| chrono::Duration::seconds(0));
        inner.active = true;
        inner.deadline = Some(deadline);
        inner.force_after_timeout = force_after_timeout;
        inner.then_exit = then_exit;
        inner.note = None;
        // #6007 pending-roll bookkeeping — a fresh drain always starts with a
        // clean retry history.
        inner.started_at = Some(Utc::now());
        inner.base_timeout = timeout;
        inner.refusals = 0;
        inner.roll_pending = false;
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
        // #6007: an abort is also the operator's way OUT of a retained (pending)
        // roll, so say so — otherwise "dispatch resumed" reads identically for
        // two quite different states.
        inner.note = Some(if inner.roll_pending {
            "drain aborted by operator — the pending roll was cancelled and dispatch resumed; \
             this host stays on its current binary until a new roll is triggered"
                .to_string()
        } else {
            "drain aborted by operator — dispatch resumed".to_string()
        });
        inner.roll_pending = false;
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
        inner.roll_pending = false;
        inner.note = Some(note);
    }

    /// Record a note on the active/last drain without touching any other state
    /// (Issue #6007) — the supervisor renders its note *after*
    /// [`Self::refuse_roll_deadline`] has decided what to do, since the wording
    /// depends on the decision.
    pub fn set_note(&self, note: String) {
        let mut inner = self.inner.lock().expect("Drain mutex poisoned");
        inner.note = Some(note);
    }

    /// The Issue #6007 fail-safe deadline path for a **relaunch (roll)** drain:
    /// retain the roll instead of discarding it.
    ///
    /// This is the fix for the drain/work-finder livelock. Before #6007 the
    /// deadline called [`Self::resolve_timeout`], which cleared the pause flag —
    /// handing the admission window straight back to the work finder, which
    /// admitted more sweeps, which made the *next* drain strictly harder to
    /// satisfy. On a host that is actually working, in-flight never reached zero
    /// and a drain-based roll never landed.
    ///
    /// Now the intent survives: the pause flag stays set, the deadline is
    /// re-armed on a widened window, the generation is **not** bumped (so the
    /// same supervisor keeps polling and completes the restart the instant
    /// in-flight reaches zero), and only once the total paused-dispatch budget is
    /// spent does the roll give up — resuming dispatch exactly as before, so a
    /// genuinely wedged sweep can never starve the host of work forever.
    ///
    /// `now` is injected so the whole widen-then-give-up sequence is testable
    /// without sleeping.
    pub fn refuse_roll_deadline(&self, now: chrono::DateTime<Utc>) -> RollRefusal {
        let mut inner = self.inner.lock().expect("Drain mutex poisoned");
        let base = inner.base_timeout;
        let started = inner.started_at.unwrap_or(now);
        let elapsed = (now - started).to_std().unwrap_or_default();
        let budget = drain_pending_budget(base);
        match drain_refusal_decision(base, inner.refusals, elapsed) {
            RefusalDecision::Defer { window } => {
                inner.refusals = inner.refusals.saturating_add(1);
                inner.roll_pending = true;
                inner.deadline = Some(
                    now + chrono::Duration::from_std(window)
                        .unwrap_or_else(|_| chrono::Duration::seconds(0)),
                );
                // Deliberately NOT touched: `self.flag` (dispatch stays paused —
                // the whole point) and `self.generation` (the live supervisor
                // must keep supervising, and an operator `abort` must still be
                // able to supersede it).
                RollRefusal::Deferred {
                    attempt: inner.refusals,
                    window,
                    elapsed,
                    budget,
                }
            }
            RefusalDecision::Abandon => {
                let attempts = inner.refusals;
                self.flag.store(false, Ordering::Relaxed);
                self.generation.fetch_add(1, Ordering::Relaxed);
                inner.active = false;
                inner.deadline = None;
                inner.roll_pending = false;
                RollRefusal::Abandoned {
                    attempts,
                    elapsed,
                    budget,
                }
            }
        }
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
    /// restart and stay up. What happens to *dispatch* then depends on the
    /// drain's terminal action (Issue #6007): a **relaunch (roll)** drain retains
    /// its intent and keeps dispatch paused
    /// ([`DrainState::refuse_roll_deadline`]), while a **then-exit (teardown)**
    /// drain keeps the historical behavior and resumes dispatch immediately
    /// ([`DrainState::resolve_timeout`]).
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
// A poisoned registry mutex is recovered rather than crashed (#4279): a prior
// panic must never turn a single fault into a permanent drain/status outage.
#[must_use]
pub fn count_in_flight_sweeps(workspace_pool: &Arc<WorkspacePool>, fallback_root: &Path) -> usize {
    let workspace_registry = WorkspaceRegistry::load_default().unwrap_or_default();
    let roots = workspace_registry.effective_roots(fallback_root);
    let mut count = 0;
    for root in &roots {
        let registry = workspace_pool.get_or_provision(root);
        let sr = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
// A poisoned registry mutex is recovered rather than crashed (#4279).
fn cancel_all_in_flight(workspace_pool: &Arc<WorkspacePool>, fallback_root: &Path) -> usize {
    let workspace_registry = WorkspaceRegistry::load_default().unwrap_or_default();
    let roots = workspace_registry.effective_roots(fallback_root);
    let mut cancelled = 0;
    for root in &roots {
        let registry = workspace_pool.get_or_provision(root);
        let ids: Vec<String> = {
            let sr = registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            sr.list(None)
                .into_iter()
                .filter(|info| !info.state.is_terminal())
                .map(|info| info.sweep_id)
                .collect()
        };
        for id in ids {
            let mut sr = registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
///
/// Made `pub` for the autonomous self-update loop (#4055): after it rebuilds
/// and provisions a fresh binary, it triggers the roll through this exact drain
/// path — not a bare `RestartDaemon` — so in-flight sweeps finish first and
/// survive in the registry rather than being orphaned. The loop calls it from a
/// blocking thread inside a `tokio::runtime::Handle::enter()` guard so the
/// internal `tokio::spawn` of the supervisor still resolves a runtime.
pub fn handle_drain_request(
    drain: &Arc<DrainState>,
    workspace_pool: &Arc<WorkspacePool>,
    fallback_root: &Path,
    event_bus: &Arc<EventBus>,
    timeout_secs: Option<u64>,
    force_after_timeout: bool,
    then_exit: bool,
) -> Response {
    // AC5 / Finding 4: prove supervision BEFORE entering DRAINING — for the
    // #4090 restart-when-drained case. A `then_exit` drain (#4343) deliberately
    // does NOT want a relaunch, so the supervisor requirement does not apply to
    // it at all: skip the refusal gate entirely and just report whatever
    // supervisor (if any) is detected, informationally.
    let supervisor = if then_exit {
        detect_supervisor()
    } else {
        match detect_supervisor() {
            Some(s) => Some(s),
            None => {
                return Response::DaemonDrain {
                    accepted: false,
                    supervisor: None,
                    in_flight: count_in_flight_sweeps(workspace_pool, fallback_root),
                    message: "refusing to drain: no supervisor detected \
                        (LOOM_DAEMON_SUPERVISOR unset). This daemon was not started under \
                        a recognized supervisor, so nothing would relaunch it after a drain. \
                        Dispatch was NOT paused. Restart manually with loom-daemon-stop.sh && \
                        loom-daemon-start.sh. If this IS a systemd --user service (e.g. a \
                        fleet worker provisioned before #4640), retrofit it instead: mkdir -p \
                        ~/.config/systemd/user/loom-daemon.service.d && printf \
                        '[Service]\\nEnvironment=LOOM_DAEMON_SUPERVISOR=systemd\\nRestart=on-success\\n' \
                        > ~/.config/systemd/user/loom-daemon.service.d/supervisor.conf && \
                        systemctl --user daemon-reload."
                        .to_string(),
                    then_exit,
                };
            }
        }
    };

    let in_flight = count_in_flight_sweeps(workspace_pool, fallback_root);
    let timeout = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_DRAIN_TIMEOUT_SECS));

    match drain.begin(timeout, force_after_timeout, then_exit) {
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
                    "then_exit": then_exit,
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
            let msg = if then_exit {
                if in_flight == 0 {
                    "drain scheduled (then-exit): 0 in-flight — stopping now (will NOT relaunch)."
                        .to_string()
                } else {
                    format!(
                        "drain scheduled (then-exit): {in_flight} in-flight sweep(s); new dispatch \
                         paused. Will stop (NOT relaunch) when drained, or {} at the deadline.",
                        if force_after_timeout {
                            "cancel stragglers and stop"
                        } else {
                            "refuse and resume dispatch"
                        }
                    )
                }
            } else if in_flight == 0 {
                format!(
                    "drain scheduled ({}-supervised): 0 in-flight — restarting now.",
                    supervisor.as_deref().unwrap_or("unknown")
                )
            } else {
                // #6007: the non-force deadline no longer "refuses and resumes
                // dispatch" on a roll — promising that is what taught operators
                // to re-run with a bigger --timeout in the first place.
                let deadline_action = if force_after_timeout {
                    "cancel stragglers and restart".to_string()
                } else {
                    format!(
                        "hold the roll PENDING (dispatch stays paused, the restart re-arms \
                         itself when in-flight reaches zero, for up to {}s total before giving \
                         up and resuming dispatch)",
                        drain_pending_budget(timeout).as_secs()
                    )
                };
                format!(
                    "drain scheduled ({}-supervised): {in_flight} in-flight sweep(s); \
                     new dispatch paused. Will restart when drained, or {deadline_action} at \
                     the deadline.",
                    supervisor.as_deref().unwrap_or("unknown"),
                )
            };
            Response::DaemonDrain {
                accepted: true,
                supervisor,
                in_flight,
                message: msg,
                then_exit,
            }
        }
        // Issue #4521: the ack must describe the **active** drain's terminal
        // action, never the requested one. Echoing the request here is what let
        // an operator's `--drain --then-exit` be acked as "will stop" while an
        // in-progress relaunch-drain (an auto-update roll) exited 0 and launchd
        // brought the daemon straight back up.
        DrainBegin::AlreadyDraining {
            active_then_exit,
            escalated,
            force_escalated,
        } => {
            let _ = event_bus.publish_generic(
                "daemon.drain.already_draining",
                serde_json::json!({
                    "in_flight": in_flight,
                    "requested_then_exit": then_exit,
                    "active_then_exit": active_then_exit,
                    "escalated": escalated,
                    "force_escalated": force_escalated,
                }),
            );
            if escalated {
                log::warn!(
                    "drain escalated to then-exit (Issue #4521): an in-progress relaunch-drain \
                     (e.g. an auto-update roll) will now exit {EXIT_SHUTDOWN} and stay down \
                     instead of relaunching"
                );
            }
            if force_escalated {
                log::warn!(
                    "pending roll escalated to --force-after-timeout (Issue #6007): the \
                     remaining in-flight sweep(s) will be cancelled and the restart will fire on \
                     the next supervisor tick"
                );
            }
            // #6007: a retained (pending) roll must not be acked as if it were a
            // first-attempt drain whose "existing deadline is unchanged" — an
            // operator needs to know the roll already survived a refusal and is
            // waiting on quiescence.
            let roll_pending = drain.snapshot().roll_pending;
            let message = if force_escalated {
                format!(
                    "already draining (idempotent) — ESCALATED to --force-after-timeout: the \
                     pending roll will now CANCEL the {in_flight} remaining in-flight sweep(s) \
                     and restart on the next supervisor tick (within \
                     {}s), instead of waiting for them to finish.",
                    DRAIN_POLL_INTERVAL.as_secs()
                )
            } else if escalated {
                format!(
                    "already draining (idempotent) — ESCALATED to then-exit: the in-progress \
                     drain was a relaunch drain (e.g. an auto-update roll) and will now STOP \
                     and stay down when drained, NOT relaunch. {in_flight} in-flight sweep(s); \
                     the existing deadline is unchanged. If a relaunch was wanted after all, \
                     `loom-daemon restart --abort-drain` and re-issue without --then-exit."
                )
            } else if active_then_exit && !then_exit {
                format!(
                    "already draining (idempotent): the in-progress drain is a then-exit \
                     teardown — it will STOP and stay down when drained, NOT relaunch, so this \
                     restart-when-drained request will not be honored (then-exit is never \
                     downgraded). {in_flight} in-flight sweep(s); the existing deadline is \
                     unchanged. Use `loom-daemon restart --abort-drain` to cancel."
                )
            } else if active_then_exit {
                format!(
                    "already draining (idempotent): the in-progress drain will STOP and stay \
                     down when drained (then-exit). {in_flight} in-flight sweep(s); the existing \
                     deadline is unchanged. Use `loom-daemon restart --abort-drain` to cancel."
                )
            } else if roll_pending {
                format!(
                    "already draining (idempotent): a PENDING ROLL is already retained — it \
                     survived its deadline, dispatch stays paused, and the restart re-arms \
                     itself when in-flight reaches zero. {in_flight} in-flight sweep(s); the \
                     re-armed deadline is unchanged. Use `loom-daemon restart --abort-drain` to \
                     cancel it, or `loom-daemon restart --drain --force-after-timeout` to cancel \
                     the stragglers and roll now."
                )
            } else {
                format!(
                    "already draining (idempotent): the in-progress drain will RESTART when \
                     drained. {in_flight} in-flight sweep(s); the existing deadline is \
                     unchanged. Use `loom-daemon restart --abort-drain` to cancel."
                )
            };
            Response::DaemonDrain {
                accepted: true,
                supervisor,
                in_flight,
                message,
                then_exit: active_then_exit,
            }
        }
    }
}

/// The exit code a terminal drain tick must use (Issue #4521).
///
/// Load-bearing and deliberately extracted as a pure function so the contract is
/// unit-testable without spawning a process: a **then-exit** drain must exit
/// [`EXIT_SHUTDOWN`] (143, non-zero) so a `KeepAlive:{SuccessfulExit:true}`
/// launchd job stays down — exiting [`EXIT_RESTART`] (0) there is precisely the
/// "drained, then relaunched anyway" failure. A relaunch drain exits
/// [`EXIT_RESTART`] so the supervisor brings it straight back.
#[must_use]
pub fn drain_exit_code(then_exit: bool) -> i32 {
    if then_exit {
        EXIT_SHUTDOWN
    } else {
        EXIT_RESTART
    }
}

/// The operator-facing log line emitted when a drain completes with zero
/// in-flight sweeps (Issue #4090, pinned by Issue #4521).
///
/// The two terminal actions **must** produce visibly different lines — a host
/// log has to say which one fired without the reader guessing. Extracted as a
/// pure function so that distinctness is a test assertion rather than a
/// convention. `supervisor` and `verify_poll_secs` are only interpolated on
/// the relaunch line (`verify_poll_secs` is unused on the `then_exit` branch —
/// there is no relaunch to verify there).
#[must_use]
pub fn drain_complete_log_line(then_exit: bool, supervisor: &str, verify_poll_secs: u64) -> String {
    if then_exit {
        format!(
            "drain complete — 0 in-flight sweeps; exiting {EXIT_SHUTDOWN} and staying down \
             (then_exit — Issue #4343 teardown). No sweep was killed; no orphan left behind."
        )
    } else {
        let note = crate::restart_verify::relaunch_verify_note(supervisor, verify_poll_secs);
        format!(
            "drain complete — 0 in-flight sweeps; exiting {EXIT_RESTART} for a \
             {supervisor}-supervised relaunch. No sweep was killed; no orphan left behind. {note}"
        )
    }
}

/// The operator-facing note recorded (and logged) when a drain times out and
/// refuses the restart (Issue #4090, made actionable by Issue #5340).
///
/// Before #5340 this stopped at "no --force-after-timeout", leaving the
/// operator to guess at a retry — the one they filed #5340 over guessed
/// `loom-daemon drain` (a bare, nonexistent subcommand) and then
/// `loom-daemon fleet drain <ssh_host>` (a *different*, newer remote
/// worker-decommission command that takes a completely different argument).
/// Naming the exact local retry command removes that guesswork: it is always
/// the same `restart --drain` invocation the operator already ran, with
/// `--force-after-timeout` added.
///
/// Extracted as a pure function — same rationale as
/// [`drain_complete_log_line`] just above — so the exact wording is a test
/// assertion rather than something only exercised by driving the full
/// supervisor loop to a real timeout.
#[must_use]
pub fn drain_timeout_refuse_note(in_flight: usize) -> String {
    format!(
        "drain timed out with {in_flight} sweep(s) still in flight — refused restart \
         (no --force-after-timeout); dispatch resumed, daemon stays up. Retry with: \
         `loom-daemon restart --drain --force-after-timeout --timeout <secs>` to force through \
         the remaining sweep(s), or re-run with a larger --timeout if they are simply \
         long-running rather than stuck."
    )
}

/// The operator-facing note recorded when a **relaunch (roll)** drain's deadline
/// passes and the roll is *retained* rather than discarded (Issue #6007).
///
/// This replaces the "dispatch resumed — retry with a bigger number" advice on
/// the roll path, which is precisely the advice that reproduced the livelock: on
/// a busy host every re-run raced the same deadline against a work finder that
/// had just been handed the admission window back. The note therefore says what
/// happens about the **recurrence** — nothing to re-run, the roll re-arms itself
/// — and names the two ways an operator can take over instead.
///
/// Extracted as a pure function (same rationale as
/// [`drain_timeout_refuse_note`]) so the wording is a test assertion rather than
/// something only a real 30-minute timeout exercises.
#[must_use]
pub fn drain_roll_pending_note(
    in_flight: usize,
    attempt: u32,
    window: Duration,
    budget: Duration,
) -> String {
    let window_secs = window.as_secs();
    let budget_secs = budget.as_secs();
    format!(
        "drain deadline passed with {in_flight} sweep(s) still in flight — restart REFUSED \
         (fail-safe: no sweep was cancelled and the pre-update binary keeps running). ROLL \
         PENDING (retry {attempt}): the roll intent is RETAINED — new dispatch stays PAUSED so \
         the in-flight set can reach zero, and the restart re-arms itself the moment it does. \
         Nothing to re-run: re-issuing `restart --drain` with a larger --timeout is exactly what \
         this replaces. Next deadline in {window_secs}s; total paused-dispatch budget \
         {budget_secs}s, after which the roll is abandoned and dispatch resumes. To give up now \
         and resume dispatch: `loom-daemon restart --abort-drain`. To force through the remaining \
         sweep(s) instead (cancels them): `loom-daemon restart --drain --force-after-timeout`."
    )
}

/// The operator-facing note recorded when a retained (pending) roll finally gives
/// up because its total paused-dispatch budget is spent (Issue #6007).
///
/// Keeps [`drain_timeout_refuse_note`]'s wording as its prefix — `loom-daemon
/// status` renders it the same way and #5340's exact-retry-command contract still
/// holds — then explains the recurrence: sweeps that outlived this much *paused*
/// dispatch are stuck rather than merely long-running, so the fix is to deal with
/// them, not to widen the window again.
#[must_use]
pub fn drain_roll_abandoned_note(in_flight: usize, attempts: u32, elapsed: Duration) -> String {
    let elapsed_secs = elapsed.as_secs();
    format!(
        "drain timed out with {in_flight} sweep(s) still in flight — refused restart \
         (no --force-after-timeout); dispatch resumed, daemon stays up. The roll was retained \
         and re-armed {attempts} time(s) across {elapsed_secs}s of PAUSED dispatch and in-flight \
         still never reached zero, so the roll intent is now ABANDONED rather than starve this \
         host of work indefinitely — the provisioned binary was NOT activated. A sweep that \
         outlives {elapsed_secs}s of paused dispatch is stuck, not merely long-running: find it \
         with `loom-daemon list`, cancel it with `loom-daemon cancel --sweep <id>`, and the next \
         roll lands on its own. To force through instead: `loom-daemon restart --drain \
         --force-after-timeout --timeout <secs>`."
    )
}

/// The drain-supervisor loop (Issue #4090). Polls the cross-root in-flight count
/// and owns the eventual `std::process::exit(EXIT_RESTART)`; on a fail-safe
/// timeout it clears the drain flag and stays up. Stops without exiting if it
/// has been superseded (a new drain) or aborted (generation moved on).
///
/// The terminal action (`then_exit`) is **re-read from [`DrainState`] on every
/// tick** rather than captured at spawn (Issue #4521): an in-progress
/// relaunch-drain can be escalated to stay-down by a later
/// `--drain --then-exit` request, and a supervisor holding a stale `false` would
/// exit `0` and be relaunched by the supervisor anyway.
async fn run_drain_supervisor(
    drain: Arc<DrainState>,
    workspace_pool: Arc<WorkspacePool>,
    fallback_root: PathBuf,
    event_bus: Arc<EventBus>,
    my_generation: u64,
    poll_interval: Duration,
) {
    // Issue #6969: the bound a detached post-exit verifier polls against,
    // resolved once per supervisor rather than per tick — the same knob
    // `restart_verify::verify_and_heal` itself resolves, so the log line and
    // the verifier it describes can never disagree about the bound.
    let verify_poll_secs = crate::restart_verify::resolve_configured_poll_secs();
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
        // One consistent read of the live descriptor per tick. `then_exit` comes
        // from here — not from a spawn-time argument — so a mid-drain escalation
        // (relaunch → stay-down, Issue #4521) is honored by this supervisor.
        let (past_deadline, force, then_exit) = {
            let snap = drain.snapshot();
            let past = snap.deadline.is_some_and(|d| Utc::now() >= d);
            (past, snap.force_after_timeout, snap.then_exit)
        };

        match evaluate_drain_tick(in_flight, past_deadline, force) {
            DrainTick::Continue => {
                tokio::time::sleep(poll_interval).await;
            }
            DrainTick::Complete => {
                let _ = event_bus.publish_generic(
                    "daemon.drain.completed",
                    serde_json::json!({ "in_flight": 0, "then_exit": then_exit }),
                );
                if then_exit {
                    log::warn!("{}", drain_complete_log_line(true, "", verify_poll_secs));
                    crate::observability::shutdown::exit(drain_exit_code(true)).await;
                }
                // This path only runs after `handle_drain_request` proved
                // supervision, so `detect_supervisor()` should still be `Some`
                // here; fall back to a generic label rather than hardcoding
                // launchd if the environment somehow changed underneath us.
                // Issue #6969: also spawns the detached post-exit verifier
                // BEFORE exiting — see `restart_verify::spawn_detached_verifier`'s
                // doc for why it cannot run in-band here. `pre_pid` is this
                // process's own pid: the supervisor's current record of the
                // daemon that is about to exit.
                let sup = crate::restart_verify::detect_and_spawn_verifier(std::process::id());
                log::warn!("{}", drain_complete_log_line(false, &sup, verify_poll_secs));
                crate::observability::shutdown::exit(drain_exit_code(false)).await;
            }
            DrainTick::TimedOutRefuse => {
                // Issue #6007. A **teardown** (`then_exit`) drain keeps the
                // historical fail-safe byte-for-byte: refuse, resume dispatch,
                // stay up. `fleet drain` orchestrates those over SSH and keys its
                // documented exit-2 contract on the remote reporting
                // `draining: false`, so retaining a pending teardown here would
                // change a remote-decommission contract this issue is not about.
                if drain_refusal_path(then_exit) == RefusalPath::ResumeDispatch {
                    let note = drain_timeout_refuse_note(in_flight);
                    let _ = event_bus.publish_generic(
                        "daemon.drain.timeout",
                        serde_json::json!({
                            "in_flight": in_flight,
                            "forced": false,
                            "then_exit": true,
                            "roll_pending": false,
                        }),
                    );
                    log::warn!("{note}");
                    drain.resolve_timeout(note);
                    return;
                }
                // A **relaunch (roll)** drain retains its intent instead of
                // handing the admission window back to the work finder, which is
                // what made every retry strictly harder to satisfy than the last.
                match drain.refuse_roll_deadline(Utc::now()) {
                    RollRefusal::Deferred {
                        attempt,
                        window,
                        budget,
                        ..
                    } => {
                        let note = drain_roll_pending_note(in_flight, attempt, window, budget);
                        let _ = event_bus.publish_generic(
                            "daemon.drain.roll_pending",
                            serde_json::json!({
                                "in_flight": in_flight,
                                "attempt": attempt,
                                "window_secs": window.as_secs(),
                                "budget_secs": budget.as_secs(),
                            }),
                        );
                        log::warn!("{note}");
                        drain.set_note(note);
                        // Dispatch is still paused and this supervisor is still
                        // the current generation — keep polling so the restart
                        // fires the instant in-flight reaches zero.
                        tokio::time::sleep(poll_interval).await;
                    }
                    RollRefusal::Abandoned {
                        attempts, elapsed, ..
                    } => {
                        let note = drain_roll_abandoned_note(in_flight, attempts, elapsed);
                        let _ = event_bus.publish_generic(
                            "daemon.drain.timeout",
                            serde_json::json!({
                                "in_flight": in_flight,
                                "forced": false,
                                "then_exit": false,
                                "roll_pending": false,
                                "attempts": attempts,
                                "elapsed_secs": elapsed.as_secs(),
                            }),
                        );
                        log::warn!("{note}");
                        // `refuse_roll_deadline` already cleared the flag and
                        // bumped the generation; only the note is left to record.
                        drain.set_note(note);
                        return;
                    }
                }
            }
            DrainTick::TimedOutForce => {
                let cancelled = cancel_all_in_flight(&workspace_pool, &fallback_root);
                let _ = event_bus.publish_generic(
                    "daemon.drain.timeout",
                    serde_json::json!({
                        "in_flight": in_flight,
                        "forced": true,
                        "cancelled": cancelled,
                        "then_exit": then_exit,
                    }),
                );
                if then_exit {
                    log::warn!(
                        "drain timed out with {in_flight} in-flight; --force-after-timeout \
                         cancelled {cancelled} sweep(s); exiting {EXIT_SHUTDOWN} and staying down \
                         (then_exit — Issue #4343 teardown)"
                    );
                    crate::observability::shutdown::exit(drain_exit_code(true)).await;
                }
                // Same detection/fallback as the `DrainTick::Complete` relaunch
                // branch above; this path only reaches here after
                // `handle_drain_request` already proved supervision. Also
                // spawns the same detached post-exit verifier (Issue #6969).
                let sup = crate::restart_verify::detect_and_spawn_verifier(std::process::id());
                log::warn!(
                    "drain timed out with {in_flight} in-flight; --force-after-timeout cancelled \
                     {cancelled} sweep(s); exiting {EXIT_RESTART} for a supervised relaunch. {}",
                    crate::restart_verify::relaunch_verify_note(&sup, verify_poll_secs)
                );
                crate::observability::shutdown::exit(drain_exit_code(false)).await;
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

        // Claim the pid file (#4774). THIS is the correct choke point: the bind
        // above just made this process the confirmed *sole* owner of the socket,
        // and every supervised relaunch (launchd `KeepAlive:SuccessfulExit`,
        // systemd `Restart=on-success`, the #4054 restart primitive, the
        // in-daemon self-update loop, `launchctl kickstart`) reaches it —
        // whereas none of them re-run `loom-daemon-start.sh`, which was the only
        // writer before now. Deliberately NOT hoisted next to #4331's marker
        // healing in `daemon_service.rs`: that call runs *before* the singleton
        // guard, so a daemon about to be refused would stomp the live
        // incumbent's pid file with its own doomed pid. Non-fatal in every
        // branch — a daemon without a pid file beats no daemon.
        match crate::daemon_pidfile::claim_for_current_process() {
            crate::daemon_pidfile::ClaimOutcome::Claimed {
                path,
                previous: Some(previous),
            } if previous != std::process::id() => log::warn!(
                "daemon_pidfile: pid file {} named stale pid {previous} — rewrote it to this \
                 daemon's pid {} (a supervisor relaunch does not re-run loom-daemon-start.sh, \
                 #4774)",
                path.display(),
                std::process::id()
            ),
            crate::daemon_pidfile::ClaimOutcome::Claimed { path, .. } => log::info!(
                "daemon_pidfile: claimed {} for pid {} (#4774)",
                path.display(),
                std::process::id()
            ),
            crate::daemon_pidfile::ClaimOutcome::Unresolvable => log::warn!(
                "daemon_pidfile: could not resolve a pid file path (no LOOM_PID_FILE, \
                 LOOM_WORKSPACE, LOOM_SOCKET_PATH, or home directory) — liveness cross-checks \
                 that consult it will have no signal (#4774)"
            ),
            crate::daemon_pidfile::ClaimOutcome::WriteFailed { path, error } => log::warn!(
                "daemon_pidfile: could not write {} — {error}. Continuing without a self-written \
                 pid file (#4774)",
                path.display()
            ),
        }

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
            // Build the report under a panic guard (#4279). This connection runs
            // in a detached `tokio::spawn` (see the accept loop): a panic while
            // building the status would unwind the task and drop the socket with
            // ZERO bytes written, so the client reads a silent EOF that a
            // stdout-capturing monitor misreads as an empty/"no workspaces"
            // status. The registry-lock poisoning that used to cause exactly this
            // is now recovered in `build_daemon_status`, but the guard makes the
            // invariant unconditional: a `DaemonStatus` request always leaves the
            // handler having written either the report or an explicit error frame
            // (the daemon logs the panic cause either way). `build_daemon_status`
            // is synchronous, so `catch_unwind` never spans an `.await`.
            let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                build_daemon_status_with_drain(
                    &workspace_pool,
                    &health_states,
                    &fallback_root,
                    &credential_preflight,
                    &drain_state,
                )
            }));
            let response = match built {
                // `Response::DaemonStatus` is boxed (issue #4292) to keep the
                // enum small; box the guarded report here.
                Ok(report) => Response::DaemonStatus(Box::new(report)),
                Err(panic) => {
                    let cause = describe_panic(panic.as_ref());
                    log::error!(
                        "DaemonStatus handler panicked while building the report: {cause}; \
                         replying with an error frame instead of dropping the connection"
                    );
                    Response::Error {
                        message: format!("daemon failed to build status report: {cause}"),
                    }
                }
            };
            let response_json = serde_json::to_string(&response)?;
            writer.write_all(response_json.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            continue;
        }

        // DispatchSweep drain-pause admission gate (Issue #5340). Handled here,
        // before the synchronous `handle_request` dispatcher below, because
        // `handle_request` never receives `drain_state` (see
        // `drain_dispatch_refusal`'s doc comment for why this gap existed and
        // why it matters — the work-finder/epic-supervisor/role-runner
        // producers all already pause on the same flag in-process, but
        // explicit `dispatch_sweep` calls did not). A refusal here short-circuits
        // before `handle_request` runs, so the request never reaches the
        // registry/headroom/model-resolution machinery at all.
        if let Request::DispatchSweep { kind, force, .. } = &request {
            if let Some(refusal) = drain_dispatch_refusal(kind, drain_state.is_draining(), *force) {
                let response_json = serde_json::to_string(&refusal)?;
                writer.write_all(response_json.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                continue;
            }
        }

        // DispatchSweep (Issue #6592) is handled here rather than in the
        // synchronous `handle_request` dispatcher below, for the same reason
        // `CancelSweep` is (Issue #3807, see `cancel_sweep_nonblocking`'s doc
        // comment): `SweepRegistry::dispatch_inner` holds the registry mutex
        // across the ENTIRE guard chain + child spawn + the child's
        // account-selection poll (up to `TOKEN_NAME_CAPTURE_TIMEOUT`, ~5s) —
        // a burst of concurrent `dispatch_sweep` calls serializes behind each
        // other's poll wait, blowing the client's 30s ack deadline even
        // though the daemon is healthy, and starves unrelated requests
        // (`ListSweeps`) on the same mutex. `dispatch_sweep_nonblocking`
        // drives the SweepRegistry's `begin_issue_dispatch` ->
        // `poll_and_classify_spawned_child` -> `finish_issue_dispatch` split
        // instead, releasing the registry mutex for the poll (run via
        // `spawn_blocking` so it never occupies a tokio worker thread either).
        if let Request::DispatchSweep {
            kind,
            idempotency_key,
            model,
            effort,
            depends_on,
            workspace_root,
            force,
        } = request
        {
            let response = dispatch_sweep_nonblocking(
                &sweep_registry,
                &workspace_pool,
                &event_bus,
                kind,
                idempotency_key,
                model,
                effort,
                depends_on,
                workspace_root,
                force,
            )
            .await;
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
            then_exit,
        } = request
        {
            let response = handle_drain_request(
                &drain_state,
                &workspace_pool,
                &fallback_root,
                &event_bus,
                timeout_secs,
                force_after_timeout,
                then_exit,
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
                then_exit: false,
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
        // for a supervised (e.g. launchd `KeepAlive:SuccessfulExit`) relaunch.
        // On an unsupervised host it returns `DaemonRestart { scheduled: false }`
        // and keeps running (do_exit == false). Mirrors the
        // CancelSweep/DaemonStatus interception.
        if let Request::RestartDaemon = request {
            // #5119: the count is read BEFORE the decision so the ack can state
            // plainly what this exit is about to do to that work (destroyed on
            // systemd's cgroup-scoped stop job, preserved on launchd).
            let in_flight = count_in_flight_sweeps(&workspace_pool, &fallback_root);
            let (response, do_exit) = build_restart_decision(in_flight);
            let response_json = serde_json::to_string(&response)?;
            writer.write_all(response_json.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
            if do_exit {
                let sup = match &response {
                    Response::DaemonRestart {
                        supervisor: Some(s),
                        ..
                    } => s.clone(),
                    _ => "supervisor".to_string(),
                };
                // The journal line carries the SAME supervisor-specific wording
                // as the ack (#5119) — a journal that says "sweeps survive" on a
                // host where the cgroup was just reaped is how the 2026-08-03
                // incident stayed invisible for four minutes. Reusing the ack's
                // own `message` (rather than re-deriving a parallel phrasing)
                // makes divergence between the two structurally impossible.
                let ack_message = match &response {
                    Response::DaemonRestart { message, .. } => message.clone(),
                    _ => restart_scheduled_message(&sup, in_flight),
                };
                log::warn!(
                    "RestartDaemon: supervised — exiting {EXIT_RESTART}. {ack_message} \
                     The stale socket is reclaimed by the relaunched daemon's singleton guard."
                );
                crate::observability::shutdown::exit(EXIT_RESTART).await;
            }
            continue;
        }

        crate::observability::shutdown::intercept(&request).await;
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
// A poisoned registry mutex (another thread panicked while holding the lock) is
// recovered rather than crashed (#4279): a single prior panic must not turn every
// subsequent cancel/status into a permanent server-side failure.
async fn cancel_sweep_nonblocking(
    sweep_registry: &Arc<Mutex<SweepRegistry>>,
    sweep_id: &str,
    grace: Duration,
) -> Response {
    // Step 1: begin (lock-scoped). Read state + SIGTERM, then release.
    let began = {
        let mut sr = sweep_registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sr.begin_cancel(sweep_id, grace)
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
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sr.poll_cancel(sweep_id, pid)
    };
    while !exited_within_grace && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(poll_interval).await;
        exited_within_grace = {
            let mut sr = sweep_registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            sr.poll_cancel(sweep_id, pid)
        };
    }

    // Step 3: finish (lock-scoped). SIGKILL decision + reap + terminal
    // transition + event emission.
    let outcome = {
        let mut sr = sweep_registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sr.finish_cancel(sweep_id, pid, &kind, started_at, exited_within_grace)
    };
    Response::SweepCancelled {
        sweep_id: outcome.sweep_id,
        pid: outcome.pid,
        sigkill_sent: outcome.sigkill_sent,
        was_running: outcome.was_running,
    }
}

/// Non-blocking `DispatchSweep` orchestration (Issue #6592), mirroring
/// [`cancel_sweep_nonblocking`] immediately above. Drives
/// `SweepRegistry::begin_issue_dispatch` -> `poll_and_classify_spawned_child`
/// -> `SweepRegistry::finish_issue_dispatch`, releasing the registry mutex
/// for the middle (poll) step — the one genuinely multi-second wait in the
/// whole dispatch path (bounded by `TOKEN_NAME_CAPTURE_TIMEOUT`, up to 5s) —
/// and running that poll on a `spawn_blocking` thread so it never occupies a
/// tokio async worker either. A burst of concurrent `dispatch_sweep` calls
/// therefore no longer serializes behind each other's poll wait, and a
/// concurrent `ListSweeps`/`GetSweepStatus` is not starved on the same mutex
/// for that duration.
///
/// The breaker checks, registry resolution, headroom advisory, and model
/// resolution ahead of the split are unchanged from the previous
/// `handle_request` arm (still lock-scoped exactly as before — none of them
/// is the hazard this issue targets; see the module doc above
/// `assess_dispatch_headroom` for why they are safe to hold the lock
/// through). `PrSet` dispatch has no long poll to split around
/// ([`SweepRegistry::dispatch_prset_inner`]'s doc comment) and is fully
/// handled inside `begin_issue_dispatch`, returned as `BeginIssueDispatch::Done`
/// — unchanged behavior for that kind.
#[allow(clippy::too_many_arguments)]
async fn dispatch_sweep_nonblocking(
    sweep_registry: &Arc<Mutex<SweepRegistry>>,
    workspace_pool: &Arc<WorkspacePool>,
    event_bus: &Arc<EventBus>,
    kind: crate::types::SweepKind,
    idempotency_key: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    depends_on: Option<u32>,
    workspace_root: Option<String>,
    force: bool,
) -> Response {
    // Host-distress circuit breaker (#4235) — unchanged from the previous
    // synchronous arm; a pure, lock-free global snapshot read.
    if !force {
        if let Some(snap) = crate::host_breaker::global_snapshot() {
            if snap.suppressed {
                let releases = snap.releases_at.map_or_else(
                    || " (host still hot — cool-down not yet started)".to_string(),
                    |r| format!(" (cool-down releases at {r})"),
                );
                log::warn!(
                    "dispatch_sweep: refused {kind:?} — host circuit breaker is {} \
                     ({}){releases}; running work drains, new dispatch paused. \
                     Re-run with force to override.",
                    snap.phase.as_str(),
                    snap.reason.as_deref().unwrap_or("sustained host distress"),
                );
                return Response::Error {
                    message: format!(
                        "dispatch_sweep refused: host circuit breaker is {} ({}).{releases} \
                         Running work is draining and new dispatch is paused (#4235). \
                         Re-run with force to override.",
                        snap.phase.as_str(),
                        snap.reason.as_deref().unwrap_or("sustained host distress"),
                    ),
                };
            }
        }
    }
    // GitHub rate-limit circuit breaker (#4429/#4440/#4666) — unchanged.
    if let Some(refusal) = rate_limit_dispatch_refusal(
        &kind,
        crate::rate_limit_breaker::global_snapshot().as_ref(),
        force,
    ) {
        return refusal;
    }
    // Dispatch-only resolution (Issue #4299) — unchanged.
    let target = match resolve_dispatch_registry(
        sweep_registry,
        workspace_pool,
        workspace_root.as_deref(),
    ) {
        Ok(target) => target,
        Err(response) => return response,
    };

    // Phase 1 (lock-scoped): headroom advisory + model resolution (both
    // unchanged from the previous arm) + `begin_issue_dispatch` — the FULL
    // guard chain, claim lock, label flip, dispatch stagger, and
    // `Command::spawn()`. Everything here is either cheap in-memory/local-fs
    // work or (for `Issue` guards) the SAME `gh` round trips the previous
    // single-call `dispatch()` already made under this same lock — this
    // split changes WHEN the lock is released, not what runs under it up to
    // this point.
    let begin_outcome = {
        let mut sr = target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let repo_root = sr.config().workspace_root.clone();

        let headroom = assess_dispatch_headroom(&mut sr, &repo_root);
        let low_headroom = dispatch_would_meet_or_exceed_headroom(&headroom);
        emit_dispatch_headroom_advisory_on_change(
            event_bus,
            &repo_root,
            low_headroom,
            &headroom,
            &kind,
        );

        log::info!(
            "dispatch_sweep: {:?}; \
             headroom occupancy={} dynamic_cap={} (disk={} ram={} tokens={} [informational \
             only, not capacity-limiting since #5270])",
            kind,
            headroom.occupancy,
            headroom.dynamic_cap,
            headroom.disk_headroom,
            headroom.ram_headroom,
            headroom.token_axis_limit
        );

        sr.begin_issue_dispatch_with_model(
            &kind,
            idempotency_key,
            crate::sweep_registry::DispatchModel::Request(model.as_deref()),
            effort.as_deref(),
            depends_on,
            None,
        )
    };

    let prepared = match begin_outcome {
        Err(e) => return dispatch_error_response(&kind, e),
        Ok(BeginIssueDispatch::Done(result)) => return dispatch_result_to_response(&kind, result),
        Ok(BeginIssueDispatch::Spawned(prepared)) => prepared,
    };

    // Phase 2 (UNLOCKED): poll the child for its account-selection log line.
    // Run via `spawn_blocking` — `poll_and_classify_spawned_child` calls
    // `std::thread::sleep` internally (bounded by `TOKEN_NAME_CAPTURE_TIMEOUT`,
    // up to 5s) and must never run inline on a tokio async worker thread.
    let poll_result = tokio::task::spawn_blocking(move || {
        let mut prepared = prepared;
        let (token_name, runtime, immediate_preflight_death) = poll_and_classify_spawned_child(
            &mut prepared.child,
            &prepared.log_path,
            &prepared.header_anchor,
        );
        (prepared, token_name, runtime, immediate_preflight_death)
    })
    .await;
    let (prepared, token_name, runtime, immediate_preflight_death) = match poll_result {
        Ok(result) => result,
        Err(join_err) => {
            // Extremely unlikely (a panic inside the poll) — never silently
            // drop the ack. The spawned child is orphaned (no `self.children`
            // entry was ever recorded — `finish_issue_dispatch` never ran),
            // so the reaper's later journal/`/proc` scan is the recovery
            // path (Issue #3953), matching how a `spawn_child` panic would
            // have been handled pre-split.
            log::error!("dispatch_sweep: poll task for {kind:?} panicked: {join_err}");
            return Response::Error {
                message: format!(
                    "dispatch_sweep failed: account-selection poll panicked: {join_err}"
                ),
            };
        }
    };

    // Phase 3 (lock-scoped): record the outcome.
    let result = {
        let mut sr = target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sr.finish_issue_dispatch(*prepared, token_name, runtime, immediate_preflight_death)
    };
    dispatch_result_to_response(&kind, result)
}

/// Shared `Result<DispatchOutcome> -> Response` mapping for `DispatchSweep`
/// (Issue #6592) — used by both `dispatch_sweep_nonblocking` and (via
/// `dispatch_error_response`) its `begin_issue_dispatch` error path. Mirrors
/// the mapping the previous single `handle_request` arm inlined.
fn dispatch_result_to_response(
    kind: &crate::types::SweepKind,
    result: Result<crate::sweep_registry::DispatchOutcome>,
) -> Response {
    match result {
        Ok(outcome) => Response::SweepDispatched {
            sweep_id: outcome.sweep_id,
            pid: outcome.pid,
            token_name: outcome.token_name,
            log_path: outcome.log_path,
        },
        Err(e) => dispatch_error_response(kind, e),
    }
}

/// `anyhow::Error -> Response` mapping for a failed `DispatchSweep`, shared
/// by `dispatch_result_to_response`'s `Err` arm and `begin_issue_dispatch`'s
/// direct `Err` return (a guard refusal before any child was ever spawned).
fn dispatch_error_response(kind: &crate::types::SweepKind, e: anyhow::Error) -> Response {
    match e.downcast::<crate::runtime_admission::RuntimeRejection>() {
        Ok(rejection) => Response::RuntimeRejected(rejection),
        Err(e) => {
            // Issue #5236/#5210: log the full error chain at WARN (not just
            // the caller-facing response) so an operator reading the
            // daemon's own log can diagnose a dispatch failure without
            // reproducing it.
            log::warn!("dispatch_sweep: {kind:?} failed: {e:#}");
            Response::Error {
                message: format!("dispatch_sweep failed: {e:#}"),
            }
        }
    }
}

/// Extract a human-readable message from a caught panic payload (#4279). Panic
/// payloads are almost always `&str` (from `panic!("literal")`) or `String`
/// (from `panic!("{}", x)`); anything else is reported generically.
fn describe_panic(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

/// Build the autonomous-mode operability snapshot for a `DaemonStatus` request
/// (Issue #3891 — follow-up to #3813 Phase D).
///
/// Combines a live registry snapshot (in-flight = non-terminal sweeps) with the
/// three dynamic-cap inputs recomputed from the workspace (disk headroom, ram
/// headroom, configured ceiling) and the shared main-health-gate halt flag. The
/// `min` of the three inputs is the effective dynamic cap the work finder would
/// use on its next tick.
///
/// Per-token usage is intentionally excluded — probing each account for
/// rate-limit headers is a slow network call the CLI performs client-side (via
/// `loom-tokens check --json`), so this handler stays non-blocking.
///
/// A poisoned registry mutex is recovered (`PoisonError::into_inner`) rather than
/// crashed (#4279). Before this, a panic anywhere under the registry lock poisoned
/// it permanently and every subsequent `status` call panicked in the detached
/// per-connection task, dropping the socket with zero bytes written — the client
/// saw a silent EOF. Recovering the guard keeps `status` answerable after any such
/// fault.
///
/// # Phase timing (#7513) and the budget it is judged against (#8163)
///
/// Fleet reports showed `status`/`health` IPC round-trips exceeding the
/// client budget on hosts with a large registered-workspace count, with the
/// per-workspace work in the loop below (registry lock/list, per-root config
/// reads, the `role_shard::decide` walk, token-pool/ranking file reads, and a
/// `git stash list` shell-out per root) as the prime suspect — but no
/// daemon-internal timing existed to confirm *which* of those actually
/// dominates on a given host. This function accumulates wall-clock time per
/// named phase, summed across every root, and hands the whole breakdown to
/// [`crate::status_budget::record_status_build`] — so the next report can
/// name the slow phase (and, if it is dominated by one repo rather than being
/// spread evenly, the single slowest root) instead of guessing.
///
/// #8163: the budget that breakdown is compared against is no longer a
/// constant. This build is `O(roots)`, so both the daemon-side target and the
/// `health` client's probe budget derive from the registered root count —
/// see [`crate::status_budget`] for the one shared cost model.
pub fn build_daemon_status(
    workspace_pool: &Arc<WorkspacePool>,
    health_states: &WorkspaceHealthStates,
    fallback_root: &Path,
    credential_preflight: &CredentialPreflightReport,
) -> DaemonStatusReport {
    // #7513: whole-build + per-phase timers. `phase_*` accumulators are
    // summed ACROSS every root in the loop below (the loop runs once per
    // registered workspace, so these answer "where did the N-root total go",
    // not "how long did any single root take" — `slowest_root` below answers
    // that second question). All are logged together, once, only when the
    // whole build crosses `STATUS_BUILD_SLOW_LOG_THRESHOLD` — a fast build
    // never pays for `Instant::now()` bookkeeping to be logged, only to be
    // computed (a handful of nanosecond-cost syscalls, negligible next to the
    // work being timed).
    let build_started = Instant::now();

    // Enumerate every registered managed workspace (Issue #3930). An empty
    // registry yields `[fallback_root]`, so the common single-workspace case is
    // byte-for-byte the pre-#3930 behavior (one root — the daemon's own).
    let phase_start = Instant::now();
    let workspace_registry = WorkspaceRegistry::load_default().unwrap_or_default();
    let roots = workspace_registry.effective_roots(fallback_root);
    let phase_registry_load = phase_start.elapsed();

    // Autonomous self-update loop snapshot (#4055), read once from the
    // process-global the loop publishes to. Default (disabled/never-checked)
    // when the loop was never spawned.
    let au = crate::auto_update::global_status_snapshot();

    // Per-repo breakdown + the union of in-flight sweeps across every repo. Each
    // root reads its own registry from the pool (the fallback/default root
    // resolves to the seeded default registry, so a single-workspace daemon reads
    // exactly the same registry it did pre-#3930).
    let mut in_flight: Vec<crate::types::SweepInfo> = Vec::new();
    let mut per_repo: Vec<crate::types::RepoStatus> = Vec::with_capacity(roots.len());
    // Issue #4214: live-locked-but-unregistered sweeps, unioned across every
    // root exactly like `in_flight` — each root is cross-checked against its
    // own `.loom/locks/issue-*/` independently. Issue #5342: a `PrSet` sweep
    // holds `.loom/locks/pr-<N>/` locks instead (a deliberately separate
    // namespace — see `SweepRegistry::pr_lock_dir`'s doc comment), so it is
    // still never a candidate here; this cross-check remains `Issue`-only.
    let mut unregistered_locked: Vec<crate::types::UnregisteredLockedSweep> = Vec::new();
    // #7513: per-phase timing accumulators, summed across every root in the
    // loop below — see the doc comment above this function.
    let mut phase_registry_lock = Duration::ZERO;
    let mut phase_role_runner_config = Duration::ZERO;
    let mut phase_role_shard = Duration::ZERO;
    let mut phase_token_pool = Duration::ZERO;
    let mut phase_stash_git_shellout = Duration::ZERO;
    let mut phase_sweep_command_check = Duration::ZERO;
    // The single slowest root's own total loop-body time + which root it was
    // — distinguishes "every root is uniformly a little slow" (raise the
    // per-phase totals above) from "one repo is pathologically slow" (name
    // it, e.g. an oversized `refs/stash` reflog or an NFS-mounted checkout).
    let mut slowest_root: Option<(std::path::PathBuf, Duration)> = None;
    let mut stale_sweeps: Vec<crate::types::StaleSweepFinding> = Vec::new();
    for root in &roots {
        let root_loop_start = Instant::now();
        let phase_start = Instant::now();
        let registry = workspace_pool.get_or_provision(root);
        // Stale-untracked-sweep backstop inputs (Issue #7529): resolved from
        // this root's OWN `.loom/config.json`, mirroring every other
        // per-root watchdog knob resolved in this loop (role-runner enablement
        // just below is the same pattern). Read outside the registry lock —
        // it's a config file read, not a registry operation.
        let stale_sweep_config = crate::sweep_registry::read_startup_race_config(root);
        let stale_sweep_min_age =
            crate::sweep_registry::resolve_stale_sweep_age(&stale_sweep_config);
        let stale_sweep_log_silence =
            crate::sweep_registry::resolve_review_stall_timeout(&stale_sweep_config);
        // #7526: take a read snapshot of the registry's raw state — clone the
        // in-memory entries/children/config paths while the lock is held (no
        // filesystem I/O, so this is fast regardless of contention), then
        // drop the guard immediately and compute everything below from the
        // owned snapshot. Before this, the four calls below (`list`'s
        // live-phase checkpoint-read overlay, `stale_sweep_findings`'s
        // `kill(pid, 0)` probe + log `stat`, `unregistered_locked_issues`'s
        // `.loom/locks/` directory walk + `owner.json` reads) all ran while
        // still holding this root's registry mutex — real I/O serialized by
        // the same lock every dispatch/reap/status call on this root
        // contends for. See `SweepRegistry::snapshot`'s doc comment for the
        // full rationale and the live fleet measurement that motivated it.
        let snapshot = {
            let sr = registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            sr.snapshot()
        };
        // In-flight = sweeps still live (Pending / Running). Terminal sweeps
        // (Exited / Crashed) linger in the registry but are not "in flight".
        let live: Vec<crate::types::SweepInfo> = snapshot
            .list(None)
            .into_iter()
            .filter(|info| !info.state.is_terminal())
            .collect();
        // Insta-crash quarantine (#3939): surface which issues this repo is
        // currently refusing to re-dispatch, so a repo with a visible backlog
        // that is dispatching nothing is explained.
        let root_stale: Vec<crate::sweep_registry::StaleSweepFinding> =
            if crate::sweep_registry::resolve_stale_sweep_enabled(&stale_sweep_config) {
                snapshot.stale_sweep_findings(stale_sweep_min_age, stale_sweep_log_silence)
            } else {
                Vec::new()
            };
        let quarantined_issues = snapshot.quarantined_issues_sorted();
        let locked_unregistered = snapshot.unregistered_locked_issues();
        stale_sweeps.extend(
            root_stale
                .into_iter()
                .map(|f| crate::types::StaleSweepFinding {
                    root: root.clone(),
                    issue: f.issue,
                    sweep_id: f.sweep_id,
                    pid: f.pid,
                    elapsed_secs: f.elapsed.as_secs(),
                    log_idle_secs: f.log_idle.map(|d| d.as_secs()),
                }),
        );
        phase_registry_lock += phase_start.elapsed();
        let phase_start = Instant::now();
        // Per-root role-runner enablement (#4377): resolved from this root's
        // OWN `.loom/config.json`, never the daemon workspace's — the whole
        // point of this status surface is that the two can legitimately
        // differ (a registered workspace can be role-runner-disabled while
        // the daemon's own workspace is enabled, or vice versa).
        let role_runner_config = crate::role_runner::read_role_runner_config(root);
        let (role_runner_enabled, role_runner_enabled_source) =
            crate::role_runner::resolve_enabled_with_source(&role_runner_config);
        // Which tier decided (#6470): `Some(v)` only when the host-wide
        // `LOOM_ROLE_RUNNER` env override is what resolved this root's state
        // — the case the #4377 per-root message used to misreport as "this
        // root's own config". `None` for both `Config` and `Default`
        // sources, which the pre-existing #4377 message already names
        // correctly.
        let role_runner_env_override =
            matches!(role_runner_enabled_source, crate::role_runner::EnabledSource::Env)
                .then_some(role_runner_enabled);
        let resolved_role_specs = crate::role_runner::resolve_roles(&role_runner_config);
        let role_runner_roles = resolved_role_specs
            .iter()
            .map(|spec| spec.name.to_string())
            .collect();
        // Issue #7238: the ACTUALLY resolved per-role tick interval (env >
        // config > that role's own built-in default), not the bare built-in
        // `RoleSpec::default_interval_secs` — so `assess_role_liveness` can
        // compare a role's silence against the cadence the role runner
        // itself will actually use, not an unconfigured default.
        let role_runner_intervals = resolved_role_specs
            .iter()
            .map(|spec| {
                (
                    spec.name.to_string(),
                    crate::role_runner::resolve_interval_for_role(spec, &role_runner_config)
                        .as_secs(),
                )
            })
            .collect();
        let role_runner_on_idle_roles =
            crate::role_runner::resolve_on_idle_roles(&role_runner_config)
                .iter()
                .map(|spec| spec.name.to_string())
                .collect();
        // Issue #7511: per-role `onIdleMaxWait` age/promoted status — one
        // entry per role that is both in `onIdle` and has a configured
        // max-wait, empty (not an error) when the key is unconfigured.
        let role_runner_on_idle_promotions = crate::role_runner::resolve_on_idle_max_wait_status(
            &role_runner_config,
            root,
            chrono::Utc::now(),
        );
        phase_role_runner_config += phase_start.elapsed();
        let phase_start = Instant::now();
        // This root's role-runner host-sharding verdict (#6374) — the same
        // `role_shard::decide` the role runner's own tick gate calls, so
        // status reports the decision that will actually be taken rather
        // than a re-derivation that could drift from it.
        let shard_decision = crate::role_shard::decide(root);
        let role_runner_shard = Some(crate::types::RoleRunnerShardStatus {
            owned_here: shard_decision.owned,
            key: shard_decision.key.key.clone(),
            key_source: shard_decision.key.source.label().to_string(),
            owning_shard: shard_decision.owning_shard,
            host_shard: shard_decision.posture.index(),
            shard_count: shard_decision.posture.count(),
        });
        phase_role_shard += phase_start.elapsed();
        let phase_start = Instant::now();
        // This root's OWN resolved token pool (#5269) — the unanchored
        // `resolve_tokens_dir(root)`, i.e. the exact resolution
        // `token_ranking_refresh.rs`'s self-refresh loop already uses to
        // decide which pool to keep fresh for this repo. Deliberately not
        // `resolve_tokens_dir_anchored`, which is scoped to the daemon's own
        // `fallback_root`/launch CWD, not this loop's `root` — the whole
        // point of this per-repo field is to answer "is THIS repo's own pool
        // fresh" regardless of which repo the daemon happened to start in.
        let repo_token_pool_dir = crate::tokens_pool::paths::resolve_tokens_dir(root);
        let (repo_ranking_present, repo_ranking_age_secs) =
            crate::capacity::ranking_file_state(&repo_token_pool_dir);
        phase_token_pool += phase_start.elapsed();
        let phase_start = Instant::now();
        // Fleet-wide quarantine-stash visibility (#5692): read from the
        // per-root cache [`crate::quarantine_stash_status::spawn_multi_stash_summary_refresh_task`]'s
        // background tick keeps warm, instead of shelling out to
        // `git stash list` synchronously on every `status`/`health` call
        // (Issue #7526). The #7525 instrumentation's live fleet data (quoted
        // in #7526's issue body) measured that shellout —
        // `phase_stash_git_shellout` below — as the dominant contributor to
        // `build_daemon_status` exceeding the fleet's 5s IPC budget on a
        // 50-root host (2.0s / 3.8s / 1.8s of three 1.9-5.3s slow builds), so
        // this phase is now a cache lookup: a mutex lock + `BTreeMap` get +
        // `Copy`, no I/O, no subprocess spawn. A root the cache has never
        // refreshed yet (freshly registered, or the daemon just started)
        // degrades to the zero-valued default rather than falling back to a
        // synchronous shellout — that fallback would reintroduce the exact
        // bottleneck this cache exists to remove; the background tick warms
        // it within one cycle.
        let repo_stash_summary = crate::quarantine_stash_status::cached_summary(root)
            .map(|cached| cached.summary)
            .unwrap_or_default();
        phase_stash_git_shellout += phase_start.elapsed();
        let phase_start = Instant::now();
        // Issue #5682: recomputed live (a cheap `stat`), not read once from the
        // registry — a root that had `.claude/commands/loom/sweep.md` at
        // `workspace add` time but lost it later (deleted, or a fresh clone
        // that never ran `init`) must still be caught on every snapshot, not
        // just at registration.
        let sweep_command_missing =
            !crate::sweep_registry::SweepRegistryConfig::new(root.clone()).has_sweep_command();
        phase_sweep_command_check += phase_start.elapsed();
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
            // Issue #4326: surface a dangling registry entry (root deleted
            // without `workspace remove`) so `status` — not just the
            // work-finder log — points the operator at it.
            root_missing: !root.is_dir(),
            // Load-aware deferral + tier label (#4259).
            health_gate_deferred: health_states.is_deferred(root),
            health_gate_deferred_reason: health_states.deferred_summary(root),
            health_gate_verdict_tier: health_states
                .gate_last_tier(root)
                .map(|t| t.label().to_string()),
            role_runner_enabled,
            role_runner_roles,
            role_runner_intervals,
            role_runner_on_idle_roles,
            role_runner_on_idle_promotions,
            role_runner_env_override,
            role_runner_shard,
            token_pool_dir: Some(repo_token_pool_dir),
            ranking_present: repo_ranking_present,
            ranking_age_secs: repo_ranking_age_secs,
            stash_total_count: repo_stash_summary.total_count,
            stash_quarantine_count: repo_stash_summary.quarantine_count,
            stash_oldest_age_secs: repo_stash_summary.oldest_stash_age_secs,
            stash_non_quarantine_unrecoverable_count: repo_stash_summary
                .non_quarantine_unrecoverable_count,
            stash_non_quarantine_unrecoverable_oldest_age_secs: repo_stash_summary
                .non_quarantine_unrecoverable_oldest_age_secs,
            sweep_command_missing,
        });
        in_flight.extend(live);
        unregistered_locked.extend(locked_unregistered.into_iter().map(|(issue, owner_pid)| {
            crate::types::UnregisteredLockedSweep {
                root: root.clone(),
                issue,
                owner_pid,
            }
        }));
        let root_loop_elapsed = root_loop_start.elapsed();
        if slowest_root
            .as_ref()
            .is_none_or(|(_, d)| root_loop_elapsed > *d)
        {
            slowest_root = Some((root.clone(), root_loop_elapsed));
        }
    }
    let phase_per_repo_loop_total = phase_registry_lock
        + phase_role_runner_config
        + phase_role_shard
        + phase_token_pool
        + phase_stash_git_shellout
        + phase_sweep_command_check;

    // Present the per-repo breakdown in dispatch-priority order (#3946) — the
    // same order the autonomous loops drain — so the highest-priority repos are
    // listed first. Stable within a tier (tiebreak on root path) for determinism.
    per_repo.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then_with(|| a.root.cmp(&b.root))
    });
    // #7513: everything from here to the report literal is machine-level
    // (computed once, not per root) — timed as one "tail" phase so the
    // slow-build log below can rule it in or out as a contributor distinct
    // from the per-root loop above.
    let phase_start = Instant::now();

    // Dynamic-cap inputs are *machine-level* (one token pool, one scratch
    // volume), so they are computed once from the daemon's primary workspace —
    // the same basis as pre-#3930 (which read them from the default registry's
    // `workspace_root`, i.e. `fallback_root`).
    let workspace_root = fallback_root;
    // Registry-aware anchoring (#4292, trip-wire 1): `workspace_root` is the
    // daemon's own seeded default (its cwd at startup, or `LOOM_WORKSPACE`),
    // which for a machine-level daemon started under systemd with a bare cwd
    // (e.g. `$HOME`) is not itself a real repo checkout. `workspace_registry`
    // is already loaded above for `effective_roots`, so this reuses it rather
    // than a second registry read.
    let tokens_dir =
        crate::tokens_pool::paths::resolve_tokens_dir_anchored(workspace_root, &workspace_registry);
    let token_pool_size = crate::tokens::token_pool_size_at_dir(&tokens_dir);
    // Exposed on the report (#4292) so a client reading `status` from any cwd
    // sees exactly which directory the daemon used rather than silently
    // re-resolving a possibly-different one.
    let token_pool_dir = Some(tokens_dir.clone());
    let disk_headroom = crate::disk_headroom::disk_headroom_limit(workspace_root);
    // RAM headroom (#5270): the second "dumb mode" machine-headroom axis,
    // folded into `dynamic_cap` alongside disk headroom.
    let ram_headroom = crate::ram_headroom::ram_headroom_limit();
    let wf_config = crate::work_finder::read_work_finder_config(workspace_root);
    let configured_max = crate::work_finder::resolve_max_concurrent_with_config(&wf_config);
    // Host CPU **observations** (#3978, measured-idle signal #4031). Since #4512
    // these no longer feed the cap — they are reported so an operator can see
    // whether this machine's `maxConcurrent` leaves it idle or saturated. Never
    // blocks: the idle fraction is the memoized sample (the caller pre-warms it
    // via `spawn_blocking(refresh_cpu_util_cache)` before invoking
    // `build_daemon_status`), plus a fast fresh loadavg read.
    let logical_cpus = crate::cpu_headroom::logical_cpu_count();
    let loadavg_1m = crate::cpu_headroom::read_loadavg_1m();
    let cpu_idle_fraction = crate::cpu_headroom::cached_cpu_idle_fraction();

    // Token-capacity backpressure (#3902): back the token axis off from the flat
    // pool count toward the count of *healthy* accounts read from the rotation
    // ranking. When no ranking exists, `token_axis_limit` == the raw pool size,
    // so the dynamic cap is byte-for-byte the pre-#3902 value.
    let ranking = crate::capacity::read_ranking_at(&tokens_dir);
    let token_axis_limit = ranking.as_ref().map_or(token_pool_size, |r| r.available);
    let dynamic_cap = crate::work_finder::resolve_dynamic_max_concurrent(
        disk_headroom,
        ram_headroom,
        configured_max,
    );
    // The token axis no longer bounds the concurrency cap (#5270) —
    // `token_bound` here does NOT mean "tokens are the binding cap term"; it
    // means genuine starvation (zero healthy accounts to select from at
    // spawn time). `token_axis_limit` remains on the report as an
    // informational account-health figure (it still drives spawn-time
    // *selection*), but it does not gate admission any more (#5305: restoring
    // this as a reachable zero-healthy check, rather than a hardcoded
    // `false`, so `status_render.rs`'s add-accounts guidance branch can fire
    // again).
    let token_bound = token_axis_limit == 0;
    // "Currently binding" vs "smallest ceiling" (#4031): the dynamic cap is the
    // minimum of several ceilings, but a ceiling only *binds* once in-flight
    // occupancy reaches it. Below the cap the limiter is work availability, not
    // any resource term — so gate the token-bound diagnosis on real occupancy.
    let capacity_bound = in_flight.len() >= dynamic_cap;
    // Claude-wrapper pre-flight-death tripwire (#4386), read from the
    // fallback/default workspace's own registry — mirrors the top-level
    // `main_health_gate_*` fields' fallback-root scoping above/below.
    let (preflight_advisory_active, preflight_advisory_message, preflight_advisory_changed_at) = {
        let registry = workspace_pool.get_or_provision(fallback_root);
        let sr = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (active, message) = sr.preflight_advisory();
        (active, message, sr.preflight_advisory_changed_at())
    };
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

    let report = DaemonStatusReport {
        in_flight,
        unregistered_locked,
        stale_sweeps,
        token_pool_size,
        token_pool_dir,
        disk_headroom,
        ram_headroom,
        logical_cpus,
        loadavg_1m,
        cpu_idle_fraction,
        capacity_bound,
        preflight_advisory_active,
        preflight_advisory_message,
        preflight_advisory_changed_at,
        configured_max,
        dynamic_cap,
        // Top-level halt preserves its pre-#3930 single-workspace meaning: the
        // daemon's own primary workspace. Per-repo halt is in `per_repo`.
        main_health_gate_halted: health_states.is_halted(fallback_root),
        main_health_gate_not_evaluated: health_states.is_unevaluated(fallback_root),
        main_health_gate_not_evaluated_reason: health_states.unevaluated_summary(fallback_root),
        main_health_gate_enabled: Some(crate::main_health_gate::effective_enabled(fallback_root)),
        main_health_gate_verdict_at: health_states.last_verdict_at(fallback_root),
        // Load-aware deferral + tier label (#4259).
        main_health_gate_deferred: health_states.is_deferred(fallback_root),
        main_health_gate_deferred_reason: health_states.deferred_summary(fallback_root),
        main_health_gate_verdict_tier: health_states
            .gate_last_tier(fallback_root)
            .map(|t| t.label().to_string()),
        capacity,
        per_repo,
        // Host-wide env-override state (#6470), resolved once for the whole
        // report — independent of any single root's config, unlike the
        // per-root `role_runner_env_override` fields above.
        role_runner_host_env_override: crate::role_runner::host_env_override(),
        // Host-level sharding posture (#6374), resolved once for the whole
        // report from the daemon's own workspace root — the index/count knobs
        // describe the HOST, so a single resolution is the right shape even
        // though the config that carries them is read per-root (the same
        // asymmetry `autonomous.roleRunner.maxConcurrent` already has). The
        // per-root `role_runner_shard` fields above carry the actual
        // per-workspace verdicts.
        role_runner_shard: {
            // `decide(...).posture` rather than `resolve_posture(...)` so that
            // with roster mode on (#7691) the header reports the ring the
            // fence actually produced — "shard 1 of 3 (index from roster,
            // count from roster)" — instead of a static posture no tick uses.
            // With the roster off (the default) `decide`'s posture IS
            // `resolve_posture`'s, so this is byte-identical to pre-#7691.
            let decision = crate::role_shard::decide(fallback_root);
            let posture = decision.posture;
            let roster = roster_status::roster_status(&decision.roster);
            Some(crate::types::RoleRunnerShardPosture {
                index: posture.index(),
                count: posture.count(),
                summary: posture.describe(),
                configured: posture.is_configured(),
                roster,
            })
        },
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
        // Autonomous self-update loop status (#4055) — read from the
        // process-global snapshot the loop publishes each tick. The loop is
        // process-global (exactly one per daemon, never a per-workspace
        // fan-out), so unlike the drain fields there is no per-connection Arc to
        // thread; an unset global (loop never spawned) reads as the default
        // "disabled, never checked" snapshot.
        auto_update_enabled: au.enabled,
        auto_update_last_check: au.last_check,
        auto_update_last_roll: au.last_roll,
        auto_update_consecutive_failures: au.consecutive_failures,
        auto_update_backoff_secs: au.backoff_secs,
        auto_update_terminal_reason: au.terminal_reason,
        auto_update_note: au.note,
        auto_update_artifact_version: au.artifact_version,
        auto_update_artifact_published_at: au.artifact_published_at,
        auto_update_stale_repo_ticks: au.stale_repo_ticks,
        auto_update_stale_repo: au.stale_repo,
        // Host-distress circuit breaker (#4235) — read from the process-global
        // handle the work-finder loop registers/updates each tick, mirroring the
        // auto-update global-snapshot pattern above. `None` (no breaker
        // registered — work-finder off or breaker disabled) reads as "inactive".
        host_breaker: crate::host_breaker::global_snapshot()
            .map(crate::host_breaker::BreakerSnapshot::into_status)
            .map(Box::new),
        // Saturation admission brake (#4903) — same process-global snapshot
        // pattern as the host breaker above. This is the field that answers the
        // question `capacity_bound: false` could not: a host that is refusing new
        // work because it is already saturated now says so instead of reading as
        // idle with free slots.
        admission_brake: crate::admission_brake::global_snapshot()
            .map(crate::admission_brake::BrakeSnapshot::into_status),
        // GitHub rate-limit circuit breaker (#4429) — same process-global
        // snapshot pattern as the host breaker above.
        rate_limit_breaker: crate::rate_limit_breaker::global_snapshot()
            .map(crate::rate_limit_breaker::RateLimitSnapshot::into_status)
            .map(Box::new),
        // Observability host-identity mismatch (#4830) — same process-global
        // snapshot pattern again, registered only when the exporter actually
        // starts, so a disabled/keyless exporter always reads `None`.
        observability_host_id_mismatch: crate::observability::global_host_id_mismatch(),
        // Positive export-liveness signal (#5083) — the counterpart to the
        // anomaly-only field above. Always `Some` from a daemon of this
        // vintage: an exporter that never started reports `disabled`, which is
        // a real answer, not the silence #4830 alone could offer.
        observability_export: Some(crate::observability::global_export_status()),
        // Forge event plane (ADR-0021, Phase 1) — the same process-global
        // snapshot pattern. Always `Some` from a daemon of this vintage: a
        // disabled or misconfigured consumer still reports its state, which
        // is the real answer rather than the silence a missing field would be.
        forge_events: Some(crate::forge_events::global_status()),
        // Per-repo deep-clean state (#5919) — the same process-global snapshot
        // pattern once more, projected by the module that owns the state (the
        // mapping lived here until #7990 moved it beside `snapshot()`).
        deep_clean: crate::deep_clean::status_snapshot(),
        // Live idle-exit eligibility (#5565) — same process-global snapshot
        // pattern as the auto-update/host-breaker fields above. `enabled:
        // false, eligible: false` when the `autonomous.idleExit` task was
        // never spawned (feature disabled), never misread as "eligible".
        idle_exit: Some({
            let snap = crate::idle_exit::global_status_snapshot();
            crate::types::IdleExitStatus {
                enabled: snap.enabled,
                eligible: snap.eligible,
                trigger: snap
                    .trigger
                    .map(crate::idle_exit::IdleExitTrigger::as_str)
                    .map(str::to_string),
                idle_minutes: snap.idle_minutes,
                in_flight_sweeps: snap.in_flight_sweeps,
                active_role_runs: snap.active_role_runs,
                healthy_tokens: snap.healthy_tokens,
                total_tokens: snap.total_tokens,
                idle_elapsed_secs: snap.idle_elapsed_secs,
                starved_elapsed_secs: snap.starved_elapsed_secs,
                starvation_enabled: snap.starvation_enabled,
                observed_at: snap.observed_at,
            }
        }),
        // Worktree removals the periodic reaper has backed off after repeated
        // or permission-class failures (#7590) — same process-global
        // snapshot pattern as `deep_clean`/`idle_exit` above. Empty in the
        // overwhelmingly common case (nothing stuck).
        stuck_worktree_reclaims: crate::worktree_reaper::stuck_worktree_removals(),
        // Token pools holding ALL sweep dispatch because nothing in them can
        // spawn (#7708, surfaced by #7990). The hold lives in the daemon
        // process; `status`/`health` run as a separate CLI process, so the
        // only way they can name it is to ride this payload. Empty whenever
        // no pool is held, which is the steady state.
        pool_exhaustion_holds: crate::work_finder::pool_preflight::active_hold_statuses(),
        // Live safehouse connection state (#4345) — the pool's shared cell is
        // updated by the narration sink / peer-coordination tasks
        // `start_safehouse_narration`/`start_peer_coordination` spawn, and
        // read back here on every status call (no second connection).
        safehouse: Some(workspace_pool.safehouse_status()),
        // Peer-claim view + transport counters (Issue #5921) — same
        // process-global-cell read-back pattern as `safehouse` above. `None`
        // when peer-claim coordination was never established (see
        // `WorkspacePool::peer_claim_status`'s doc comment).
        peer_claims: workspace_pool.peer_claim_status(),
        // Whether the work-finder loop is enabled for THIS running daemon
        // process (#4693) — read from this process's own env/config, the same
        // `wf_config` already resolved above for the dynamic-cap fields, so it
        // costs no extra config read. Mirrors `main_health_gate_enabled`'s
        // `Some(resolve...)` shape.
        work_finder_enabled: Some(crate::work_finder::resolve_enabled(&wf_config)),
        // Last completed work-finder tick + the role-tick ring (#4761) — both
        // process-global slots the respective loops publish to each tick, read
        // back here for the same reason the auto-update/host-breaker snapshots
        // above are: they are the only cross-process view of *why* dispatch and
        // the role cadence are (or are not) making progress, and
        // `loom-daemon health` must not have to scrape the daemon log for them.
        // Unset globals (loop never spawned) read as `None` / empty — honestly
        // "no tick observed", never "nothing happened".
        last_work_finder_tick: crate::work_finder::last_tick_summary(),
        role_tick_records: crate::role_runner::role_tick_records(),
        // #6201: the never-evicted last-tick-per-pair companion to the ring
        // above — see `RoleLastTick`'s doc comment for why both are needed.
        role_last_tick: crate::role_runner::last_role_tick_snapshot(),
        // Live role-agent load + its ceiling (#6102). Same process-global
        // read-back shape as the ring above, and reported for the same reason:
        // `autonomous.workFinder.maxConcurrent` bounds sweep dispatch only, so
        // an operator reading "1 in flight, cap 8" off this report while the
        // host runs 11 agents had no in-band way to see the other ten. The
        // count is process-global (the host is shared); the ceiling is resolved
        // from the primary workspace's config, the same root every other
        // dynamic-cap field on this report is resolved against.
        active_role_agents: crate::role_runner::global_active_run_count(),
        role_agent_max_concurrent: Some(crate::role_runner::resolve_max_concurrent_for(
            workspace_root,
        )),
        // Restart-survivorship seed (#6262): how many still-running sweeps this
        // daemon had to adopt from the machine journal at startup because their
        // claim locks did not survive the restart. Process-global, set once by
        // `startup_adoption::seed_capacity_from_journal` before any dispatch
        // producer is spawned, so this is a stable startup fact rather than a
        // live sample.
        journal_adopted_at_startup: crate::startup_adoption::journal_adopted_at_startup(),
        // The answering process's own pid + the pid file it claimed at startup
        // (#4774). `std::process::id()` is deliberately taken HERE, in the
        // daemon, rather than inferred by the client from a file: it is the
        // only unforgeable statement of "who owns this socket", and it is what
        // lets `status`/`health` call a stale `.daemon.pid` stale instead of
        // trusting it. The path is re-resolved (not cached from the startup
        // claim) so the report always names the file *this* daemon's
        // environment points at.
        daemon_pid: Some(std::process::id()),
        pid_file: crate::daemon_pidfile::resolve_pid_file_path(),
        // The commit THIS daemon binary was built from, plus the tick interval
        // THIS process resolved (#4824). Both are taken daemon-side for the
        // same reason `daemon_pid` above is: they are statements only the
        // answering process can make truthfully. A client that knows both its
        // own `BUILT_COMMIT` and the daemon's can report CLI/daemon build skew
        // as its own condition instead of misreading an older daemon's absent
        // telemetry as a dead subsystem, and one that knows the daemon's real
        // cadence can size the post-restart grace window correctly instead of
        // assuming the 60s default.
        daemon_build_commit: Some(crate::self_update::BUILT_COMMIT.to_string()),
        // The running process's own build-time stamp (#5341), alongside its
        // commit above — see `DaemonStatusReport::daemon_built_at_raw` for why
        // this must be read daemon-side rather than re-derived by the CLI
        // process from the on-disk binary.
        daemon_built_at_raw: Some(crate::self_update::BUILT_AT_RAW.to_string()),
        work_finder_interval_secs: Some(
            crate::work_finder::resolve_interval_with_config(&wf_config).as_secs(),
        ),
    };

    // #7513/#8163: hand the phase breakdown to the shared cost model, which
    // owns both the one-time per-root INFO line and the slow-build WARN (and
    // names the root-scaled budget each is judged against). Logging policy
    // lives there, not here — see `crate::status_budget`.
    let phase_tail = phase_start.elapsed();
    crate::status_budget::record_status_build(
        build_started.elapsed(),
        roots.len(),
        &crate::status_budget::StatusBuildPhases {
            registry_load: phase_registry_load,
            per_repo_loop_total: phase_per_repo_loop_total,
            registry_lock: phase_registry_lock,
            role_runner_config: phase_role_runner_config,
            role_shard: phase_role_shard,
            token_pool: phase_token_pool,
            stash_git_shellout: phase_stash_git_shellout,
            sweep_command_check: phase_sweep_command_check,
            tail: phase_tail,
            slowest_root,
        },
    );

    report
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
/// Pure decision for the GitHub rate-limit circuit breaker gate on
/// `Request::DispatchSweep` (#4666). Given the request's `kind` (for the log
/// line) and the current rate-limit breaker snapshot — `None` when no
/// breaker is registered, mirroring the "zero behavior change when unset"
/// contract [`crate::rate_limit_breaker::global_snapshot`] documents — decide
/// whether the dispatch should be refused, and produce the refusal
/// [`Response`] if so.
///
/// Extracted as a pure function (rather than reading
/// `rate_limit_breaker::global_snapshot()` inline, the way the host-distress
/// breaker check just above this arm does) specifically for testability: the
/// process-global breaker handle it wraps is a `OnceLock` that accepts only
/// one registration per process, so a test that wants to exercise a tripped
/// breaker through the *global* would permanently poison that state for
/// every other `DispatchSweep` test sharing the same test binary (`cargo
/// test --workspace`, unlike `cargo nextest run`, does not run each test in
/// its own process — see `.config/nextest.toml`'s test-isolation notes). This
/// pure form sidesteps that hazard entirely: unit tests pass a manually
/// constructed [`crate::rate_limit_breaker::RateLimitSnapshot`] and never
/// touch the global.
///
/// Takes `force` directly (rather than the call site gating on `if !force`
/// the way the host-distress breaker check just above this arm does) so the
/// override is itself part of this function's testable contract: a unit test
/// can assert `force: true` returns `None` even against an already-suppressed
/// snapshot, proving the override independently of the host-distress
/// breaker's own `force` handling.
fn rate_limit_dispatch_refusal(
    kind: &crate::types::SweepKind,
    snapshot: Option<&crate::rate_limit_breaker::RateLimitSnapshot>,
    force: bool,
) -> Option<Response> {
    if force {
        return None;
    }
    let snap = snapshot?;
    if !snap.suppressed {
        return None;
    }
    let releases = snap.cooldown_until.map_or_else(
        || " (cooldown release time not yet known)".to_string(),
        |r| format!(" (cooldown releases at {r})"),
    );
    log::warn!(
        "dispatch_sweep: refused {kind:?} — GitHub rate-limit circuit breaker is {} \
         ({}){releases}; the shared gh API budget is in cooldown. This is the rate-limit \
         breaker, not the host-distress breaker: the fix here is waiting for the gh \
         rate-limit reset, not host load dropping. Re-run with force to override.",
        snap.phase.as_str(),
        snap.source.as_deref().unwrap_or("gh rate-limit exhaustion"),
    );
    Some(Response::Error {
        message: format!(
            "dispatch_sweep refused: GitHub rate-limit circuit breaker is {} ({}).{releases} \
             This is the shared gh API rate-limit cooldown breaker (#4429/#4440), distinct \
             from the host-distress breaker: the remediation here is waiting for the gh \
             rate-limit reset, not host load dropping. Re-run with force to override.",
            snap.phase.as_str(),
            snap.source.as_deref().unwrap_or("gh rate-limit exhaustion"),
        ),
    })
}

/// Pure decision for the drain admission gate on an explicit `dispatch_sweep`
/// request (Issue #5340). Given the request's `kind` (for the log line),
/// whether a drain is currently active, and the request's own `force` flag,
/// decide whether the dispatch should be refused, and produce the refusal
/// [`Response`] if so.
///
/// **Why this gate exists at all.** `DrainState`'s flag (#4090) is OR'd onto
/// the autonomous work-finder's, epic supervisor's, and role runner's own
/// per-tick dispatch holds — all three read it in-process and already pause
/// themselves for the duration of a drain. But `Request::DispatchSweep` (the
/// `loom-daemon dispatch` CLI and the MCP `dispatch_sweep` tool both go
/// through this one IPC request type) is handled by the synchronous
/// `handle_request` dispatcher, which never receives `drain_state` — so
/// **explicit** dispatch calls were never paused by a drain at all. On a host
/// that keeps receiving explicit dispatches (a still-active MCP client, a
/// script, another host's `dispatch_sweep` call) this alone is enough to keep
/// `count_in_flight_sweeps` from ever reaching zero, independent of whether
/// the pre-existing sweeps at drain-start were simply long-running.
///
/// Extracted as a pure function — same rationale as
/// [`rate_limit_dispatch_refusal`] just above — so a unit test can assert the
/// decision directly with a plain `bool` instead of mutating the real
/// [`DrainState`] (a `Mutex`-guarded singleton per daemon process) or wiring a
/// full [`handle_client`] socket round-trip.
///
/// `force: true` overrides, mirroring the host-distress and rate-limit
/// breakers' existing `force` precedent in `handle_request`'s own
/// `DispatchSweep` arm — an operator who explicitly wants to push a dispatch
/// through during a drain (e.g. an urgent hotfix) can.
fn drain_dispatch_refusal(
    kind: &crate::types::SweepKind,
    is_draining: bool,
    force: bool,
) -> Option<Response> {
    if !is_draining || force {
        return None;
    }
    log::warn!(
        "dispatch_sweep: refused {kind:?} — an active drain (`restart --drain`) is pausing new \
         dispatch pending a supervised restart (#4090/#5340). Wait for the drain to finish, \
         check progress with `loom-daemon status`, cancel it with `loom-daemon restart \
         --abort-drain` to resume normal dispatch immediately, or re-run with force to \
         override."
    );
    Some(Response::Error {
        message: "dispatch_sweep refused: an active drain (`restart --drain`) is pausing new \
             dispatch pending a supervised restart (#4090/#5340). Check progress with \
             `loom-daemon status`, cancel the drain with `loom-daemon restart --abort-drain` \
             to resume normal dispatch immediately, or re-run with force to override."
            .to_string(),
    })
}

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

/// Resolve which per-repo [`SweepRegistry`] a `DispatchSweep` request targets
/// (Issue #4299). Unlike [`resolve_registry`] (used by every read path —
/// `ListSweeps`, `GetSweepStatus`, quarantine requests — which keeps its
/// unconditional cwd-registry fallback; changing those defaults is out of
/// scope here), the **dispatch** path never silently trusts the daemon's own
/// cwd for the explicit-param-absent case. It consults the on-disk
/// [`WorkspaceRegistry`] instead:
///
/// - `workspace_root` `Some(non-empty)` -> normalize and, if the path is a
///   **registered** workspace, provision/return that repo's registry
///   (explicit param always wins over the default). If the normalized path is
///   *not* registered, returns a structured `workspace_unregistered` error
///   naming the offending path and every registered root (#5210) instead of
///   silently provisioning a registry for an arbitrary directory — which
///   previously surfaced only much later, as an opaque "failed to spawn sweep
///   child" once `resolve_spawn_bin` found no `spawn-worker.sh` there.
/// - `workspace_root` `None`/empty -> [`WorkspaceRegistry::resolve_dispatch_root`]
///   against the seeded default (`default`'s own `workspace_root`) decides:
///   empty registry or seeded-default-is-registered both resolve back to
///   `default` (byte-for-byte pre-#4299 behavior); a single non-cwd
///   registration provisions that workspace; multiple non-cwd registrations
///   with no seeded-default match returns a structured ambiguity error naming
///   every registered root instead of guessing.
///
/// A `WorkspaceRegistry::load_default()` failure (e.g. a corrupt registry
/// file) degrades to the empty-registry behavior (seeded default) rather than
/// blocking dispatch entirely — mirroring the existing `unwrap_or_default()`
/// precedent used elsewhere for registry reads (`main.rs`'s `workspace list`
/// handlers).
/// Returns `Err(Response::StructuredError(..))` (rather than a bare
/// `DaemonError`) so the caller can propagate it directly as the arm's
/// response. `Response` is the same "big enum" every other IPC handler
/// returns directly (never via `Result`), so `clippy::result_large_err` fires
/// here purely because of the `Result` wrapper — allowed rather than boxed to
/// match the rest of this file's `Response`-as-return-value convention.
#[allow(clippy::result_large_err)]
fn resolve_dispatch_registry(
    default: &Arc<Mutex<SweepRegistry>>,
    workspace_pool: &Arc<WorkspacePool>,
    workspace_root: Option<&str>,
) -> Result<Arc<Mutex<SweepRegistry>>, Response> {
    if let Some(root) = workspace_root {
        if !root.trim().is_empty() {
            let normalized = crate::workspace_registry::normalize_path(Path::new(root));
            // #5210: an explicit `workspace_root` must actually be a
            // registered workspace. Without this check, an unregistered path
            // (e.g. a typo, or a repo the daemon simply hasn't been told
            // about) sails straight through `get_or_provision` — which
            // provisions a registry for *any* path, registered or not — and
            // the caller only learns something is wrong many steps later,
            // via an opaque "failed to spawn sweep child" once
            // `resolve_spawn_bin` can't find `spawn-worker.sh` under the
            // bogus root.
            let registry = WorkspaceRegistry::load_default().unwrap_or_default();
            if !registry.contains(&normalized) {
                let registered: Vec<std::path::PathBuf> =
                    registry.workspaces.iter().map(|w| w.root.clone()).collect();
                // #5345: the recovery hint should point at the *target*
                // repo's own delegation, not the daemon process's cwd — a
                // delegated repo dispatching into itself should be told
                // where to register, not silently treated as undelegated.
                let target_delegated_to = crate::config_resolver::daemon_delegated_to(&normalized);
                return Err(Response::StructuredError(DaemonError::workspace_unregistered(
                    &normalized,
                    &registered,
                    target_delegated_to.as_deref(),
                )));
            }
            return Ok(workspace_pool.get_or_provision(&normalized));
        }
    }

    let seeded_default = {
        let sr = default.lock().expect("Sweep registry mutex poisoned");
        sr.config().workspace_root.clone()
    };
    let registry = WorkspaceRegistry::load_default().unwrap_or_default();

    match registry.resolve_dispatch_root(&seeded_default) {
        // `SeededDefault` is a deliberate marker, not a path to re-derive and
        // compare — see `resolve_dispatch_root`'s doc comment: reusing the
        // literal `default` `Arc` (rather than re-provisioning via the pool
        // from a normalized copy of `seeded_default`) is what guarantees this
        // always resolves to the *same* registry instance `main` seeded the
        // pool with, even when `seeded_default` contains an unresolved
        // symlink component (e.g. a `/var` -> `/private/var` tempdir on
        // macOS) that would otherwise make a path-equality check miss.
        crate::workspace_registry::DispatchRootResolution::SeededDefault => Ok(default.clone()),
        crate::workspace_registry::DispatchRootResolution::Registered(root) => {
            Ok(workspace_pool.get_or_provision(&root))
        }
        crate::workspace_registry::DispatchRootResolution::Ambiguous { registered } => {
            Err(Response::StructuredError(DaemonError::workspace_ambiguous(&registered)))
        }
    }
}

// ============================================================================
// dispatch_sweep headroom advisory (#4234 — Gap 1 of the #4231 decomposition)
// ============================================================================
//
// Before #4234, `dispatch_sweep` was the *only* remaining sweep-dispatch entry
// point that never consulted the dynamic concurrency cap
// (`resolve_dynamic_max_concurrent` — token/disk/cpu/configured-max) the
// autonomous work finder has enforced on its own dispatches since
// #3811/#3978/#4032. Any operator- or MCP-driven `dispatch_sweep` call was
// completely ungated — the mechanism this closed a gap in, not a mechanism
// built from scratch (the work finder's cap already existed and worked; this
// handler alone bypassed it). The #4231 host-meltdown 6-way fan-out was
// dispatched through exactly this handler.
//
// The policy, per the curator's #4234 guidance, is **advisory-first** —
// matching the `capacity.rs` "never a halt" precedent for token backpressure
// (#3902): `dispatch_sweep` always dispatches (an explicit operator/MCP
// request is a deliberate act, and the autonomous loop's own cap remains the
// hard backstop for *its* dispatches), but now computes the same headroom the
// work finder uses and, on a **state change** into/out of "occupancy at or
// over that headroom", logs a warning and publishes a
// `daemon.dispatch.headroom_advisory` event — so an operator firing a manual
// fan-out sees the same signal the autonomous loop already acts on. This
// requires **zero protocol change**: no new `Request::DispatchSweep` field, no
// new `Response` variant. The advisory is a side channel (log + event bus),
// exactly like `capacity.rs`'s token-pressure advisory.
//
// # Nothing blocking under the registry lock
//
// The `DispatchSweep` handler holds the registry mutex from just after this
// assessment through `begin_issue_dispatch` (idempotency dedup, guard chain,
// claim lock, label flip, `Command::spawn()`), so nothing here may block.
// Issue #6592: the ONE genuinely multi-second step in the whole dispatch path
// — the post-spawn account-selection poll, up to `TOKEN_NAME_CAPTURE_TIMEOUT`
// — is deliberately NOT under this lock; see `dispatch_sweep_nonblocking`'s
// doc comment for the begin/poll/finish split that keeps it that way. Since
// #5270 the headroom computed here is `min(disk, ram, configured max)` — cheap
// filesystem/config reads (plus a non-sleeping `/proc/meminfo` read or a
// flag-less `vm_stat`
// snapshot on macOS), no CPU sampling at all. (Before #4512 this had to
// carefully avoid `cpu_headroom_limit`, whose macOS `iostat` refresh sleeps ~1s
// and would have stalled every other IPC request on the same registry for that
// second; removing the CPU term removed that hazard outright, and RAM headroom
// deliberately preserves the same non-blocking contract.)

/// Per-repo dynamic-cap headroom snapshot computed for a `dispatch_sweep`
/// request (#4234). Mirrors the inputs `build_daemon_status` already exposes
/// on the status surface — no new plumbing, just the same math consulted at
/// dispatch time instead of only at status-poll time.
struct DispatchHeadroom {
    /// Live (non-terminal) sweep count already registered for this repo.
    occupancy: usize,
    /// `resolve_dynamic_max_concurrent` — min(disk, ram, configured max). The
    /// token axis no longer participates (#5270); `token_axis_limit` below is
    /// kept as an informational account-health figure only.
    dynamic_cap: usize,
    disk_headroom: usize,
    ram_headroom: usize,
    token_axis_limit: usize,
}

/// Compute [`DispatchHeadroom`] for `repo_root` against the **already-locked**
/// registry `sr`. See the module docs above for why nothing here may block.
fn assess_dispatch_headroom(sr: &mut SweepRegistry, repo_root: &Path) -> DispatchHeadroom {
    // Reap-on-read (mirrors ListSweeps/GetSweepStatus, Issue #3893): a sweep
    // whose child already exited must not inflate occupancy against a stale
    // `Running` entry.
    sr.reap_liveness();
    let occupancy = sr
        .list(None)
        .into_iter()
        .filter(|info| !info.state.is_terminal())
        .count();

    let wf_config = crate::work_finder::read_work_finder_config(repo_root);
    let configured_max = crate::work_finder::resolve_max_concurrent_with_config(&wf_config);
    let disk_headroom = crate::disk_headroom::disk_headroom_limit(repo_root);
    let ram_headroom = crate::ram_headroom::ram_headroom_limit();
    let token_pool_size = crate::tokens::token_pool_size(repo_root);
    let ranking = crate::capacity::read_ranking(repo_root);
    // Informational only since #5270 — no longer part of the dynamic cap.
    let token_axis_limit = ranking.as_ref().map_or(token_pool_size, |r| r.available);
    let dynamic_cap = crate::work_finder::resolve_dynamic_max_concurrent(
        disk_headroom,
        ram_headroom,
        configured_max,
    );

    DispatchHeadroom {
        occupancy,
        dynamic_cap,
        disk_headroom,
        ram_headroom,
        token_axis_limit,
    }
}

/// Whether admitting one more sweep would meet or exceed the computed dynamic
/// cap. `>=` (not `>`): dispatching this new sweep pushes occupancy to
/// `occupancy + 1`, so `occupancy >= dynamic_cap` already means "no headroom
/// left for it." Pure predicate — trivially unit-testable without touching the
/// registry or the host.
#[must_use]
fn dispatch_would_meet_or_exceed_headroom(h: &DispatchHeadroom) -> bool {
    h.occupancy >= h.dynamic_cap
}

/// Process-global, per-repo dedup state for the `daemon.dispatch.headroom_advisory`
/// event (#4234) — mirrors the work finder's `was_pressured` state-change dedup
/// (#3902), but keyed by normalized repo root (rather than a single loop-local
/// `bool`) since `dispatch_sweep` is a request handler, not a per-workspace loop
/// task, and a multi-workspace daemon must not let repo A's transition suppress
/// or falsely flip repo B's advisory.
static DISPATCH_HEADROOM_STATE: Mutex<BTreeMap<PathBuf, bool>> = Mutex::new(BTreeMap::new());

// ============================================================================
// Per-terminal input correlation (Issue #4554)
// ============================================================================
// `Request::SendInput` and `Request::GetTerminalOutput` are separate IPC round
// trips for the same agent turn: `SendInput` records the `agent_inputs` row and
// returns its id, but the forge-event (`prompt_github`) and resource-usage
// (`resource_usage`) rows recorded from the *output* of that turn are written
// by a later, independent `GetTerminalOutput` call that has no direct handle
// on that id. Before this fix both writes hardcoded `input_id: None`, so the
// `resource_usage -> agent_inputs -> prompt_github` join backing
// `get_cost_by_issue`/`get_cost_by_pr` could never match in production (#4554).
//
// This process-global map tracks the most recently recorded `agent_inputs.id`
// per terminal so `GetTerminalOutput` can look it up and correlate its writes
// to the input that (most likely) produced the output being parsed. It is a
// best-effort correlation, not a strict transactional link: concurrent input on
// the same terminal between the `SendInput` and the next `GetTerminalOutput`
// poll would attribute cost to the wrong turn, but every turn still lands on
// *some* real input row for that terminal (and thus its issue/PR), which is a
// strict improvement over the always-`None` status quo. Entries are
// intentionally never evicted on `DestroyTerminal` — a stale mapping is
// harmless (worst case: one extra analytics write correlates to an older input
// row for a terminal that no longer exists).
static LAST_INPUT_ID_BY_TERMINAL: std::sync::LazyLock<Mutex<HashMap<String, i64>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Record `input_id` as the most recent `agent_inputs` row for `terminal_id`.
/// `0` is the "recording failed" sentinel used by `Request::SendInput` (see
/// below) and is never a real row id, so it is not recorded.
fn record_last_input_id(terminal_id: &str, input_id: i64) {
    if input_id == 0 {
        return;
    }
    LAST_INPUT_ID_BY_TERMINAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(terminal_id.to_string(), input_id);
}

/// Look up the most recently recorded `agent_inputs.id` for `terminal_id`, if
/// any turn has been recorded for it yet.
fn last_input_id_for_terminal(terminal_id: &str) -> Option<i64> {
    LAST_INPUT_ID_BY_TERMINAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(terminal_id)
        .copied()
}

/// Build the advisory/recovery message for a `dispatch_sweep` headroom
/// transition. Split out from [`emit_dispatch_headroom_advisory_on_change`] so
/// the message text itself is unit-testable without the global dedup state.
fn dispatch_headroom_message(
    repo_root: &Path,
    low_headroom: bool,
    h: &DispatchHeadroom,
    kind: &crate::types::SweepKind,
) -> String {
    if low_headroom {
        format!(
            "dispatch_sweep: dispatching {kind:?} into {} while occupancy is at/over the \
             computed dynamic-cap headroom (occupancy={} >= dynamic_cap={}; \
             disk_headroom={}, ram_headroom={}, token_axis_limit={} [informational only, not \
             capacity-limiting since #5270]) — advisory only per #4234 (dispatch \
             proceeds; the autonomous work finder's own cap is unaffected)",
            repo_root.display(),
            h.occupancy,
            h.dynamic_cap,
            h.disk_headroom,
            h.ram_headroom,
            h.token_axis_limit
        )
    } else {
        format!(
            "dispatch_sweep: headroom recovered for {} (occupancy={} < dynamic_cap={})",
            repo_root.display(),
            h.occupancy,
            h.dynamic_cap
        )
    }
}

/// Emit the `daemon.dispatch.headroom_advisory` log line + event **only on a
/// state change** (never a per-call stream — mirrors `capacity.rs`'s
/// `emit_capacity_transition`, #3902). A no-op when `low_headroom` matches the
/// last-known state for `repo_root`.
fn emit_dispatch_headroom_advisory_on_change(
    event_bus: &Arc<EventBus>,
    repo_root: &Path,
    low_headroom: bool,
    h: &DispatchHeadroom,
    kind: &crate::types::SweepKind,
) {
    {
        let mut state = DISPATCH_HEADROOM_STATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let was_low = state.get(repo_root).copied().unwrap_or(false);
        if low_headroom == was_low {
            return;
        }
        state.insert(repo_root.to_path_buf(), low_headroom);
    }

    let message = dispatch_headroom_message(repo_root, low_headroom, h, kind);
    if low_headroom {
        log::warn!("{message}");
    } else {
        log::info!("{message}");
    }
    if let Err(e) = event_bus.publish_generic(
        "daemon.dispatch.headroom_advisory",
        serde_json::json!({
            "repo_root": repo_root.display().to_string(),
            "low_headroom": low_headroom,
            "occupancy": h.occupancy,
            "dynamic_cap": h.dynamic_cap,
            "disk_headroom": h.disk_headroom,
            "ram_headroom": h.ram_headroom,
            "token_axis_limit": h.token_axis_limit,
            "message": message,
        }),
    ) {
        log::debug!("dispatch_sweep: headroom advisory not delivered: {e}");
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
            let (raw_role, working_dir, worktree_path) = if let Some(info) = terminal_info {
                (info.role, info.working_dir, info.worktree_path)
            } else {
                (None, None, None)
            };

            // Determine workspace path (prefer worktree, fallback to working_dir)
            let workspace_path = worktree_path.or(working_dir.clone());

            // Resolve the real Loom role (e.g. "judge", "curator") from the
            // terminal's own `roleConfig.roleFile` in `.loom/config.json`,
            // rather than trusting `raw_role` (`terminals[].role`) — every
            // configured terminal sets that field to the same literal
            // `"claude-code-worker"` string, which is why the `stats` role
            // breakdown collapsed every agent into one bucket (#6128). Falls
            // back to `raw_role` when no terminal config entry matches (e.g.
            // an ad-hoc terminal created without a roster entry), preserving
            // prior behavior for that case.
            let agent_role = workspace_path
                .as_ref()
                .and_then(|ws| {
                    let config =
                        config_resolver::resolve_effective_config(std::path::Path::new(ws));
                    role_validation::resolve_role_file_for_terminal_id(&config, &id)
                })
                .or(raw_role);

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

            // Track this input as the terminal's most recent turn so a later
            // `GetTerminalOutput` call can correlate its forge-event / resource-
            // usage writes back to it (#4554). Recorded unconditionally (even if
            // `tm.send_input` below fails) since the `agent_inputs` row itself was
            // already written above regardless of delivery outcome.
            record_last_input_id(&id, input_id);

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

                        // Correlate this output batch with the most recently recorded
                        // input for this terminal (#4554) — `SendInput` and
                        // `GetTerminalOutput` are separate IPC round trips for the
                        // same turn, and this is the only link between them.
                        let correlated_input_id = last_input_id_for_terminal(&id);

                        let output_record = AgentOutput {
                            id: None,
                            input_id: correlated_input_id,
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
                                let prompt_event =
                                    parsed_event.to_prompt_forge_event(correlated_input_id);
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
                            match db.record_resource_usage_from_output(
                                correlated_input_id,
                                &output_str,
                                None,
                            ) {
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
        //
        // Production traffic never reaches this `DispatchSweep` arm (Issue
        // #6592): `handle_client` intercepts `DispatchSweep` and services it
        // via the non-blocking `dispatch_sweep_nonblocking`, which releases
        // the registry mutex around the child's account-selection poll
        // (mirrors the #3807 `CancelSweep` split above). This synchronous
        // fallback (holding the lock across the full guard chain + spawn +
        // poll) remains for direct/unit-test callers where lock contention is
        // irrelevant.
        Request::DispatchSweep {
            kind,
            idempotency_key,
            model,
            effort,
            depends_on,
            workspace_root,
            force,
        } => {
            // Host-distress circuit breaker (#4235): a *tripped* breaker
            // represents SUSTAINED, already-observed host distress across
            // multiple ticks — a materially stronger signal than the
            // point-in-time headroom advisory below (which by #4234's deliberate
            // design only *advises*, never blocks). Because the breaker's signal
            // is stronger and stateful, it **hard-blocks** an explicit
            // `dispatch_sweep` by default; an operator who truly wants to
            // dispatch into a distressed host passes `force: true` to override.
            // This is the one-sentence reconciliation the issue asks for: the
            // breaker blocks where the headroom check advises *because* it fires
            // only on proven, sustained distress, not a single-tick reading.
            if !force {
                if let Some(snap) = crate::host_breaker::global_snapshot() {
                    if snap.suppressed {
                        let releases = snap.releases_at.map_or_else(
                            || " (host still hot — cool-down not yet started)".to_string(),
                            |r| format!(" (cool-down releases at {r})"),
                        );
                        log::warn!(
                            "dispatch_sweep: refused {kind:?} — host circuit breaker is {} \
                             ({}){releases}; running work drains, new dispatch paused. \
                             Re-run with force to override.",
                            snap.phase.as_str(),
                            snap.reason.as_deref().unwrap_or("sustained host distress"),
                        );
                        return Response::Error {
                            message: format!(
                                "dispatch_sweep refused: host circuit breaker is {} ({}).{releases} \
                                 Running work is draining and new dispatch is paused (#4235). \
                                 Re-run with force to override.",
                                snap.phase.as_str(),
                                snap.reason.as_deref().unwrap_or("sustained host distress"),
                            ),
                        };
                    }
                }
            }
            // GitHub rate-limit circuit breaker (#4429/#4440, gap closed by
            // #4666): the daemon's own internal polling loops (work-finder,
            // claim/quarantine reconciliation, epic supervisor, role-runner)
            // already pause against this breaker during cooldown — but until
            // now an explicit `dispatch_sweep` never consulted it, so a
            // brand-new sweep/judge/champion session could still be
            // dispatched while the shared forge rate-limit budget was in a
            // known cooldown. This is a *distinct* breaker from the
            // host-distress one above: different root cause (a `gh`
            // rate-limit cooldown vs. host CPU/memory distress) and different
            // remediation (waiting for the `gh` rate-limit reset vs. waiting
            // for host load to drop) — see [`rate_limit_dispatch_refusal`]'s
            // doc comment for why the decision itself is a pure, `force`-aware
            // helper rather than inlined here. Hard-blocks by default;
            // `force: true` overrides this breaker independently of the
            // host-distress one.
            if let Some(refusal) = rate_limit_dispatch_refusal(
                &kind,
                crate::rate_limit_breaker::global_snapshot().as_ref(),
                force,
            ) {
                return refusal;
            }
            // Dispatch-only resolution (Issue #4299): consults the workspace
            // registry for the explicit-param-absent case instead of always
            // trusting the daemon's own cwd. See `resolve_dispatch_registry`'s
            // doc comment for the full precedence and why this differs from
            // `resolve_registry` (used by the read paths below).
            let target = match resolve_dispatch_registry(
                sweep_registry,
                workspace_pool,
                workspace_root.as_deref(),
            ) {
                Ok(target) => target,
                Err(response) => return response,
            };
            let mut sr = target
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Model resolution (issue #3944): an explicit `model` param still
            // wins, but an ABSENT one falls back to `autonomous.model` in
            // `.loom/config.json` and then the shipped non-premium default —
            // never the operator's interactive CLI default. This mirrors the
            // autonomous work-finder / epic-supervisor dispatch paths so every
            // daemon-dispatched child is pinned to an explicit model.
            let repo_root = sr.config().workspace_root.clone();

            // Headroom advisory (#4234, Gap 1 of #4231's decomposition): consult
            // the same dynamic concurrency cap the autonomous work finder
            // applies to its own dispatches, and advise — never gate — an
            // explicit `dispatch_sweep` call that would push occupancy at/over
            // it. See the module docs above this arm for the full rationale and
            // the "never the blocking refresh under this lock" hazard.
            let headroom = assess_dispatch_headroom(&mut sr, &repo_root);
            let low_headroom = dispatch_would_meet_or_exceed_headroom(&headroom);
            emit_dispatch_headroom_advisory_on_change(
                event_bus,
                &repo_root,
                low_headroom,
                &headroom,
                &kind,
            );

            // Model intent survives until the shared runtime-admission boundary.
            log::info!(
                "dispatch_sweep: {:?}; \
                 headroom occupancy={} dynamic_cap={} (disk={} ram={} tokens={} [informational \
                 only, not capacity-limiting since #5270])",
                kind,
                headroom.occupancy,
                headroom.dynamic_cap,
                headroom.disk_headroom,
                headroom.ram_headroom,
                headroom.token_axis_limit
            );
            match sr.dispatch_with_model(
                &kind,
                idempotency_key,
                crate::sweep_registry::DispatchModel::Request(model.as_deref()),
                effort.as_deref(),
                depends_on,
            ) {
                Ok(outcome) => Response::SweepDispatched {
                    sweep_id: outcome.sweep_id,
                    pid: outcome.pid,
                    token_name: outcome.token_name,
                    log_path: outcome.log_path,
                },
                Err(e) => match e.downcast::<crate::runtime_admission::RuntimeRejection>() {
                    Ok(rejection) => Response::RuntimeRejected(rejection),
                    Err(e) => {
                        // Issue #5236: the pre-dispatch `log::info!` above only
                        // ever logs the *attempt*, never the failure — until
                        // now, the daemon's own log had no record of why a
                        // dispatch failed at all, only the caller's response
                        // did (#5210/#5218 fixed the caller-facing half). Log
                        // the same full error chain at WARN so an operator
                        // reading `loom-daemon`'s log (not just the MCP/CLI
                        // response) can diagnose a dispatch failure without
                        // reproducing it.
                        log::warn!("dispatch_sweep: {kind:?} failed: {e:#}");
                        Response::Error {
                            // #5210: `{e:#}` (anyhow's alternate Display) walks
                            // the full `.context()` chain instead of printing
                            // only the outermost context, so a specific inner
                            // failure (e.g. `resolve_spawn_bin`'s
                            // "spawn-worker.sh not found under ...") reaches
                            // the MCP client instead of being silently
                            // collapsed into "failed to spawn sweep child".
                            message: format!("dispatch_sweep failed: {e:#}"),
                        }
                    }
                },
            }
        }

        Request::ListSweeps {
            state_filter,
            workspace_root,
            all_workspaces,
        } => {
            let sweeps = if all_workspaces {
                // Fleet-wide fan-out (Issue #6006, the deferred follow-up to
                // #3930): enumerate every registered managed workspace the
                // same way `ListQuarantines`'s `None` case and
                // `build_daemon_status` do — an empty registry still yields
                // exactly `[fallback_root]`, so a single-workspace daemon's
                // fan-out is byte-for-byte the same set `workspace_root: None`
                // would have returned. `workspace_root` is ignored here (the
                // two are mutually exclusive; the flag always wins).
                let fallback_root = {
                    let sr = sweep_registry
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    sr.config().workspace_root.clone()
                };
                let workspace_registry = WorkspaceRegistry::load_default().unwrap_or_default();
                let roots = workspace_registry.effective_roots(&fallback_root);
                let mut sweeps = Vec::new();
                for root in &roots {
                    let registry = workspace_pool.get_or_provision(root);
                    let mut sr = registry
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    // Reap-on-read (Issue #3893) per-registry, same as the
                    // single-workspace path below.
                    sr.reap_liveness();
                    sweeps.extend(sr.list(state_filter.as_ref()));
                }
                // Stable, deterministic ordering across repos: group by owning
                // repo, then by dispatch time within a repo.
                sweeps.sort_by(|a, b| (&a.repo, a.started_at).cmp(&(&b.repo, b.started_at)));
                sweeps
            } else {
                let target =
                    resolve_registry(sweep_registry, workspace_pool, workspace_root.as_deref());
                let mut sr = target
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // Reap-on-read (Issue #3893): reconcile liveness before listing
                // so a sweep whose child has already exited is never reported
                // `Running` just because the 30s reaper timer has not ticked
                // yet.
                sr.reap_liveness();
                sr.list(state_filter.as_ref())
            };
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
            let mut sr = target
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            let sr = target
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            let mut sr = target
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            let mut sr = target
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let was_quarantined = sr.clear_quarantine(issue);
            Response::QuarantineCleared {
                issue,
                was_quarantined,
            }
        }

        Request::ListQuarantines { workspace_root } => {
            // Operator-reachable insta-crash-quarantine read path (Issue
            // #4215) — the authority for "which issues are quarantined right
            // now", distinct from a forge `loom:blocked` query. Unlike every
            // other `workspace_root: Option<String>` request, `None` here
            // means "every registered workspace" (see the doc comment on
            // `Request::ListQuarantines`), not just the default one, so a
            // `Some(root)` scopes to a single registry via the same
            // `resolve_registry` path `ClearQuarantine` uses, while `None`
            // enumerates roots the way `build_daemon_status` does.
            let now = Utc::now();
            let entries = match workspace_root.as_deref() {
                Some(root) if !root.trim().is_empty() => {
                    let target = resolve_registry(sweep_registry, workspace_pool, Some(root));
                    let sr = target
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    sr.quarantine_entries(now)
                }
                _ => {
                    // No `fallback_root` is threaded into this synchronous
                    // dispatcher (unlike `build_daemon_status`), but the
                    // default registry's own config already carries it —
                    // it's the same root `resolve_registry`'s `None` arm
                    // would have targeted.
                    let fallback_root = {
                        let sr = sweep_registry
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        sr.config().workspace_root.clone()
                    };
                    let workspace_registry = WorkspaceRegistry::load_default().unwrap_or_default();
                    let roots = workspace_registry.effective_roots(&fallback_root);
                    let mut entries = Vec::new();
                    for root in &roots {
                        let registry = workspace_pool.get_or_provision(root);
                        let sr = registry
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        entries.extend(sr.quarantine_entries(now));
                    }
                    entries.sort_unstable_by_key(|e| e.issue);
                    entries
                }
            };
            Response::QuarantineList { entries }
        }

        Request::RecordDispatchFailure {
            issue,
            reason,
            workspace_root,
        } => {
            // Operator/script-reachable dispatch-backoff arm (Issue #6192):
            // the sweep-side counterpart to the reaper's own automatic
            // `record_dispatch_failure` calls, for a caller with no direct
            // access to the in-memory `SweepRegistry` (a builder worktree's
            // `build-gate.sh`, after its own bounded per-step toolchain
            // timeout kills a hung command). Same `resolve_registry` +
            // `ClearQuarantine`-style `workspace_root` semantics as its
            // sibling requests above.
            let target =
                resolve_registry(sweep_registry, workspace_pool, workspace_root.as_deref());
            let mut sr = target
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(reason) = reason.as_deref() {
                log::info!(
                    "sweep_registry: issue #{issue} dispatch failure recorded via IPC \
                     (RecordDispatchFailure, #6192): {reason}"
                );
            }
            sr.record_dispatch_failure(issue);
            let consecutive = sr.dispatch_failure_count(issue);
            let backoff_secs = sr
                .dispatch_backoff_remaining(issue, Utc::now())
                .map(|d| d.as_secs());
            Response::DispatchFailureRecorded {
                issue,
                consecutive,
                backoff_secs,
            }
        }

        Request::RecordNoopRelease {
            issue,
            reason,
            workspace_root,
        } => {
            // Sweep-reachable no-op-release arm (Issue #6670): the primary
            // caller is the `/loom:sweep` orchestrator itself, right before
            // releasing a `loom:building` claim it determined needs no
            // changes — same `resolve_registry` + `RecordDispatchFailure`-style
            // `workspace_root` semantics as its sibling request above.
            let target =
                resolve_registry(sweep_registry, workspace_pool, workspace_root.as_deref());
            let mut sr = target
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(reason) = reason.as_deref() {
                log::info!(
                    "sweep_registry: issue #{issue} no-op release recorded via IPC \
                     (RecordNoopRelease, #6670): {reason}"
                );
            }
            sr.record_noop_release(issue, reason);
            let consecutive = sr.noop_release_count(issue);
            let cooldown_secs = sr
                .noop_cooldown_remaining(issue, Utc::now())
                .map(|d| d.as_secs());
            Response::NoopReleaseRecorded {
                issue,
                consecutive,
                cooldown_secs,
            }
        }

        // ====================================================================
        // Event Bus Handlers (Issue #3453 — Phase B of #3449)
        // ====================================================================
        Request::PublishEvent { topic, payload } => {
            // Publish path used by sweep children — the topic is the canonical
            // name (e.g., "sweep.issue.123.phase") and the payload is JSON. See
            // `defaults/.claude/commands/loom/sweep.md` for the per-topic
            // payload schema.
            //
            // Issue #4466: the two documented child-published topics
            // (`sweep.issue.{N}.phase` / `.blocker`) are upgraded to their
            // typed variants here so the narration sink can emit the documented
            // room lines — an `Event::Generic` is never narrated. Unknown
            // topics and malformed payloads fall through to `Event::Generic`
            // unchanged (publish is fire-and-forget advisory).
            let topic_ack = topic.clone();
            let event = Event::from_published(topic, payload);
            match event_bus.publish(event) {
                Ok(receivers) => Response::EventPublished {
                    topic: topic_ack,
                    receivers,
                },
                Err(_) => Response::EventPublished {
                    topic: topic_ack,
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

        // Real IPC shutdown is intercepted asynchronously before this dispatcher.
        Request::Shutdown => std::process::exit(EXIT_SHUTDOWN),
        Request::RestartDaemon => {
            // Structurally unreachable: `handle_client` intercepts
            // `RestartDaemon` before dispatching to `handle_request` (it must
            // reply-then-exit). Answer defensively in case of a future direct
            // caller — do NOT exit here, only `handle_client` may end the
            // process for a supervised relaunch.
            //
            // #5119: this dispatcher has no `fallback_root`, so the in-flight
            // count feeding the message's fate clause comes from the PRIMARY
            // registry only (not the cross-root walk `handle_client` does). That
            // under-count is acceptable precisely because no live caller reaches
            // here; the wording itself — which supervisor destroys the work — is
            // identical either way.
            let in_flight = sweep_registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .list(None)
                .into_iter()
                .filter(|info| !info.state.is_terminal())
                .count();
            build_restart_decision(in_flight).0
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

mod roster_status;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests;
