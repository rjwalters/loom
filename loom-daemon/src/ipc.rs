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
/// convention. `supervisor` is only interpolated on the relaunch line.
#[must_use]
pub fn drain_complete_log_line(then_exit: bool, supervisor: &str) -> String {
    if then_exit {
        format!(
            "drain complete — 0 in-flight sweeps; exiting {EXIT_SHUTDOWN} and staying down \
             (then_exit — Issue #4343 teardown). No sweep was killed; no orphan left behind."
        )
    } else {
        format!(
            "drain complete — 0 in-flight sweeps; exiting {EXIT_RESTART} for a \
             {supervisor}-supervised relaunch. No sweep was killed; no orphan left behind."
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
                    log::warn!("{}", drain_complete_log_line(true, ""));
                    std::process::exit(drain_exit_code(true));
                }
                // This path only runs after `handle_drain_request` proved
                // supervision, so `detect_supervisor()` should still be `Some`
                // here; fall back to a generic label rather than hardcoding
                // launchd if the environment somehow changed underneath us.
                let sup = detect_supervisor().unwrap_or_else(|| "supervisor".to_string());
                log::warn!("{}", drain_complete_log_line(false, &sup));
                std::process::exit(drain_exit_code(false));
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
                    std::process::exit(drain_exit_code(true));
                }
                log::warn!(
                    "drain timed out with {in_flight} in-flight; --force-after-timeout cancelled \
                     {cancelled} sweep(s); exiting {EXIT_RESTART} for a supervised relaunch"
                );
                std::process::exit(drain_exit_code(false));
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

        let gh_bin = sr
            .config()
            .gh_bin
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("gh"));
        let (resolved_model, model_source_label, arm) = match (&kind, model.as_deref()) {
            (crate::types::SweepKind::Issue(issue), None) => {
                let resolved = crate::sweep_registry::resolve_autonomous_dispatch_model_lazy(
                    &repo_root,
                    *issue,
                    || crate::sweep_registry::fetch_issue_complexity(&gh_bin, &repo_root, *issue),
                );
                (resolved.model, resolved.source_label, resolved.arm)
            }
            _ => {
                let (m, s) =
                    crate::sweep_registry::resolve_dispatch_model(&repo_root, model.as_deref());
                (m, s.as_str(), None)
            }
        };
        log::info!(
            "dispatch_sweep: {:?} with{} model={resolved_model} (source={model_source_label}); \
             headroom occupancy={} dynamic_cap={} (disk={} ram={} tokens={} [informational \
             only, not capacity-limiting since #5270])",
            kind,
            arm.map_or_else(String::new, |a| format!(" arm={a}")),
            headroom.occupancy,
            headroom.dynamic_cap,
            headroom.disk_headroom,
            headroom.ram_headroom,
            headroom.token_axis_limit
        );

        sr.begin_issue_dispatch(
            &kind,
            idempotency_key,
            Some(&resolved_model),
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
    for root in &roots {
        let registry = workspace_pool.get_or_provision(root);
        let (live, quarantined_issues, locked_unregistered): (
            Vec<crate::types::SweepInfo>,
            Vec<u32>,
            Vec<(u32, u32)>,
        ) = {
            let sr = registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            (live, sr.quarantined_issues_sorted(), sr.unregistered_locked_issues())
        };
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
        let role_runner_roles = crate::role_runner::resolve_roles(&role_runner_config)
            .iter()
            .map(|spec| spec.name.to_string())
            .collect();
        let role_runner_on_idle_roles =
            crate::role_runner::resolve_on_idle_roles(&role_runner_config)
                .iter()
                .map(|spec| spec.name.to_string())
                .collect();
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
        // Fleet-wide quarantine-stash visibility (#5692): a `git stash list`
        // shell-out per registered root, aggregated into this repo's own
        // counts. Best-effort (see `collect_stash_summary`'s doc comment) —
        // a repo with no stashes, or that is transiently unreadable, simply
        // reports zeros rather than failing this whole status build.
        let repo_stash_summary = crate::quarantine_stash_status::collect_stash_summary(root);
        // Issue #5682: recomputed live (a cheap `stat`), not read once from the
        // registry — a root that had `.claude/commands/loom/sweep.md` at
        // `workspace add` time but lost it later (deleted, or a fresh clone
        // that never ran `init`) must still be caught on every snapshot, not
        // just at registration.
        let sweep_command_missing =
            !crate::sweep_registry::SweepRegistryConfig::new(root.clone()).has_sweep_command();
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
            role_runner_on_idle_roles,
            role_runner_env_override,
            token_pool_dir: Some(repo_token_pool_dir),
            ranking_present: repo_ranking_present,
            ranking_age_secs: repo_ranking_age_secs,
            stash_total_count: repo_stash_summary.total_count,
            stash_quarantine_count: repo_stash_summary.quarantine_count,
            stash_oldest_age_secs: repo_stash_summary.oldest_stash_age_secs,
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

    DaemonStatusReport {
        in_flight,
        unregistered_locked,
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
        // Per-repo deep-clean state (#5919) — the same process-global snapshot
        // pattern once more. Reported for EVERY repo the reaper has evaluated,
        // not only those where a pass fired: "last evaluated 3m ago, 118G free"
        // is the answer to "is this host reclaiming its own disk?" on a healthy
        // host, and the absence of any entry is itself the signal that the
        // reaper (and therefore the deep pass) is not running at all.
        deep_clean: crate::deep_clean::snapshot()
            .into_iter()
            .map(|(root, s)| crate::types::DeepCleanRepoStatus {
                root,
                last_evaluated_at: s.last_evaluated_at,
                last_reason: s.last_reason,
                last_free_gb: s.last_free_gb,
                last_fired_at: s.last_fired_at,
                last_reclaimed: s.last_reclaimed,
            })
            .collect(),
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

            // Issue #4809: an explicit `model` param always wins (unchanged
            // precedence), but an ABSENT one for a single-issue dispatch also
            // considers the model-cost A/B experiment's forced arm — mirroring
            // the autonomous work-finder / epic-supervisor dispatch paths —
            // before falling back to `autonomous.model` / the shipped default.
            //
            // Issue #4827: the arm is stratified by the issue's real
            // `<!-- loom:complexity=... -->` marker. Like the epic supervisor
            // (and unlike the work finder, which carries the body on its
            // `WorkItem`), this handler has no cached body — so the fetch is
            // LAZY, running only inside the `experiment` branch. `off` /
            // `observe` dispatches make zero extra `gh` calls, and a failed
            // fetch degrades to the unchanged `routine` stratum.
            let gh_bin = sr
                .config()
                .gh_bin
                .clone()
                .unwrap_or_else(|| std::path::PathBuf::from("gh"));
            let (resolved_model, model_source_label, arm) = match (&kind, model.as_deref()) {
                (crate::types::SweepKind::Issue(issue), None) => {
                    let resolved = crate::sweep_registry::resolve_autonomous_dispatch_model_lazy(
                        &repo_root,
                        *issue,
                        || {
                            crate::sweep_registry::fetch_issue_complexity(
                                &gh_bin, &repo_root, *issue,
                            )
                        },
                    );
                    (resolved.model, resolved.source_label, resolved.arm)
                }
                _ => {
                    let (m, s) =
                        crate::sweep_registry::resolve_dispatch_model(&repo_root, model.as_deref());
                    (m, s.as_str(), None)
                }
            };
            log::info!(
                "dispatch_sweep: {:?} with{} model={resolved_model} (source={model_source_label}); \
                 headroom occupancy={} dynamic_cap={} (disk={} ram={} tokens={} [informational \
                 only, not capacity-limiting since #5270])",
                kind,
                arm.map_or_else(String::new, |a| format!(" arm={a}")),
                headroom.occupancy,
                headroom.dynamic_cap,
                headroom.disk_headroom,
                headroom.ram_headroom,
                headroom.token_axis_limit
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

    // ===== SendInput / GetTerminalOutput input correlation (Issue #4554) =====

    /// End-to-end proof that a `SendInput` turn's `agent_inputs.id` is threaded
    /// through to the `resource_usage` and `prompt_github` rows written by the
    /// following `GetTerminalOutput` call, so `get_cost_by_issue` — which joins
    /// `resource_usage -> agent_inputs -> prompt_github` on `input_id` — returns
    /// a non-empty result for the turn's issue. Before the #4554 fix, both
    /// writes hardcoded `input_id: None` and this join could never match in
    /// production.
    #[test]
    fn test_send_input_then_get_terminal_output_correlates_cost_by_issue() {
        let (tm, db, sr, bus) = setup_test_context();
        let terminal_id = format!("ipc-test-4554-{}", std::process::id());

        // Seed the terminal's output file directly: `get_terminal_output` reads
        // from `/tmp/loom-<id>.out` unconditionally, regardless of whether `id`
        // is a live, registered terminal (see `TerminalManager::get_terminal_output`),
        // so this test doesn't need a real tmux-backed terminal.
        let output_path = format!("/tmp/loom-{terminal_id}.out");
        let output_body = "Creating pull request...\n\
             https://github.com/rjwalters/loom/issues/4554\n\
             Tokens: 1,234 in / 567 out\n\
             Model: claude-3-5-sonnet\n";
        std::fs::write(&output_path, output_body).unwrap();

        // Cleanup guard so a failing assertion below still removes the fixture
        // file rather than leaking it into later test runs.
        struct CleanupGuard(String);
        impl Drop for CleanupGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _cleanup = CleanupGuard(output_path.clone());

        // `SendInput` records the `agent_inputs` row and, via the #4554 fix,
        // tracks its id for this terminal. `id` isn't a real, registered
        // terminal, so delivery itself fails — that's fine: the DB write and
        // the correlation tracking both happen unconditionally before delivery
        // is attempted (mirrors production, where the two are also decoupled).
        let send_response = handle_request(
            Request::SendInput {
                id: terminal_id.clone(),
                data: "some command".to_string(),
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        assert!(
            matches!(send_response, Response::StructuredError(_)),
            "expected delivery to fail for an unregistered terminal id, got: {send_response:?}"
        );

        // `GetTerminalOutput` reads the seeded file and, with the fix,
        // correlates its forge-event/resource-usage writes to the input
        // recorded above instead of writing `input_id: None`.
        let output_response = handle_request(
            Request::GetTerminalOutput {
                id: terminal_id.clone(),
                start_byte: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        assert!(
            matches!(output_response, Response::TerminalOutput { .. }),
            "expected TerminalOutput, got: {output_response:?}"
        );

        let cost = db.lock().unwrap().get_cost_by_issue(Some(4554)).unwrap();
        assert!(
            !cost.is_empty(),
            "expected a non-empty cost-by-issue rollup for issue #4554 after a recorded turn \
             (the resource_usage -> agent_inputs -> prompt_github join must match)"
        );
        assert_eq!(cost[0].issue_number, 4554);
        assert!(cost[0].total_cost > 0.0);
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

    // ========================================================================
    // dispatch_sweep headroom advisory (#4234 — Gap 1 of #4231's decomposition)
    // ========================================================================

    fn fake_headroom(occupancy: usize, dynamic_cap: usize) -> DispatchHeadroom {
        DispatchHeadroom {
            occupancy,
            dynamic_cap,
            disk_headroom: 10,
            ram_headroom: 10,
            token_axis_limit: 5,
        }
    }

    #[test]
    fn dispatch_headroom_predicate_boundary() {
        assert!(
            !dispatch_would_meet_or_exceed_headroom(&fake_headroom(2, 3)),
            "below cap: headroom remains"
        );
        assert!(
            dispatch_would_meet_or_exceed_headroom(&fake_headroom(3, 3)),
            "at cap: no headroom left for one more"
        );
        assert!(
            dispatch_would_meet_or_exceed_headroom(&fake_headroom(5, 3)),
            "over cap: definitely no headroom"
        );
    }

    #[test]
    fn dispatch_headroom_message_names_every_axis() {
        let h = DispatchHeadroom {
            occupancy: 4,
            dynamic_cap: 3,
            disk_headroom: 9,
            ram_headroom: 7,
            token_axis_limit: 6,
        };
        let kind = SweepKind::Issue(123);
        let repo = Path::new("/tmp/loom-test-repo");

        let entered = dispatch_headroom_message(repo, true, &h, &kind);
        assert!(entered.contains("occupancy=4"), "{entered}");
        assert!(entered.contains("dynamic_cap=3"), "{entered}");
        assert!(entered.contains("disk_headroom=9"), "{entered}");
        assert!(entered.contains("ram_headroom=7"), "{entered}");
        assert!(entered.contains("token_axis_limit=6"), "{entered}");
        assert!(entered.contains("123"), "{entered}");
        assert!(entered.contains("advisory only"), "{entered}");

        let recovered = dispatch_headroom_message(repo, false, &h, &kind);
        assert!(recovered.contains("recovered"), "{recovered}");
    }

    #[test]
    fn dispatch_headroom_advisory_dedups_on_state_change() {
        let bus = Arc::new(EventBus::new());
        let mut sub = bus.subscribe(["daemon.dispatch.headroom_advisory"]);
        // A fresh, unique tempdir path keys the process-global dedup state
        // independently of any other test (#4234's per-repo dedup design).
        let repo_dir = tempdir().unwrap();
        let repo_root = repo_dir.path().to_path_buf();
        let kind = SweepKind::Issue(4234);
        let low = fake_headroom(5, 3);
        let ok = fake_headroom(1, 3);

        // Entering low headroom fires the advisory.
        emit_dispatch_headroom_advisory_on_change(&bus, &repo_root, true, &low, &kind);
        match sub
            .try_recv()
            .expect("advisory published on entering low headroom")
        {
            Event::Generic { topic, payload } => {
                assert_eq!(topic, "daemon.dispatch.headroom_advisory");
                assert_eq!(payload["low_headroom"].as_bool(), Some(true));
                assert_eq!(payload["occupancy"].as_u64(), Some(5));
                assert_eq!(payload["dynamic_cap"].as_u64(), Some(3));
            }
            other => panic!("expected Generic advisory event, got {other:?}"),
        }

        // Still low on the next call — deduped, no second event.
        emit_dispatch_headroom_advisory_on_change(&bus, &repo_root, true, &low, &kind);
        assert!(
            matches!(sub.try_recv(), Err(crate::event_bus::RecvError::Empty)),
            "no duplicate advisory while headroom stays low"
        );

        // Recovers — symmetric recovery event.
        emit_dispatch_headroom_advisory_on_change(&bus, &repo_root, false, &ok, &kind);
        match sub.try_recv().expect("recovery event published") {
            Event::Generic { topic, payload } => {
                assert_eq!(topic, "daemon.dispatch.headroom_advisory");
                assert_eq!(payload["low_headroom"].as_bool(), Some(false));
            }
            other => panic!("expected Generic recovery event, got {other:?}"),
        }

        // Staying recovered — deduped again.
        emit_dispatch_headroom_advisory_on_change(&bus, &repo_root, false, &ok, &kind);
        assert!(
            matches!(sub.try_recv(), Err(crate::event_bus::RecvError::Empty)),
            "no duplicate recovery event while headroom stays healthy"
        );
    }

    /// End-to-end (#4234): `dispatch_sweep` must dispatch even when the
    /// computed headroom is fully saturated — advisory-first, never a hard
    /// gate. Forces `configured_max=1` via env (the smallest term always wins
    /// the `min()` in `resolve_dynamic_max_concurrent`), which is deterministic
    /// regardless of the real host's token/disk/cpu state.
    #[test]
    #[serial_test::serial]
    fn test_dispatch_sweep_still_dispatches_under_synthetic_low_headroom() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
        // Absent `workspace_root` now consults the on-disk workspace registry
        // (#4299) — pin it to an empty temp registry so this test's outcome
        // never depends on the host's real `~/.loom/workspaces.json`.
        let _registry_guard = seed_temp_registry(&[]);

        std::env::set_var(crate::work_finder::WORK_FINDER_MAX_CONCURRENT_ENV, "1");

        let dispatch_issue = |n: u32| {
            handle_request(
                Request::DispatchSweep {
                    kind: SweepKind::Issue(n),
                    idempotency_key: None,
                    model: None,
                    effort: None,
                    depends_on: None,
                    workspace_root: None,
                    force: false,
                },
                &tm,
                &db,
                &sr,
                &bus,
                &test_pool(),
            )
        };

        // First dispatch always succeeds regardless of computed headroom.
        match dispatch_issue(90_001) {
            Response::SweepDispatched { .. } => {}
            other => panic!("Expected SweepDispatched, got: {other:?}"),
        }

        // Second dispatch: occupancy is now >= the forced ceiling of 1, so this
        // call is guaranteed to be at/over the dynamic cap — and it STILL
        // dispatches (advisory-only per #4234, never a hard gate).
        match dispatch_issue(90_002) {
            Response::SweepDispatched { .. } => {}
            other => panic!(
                "Expected SweepDispatched even at/over headroom (advisory-first policy), \
                 got: {other:?}"
            ),
        }

        std::env::remove_var(crate::work_finder::WORK_FINDER_MAX_CONCURRENT_ENV);
    }

    #[test]
    fn test_handle_request_list_sweeps_empty() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
        let response = handle_request(
            Request::ListSweeps {
                state_filter: None,
                workspace_root: None,
                all_workspaces: false,
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

        // #5210: an explicit `workspace_root` on DispatchSweep must now name a
        // *registered* workspace (`seed_temp_registry` is defined below in this
        // module; it also points `WorkspaceRegistry::load_default()` at a temp
        // file so this test never touches the real `~/.loom/workspaces.json`).
        let _guard = seed_temp_registry(&[dir_b.path()]);

        // Dispatch issue #42 into repo B explicitly.
        let dispatched = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(42),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                workspace_root: Some(dir_b.path().to_string_lossy().into_owned()),
                force: false,
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
                all_workspaces: false,
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
                all_workspaces: false,
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

    // ===== ListSweeps fleet-wide fan-out (Issue #6006 — deferred follow-up
    // to #3930) =====
    //
    // `all_workspaces: true` enumerates every registered managed workspace
    // the same way `ListQuarantines`'s `None` case does, so these tests seed
    // `REGISTRY_PATH_ENV` at a temp file (via `seed_temp_registry`) rather
    // than touching the real `~/.loom/workspaces.json`.

    /// `all_workspaces: true` aggregates sweeps from every registered root in
    /// one call — no caller-supplied `workspace_root` needed — and each
    /// returned `SweepInfo` still carries the `repo` field naming its owner,
    /// so a fleet-wide caller never needs to already know the individual repo
    /// roots (the issue's core acceptance criterion).
    #[test]
    #[serial_test::serial]
    fn test_list_sweeps_all_workspaces_fans_out_across_registered_roots() {
        let (tm, db, _, bus) = setup_test_context();

        let (sr_default, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
        let (sr_b, dir_b, _rec_b) = setup_sweep_registry_in_tempdir();
        let root_a = crate::workspace_registry::normalize_path(dir_a.path());
        let root_b = crate::workspace_registry::normalize_path(dir_b.path());

        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(root_a, sr_default.clone());
        pool.seed(root_b.clone(), sr_b.clone());

        // Register BOTH roots so `effective_roots` enumerates both.
        let _guard = seed_temp_registry(&[dir_a.path(), dir_b.path()]);

        let dispatched_a = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(60_060),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                workspace_root: Some(dir_a.path().to_string_lossy().into_owned()),
                force: false,
            },
            &tm,
            &db,
            &sr_default,
            &bus,
            &pool,
        );
        assert!(
            matches!(dispatched_a, Response::SweepDispatched { .. }),
            "expected SweepDispatched for repo A, got: {dispatched_a:?}"
        );

        let dispatched_b = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(60_061),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                workspace_root: Some(dir_b.path().to_string_lossy().into_owned()),
                force: false,
            },
            &tm,
            &db,
            &sr_default,
            &bus,
            &pool,
        );
        assert!(
            matches!(dispatched_b, Response::SweepDispatched { .. }),
            "expected SweepDispatched for repo B, got: {dispatched_b:?}"
        );

        // Fleet-wide fan-out: no `workspace_root`, just `all_workspaces: true`.
        let listed_all = handle_request(
            Request::ListSweeps {
                state_filter: None,
                workspace_root: None,
                all_workspaces: true,
            },
            &tm,
            &db,
            &sr_default,
            &bus,
            &pool,
        );
        match listed_all {
            Response::SweepList { sweeps } => {
                assert_eq!(sweeps.len(), 2, "fan-out must see both repos' sweeps");
                let repos: std::collections::BTreeSet<_> =
                    sweeps.iter().map(|s| s.repo.clone()).collect();
                assert_eq!(
                    repos,
                    std::collections::BTreeSet::from([
                        Some(dir_a.path().display().to_string()),
                        Some(dir_b.path().display().to_string()),
                    ]),
                    "each SweepInfo must carry its owning repo, no repo omitted"
                );
            }
            other => panic!("Expected SweepList, got: {other:?}"),
        }
    }

    /// Regression guard: `all_workspaces` absent/`false` reproduces
    /// byte-for-byte pre-#6006 `workspace_root`-scoped (or
    /// default-workspace-only) behavior even when multiple workspaces are
    /// registered and populated — the fan-out is strictly opt-in, never a
    /// reinterpretation of the existing `None`/absent `workspace_root`
    /// contract. Also asserts an explicit `workspace_root` still scopes to
    /// that one repo when `all_workspaces` is left at its default.
    #[test]
    #[serial_test::serial]
    fn test_list_sweeps_all_workspaces_false_preserves_single_workspace_behavior() {
        let (tm, db, _, bus) = setup_test_context();

        let (sr_default, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
        let (sr_b, dir_b, _rec_b) = setup_sweep_registry_in_tempdir();
        let root_a = crate::workspace_registry::normalize_path(dir_a.path());
        let root_b = crate::workspace_registry::normalize_path(dir_b.path());

        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(root_a, sr_default.clone());
        pool.seed(root_b.clone(), sr_b.clone());

        let _guard = seed_temp_registry(&[dir_a.path(), dir_b.path()]);

        // Dispatch into repo B only.
        let dispatched_b = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(60_062),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                workspace_root: Some(dir_b.path().to_string_lossy().into_owned()),
                force: false,
            },
            &tm,
            &db,
            &sr_default,
            &bus,
            &pool,
        );
        assert!(matches!(dispatched_b, Response::SweepDispatched { .. }));

        // Default (`workspace_root: None`, `all_workspaces: false`) must NOT
        // see repo B's sweep, exactly as before #6006 — even though repo B is
        // now registered and populated.
        let listed_default = handle_request(
            Request::ListSweeps {
                state_filter: None,
                workspace_root: None,
                all_workspaces: false,
            },
            &tm,
            &db,
            &sr_default,
            &bus,
            &pool,
        );
        match listed_default {
            Response::SweepList { sweeps } => assert!(
                sweeps.is_empty(),
                "default-only listing must ignore repo B's sweep even though it exists"
            ),
            other => panic!("Expected SweepList, got: {other:?}"),
        }

        // An explicit `workspace_root` still scopes to that one repo when
        // `all_workspaces` is left at its default.
        let listed_b = handle_request(
            Request::ListSweeps {
                state_filter: None,
                workspace_root: Some(dir_b.path().to_string_lossy().into_owned()),
                all_workspaces: false,
            },
            &tm,
            &db,
            &sr_default,
            &bus,
            &pool,
        );
        match listed_b {
            Response::SweepList { sweeps } => {
                assert_eq!(sweeps.len(), 1, "explicit workspace_root still scopes to repo B");
            }
            other => panic!("Expected SweepList, got: {other:?}"),
        }
    }

    /// `all_workspaces: true` and an explicit `workspace_root` are mutually
    /// exclusive by design — the flag always wins. Repo A's sweep is still
    /// visible in the fan-out even though `workspace_root` names repo B.
    #[test]
    #[serial_test::serial]
    fn test_list_sweeps_all_workspaces_true_ignores_explicit_workspace_root() {
        let (tm, db, _, bus) = setup_test_context();

        let (sr_default, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
        let (sr_b, dir_b, _rec_b) = setup_sweep_registry_in_tempdir();
        let root_a = crate::workspace_registry::normalize_path(dir_a.path());
        let root_b = crate::workspace_registry::normalize_path(dir_b.path());

        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(root_a, sr_default.clone());
        pool.seed(root_b.clone(), sr_b.clone());

        let _guard = seed_temp_registry(&[dir_a.path(), dir_b.path()]);

        let dispatched_a = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(60_063),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                workspace_root: Some(dir_a.path().to_string_lossy().into_owned()),
                force: false,
            },
            &tm,
            &db,
            &sr_default,
            &bus,
            &pool,
        );
        assert!(matches!(dispatched_a, Response::SweepDispatched { .. }));

        // `workspace_root` names repo B, but `all_workspaces: true` wins —
        // repo A's sweep is still visible in the aggregated response.
        let listed = handle_request(
            Request::ListSweeps {
                state_filter: None,
                workspace_root: Some(dir_b.path().to_string_lossy().into_owned()),
                all_workspaces: true,
            },
            &tm,
            &db,
            &sr_default,
            &bus,
            &pool,
        );
        match listed {
            Response::SweepList { sweeps } => {
                assert_eq!(
                    sweeps.len(),
                    1,
                    "fan-out must include repo A's sweep despite workspace_root naming repo B"
                );
                assert_eq!(
                    sweeps[0].repo.as_deref(),
                    Some(dir_a.path().display().to_string().as_str())
                );
            }
            other => panic!("Expected SweepList, got: {other:?}"),
        }
    }

    // ===== Dispatch-path workspace resolution (#4299) =====

    /// Registers `path` as the sole workspace at a temp registry file (via
    /// [`crate::workspace_registry::REGISTRY_PATH_ENV`]) and returns a guard
    /// that clears the env var on drop, so `WorkspaceRegistry::load_default()`
    /// inside `resolve_dispatch_registry` never touches the real
    /// `~/.loom/workspaces.json`.
    struct RegistryEnvGuard {
        _dir: tempfile::TempDir,
    }
    impl Drop for RegistryEnvGuard {
        fn drop(&mut self) {
            std::env::remove_var(crate::workspace_registry::REGISTRY_PATH_ENV);
        }
    }
    fn seed_temp_registry(roots: &[&Path]) -> RegistryEnvGuard {
        let dir = tempdir().unwrap();
        let path = dir.path().join("workspaces.json");
        std::env::set_var(crate::workspace_registry::REGISTRY_PATH_ENV, &path);
        let mut registry = WorkspaceRegistry::default();
        for root in roots {
            registry.add(root, None).unwrap();
        }
        registry.save(&path).unwrap();
        RegistryEnvGuard { _dir: dir }
    }

    /// Issue #4299 — the Linux worker-host shape this issue exists to fix:
    /// exactly one workspace is registered and it is NOT the daemon's seeded
    /// default (its cwd). An absent `workspace_root` on `DispatchSweep` must
    /// still target the single registration, not the unregistered default.
    #[test]
    #[serial_test::serial]
    fn test_dispatch_sweep_absent_workspace_root_targets_single_registration() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr_default, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
        let (sr_b, dir_b, _rec_b) = setup_sweep_registry_in_tempdir();
        let root_a = crate::workspace_registry::normalize_path(dir_a.path());
        let root_b = crate::workspace_registry::normalize_path(dir_b.path());

        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(root_a, sr_default.clone());
        pool.seed(root_b.clone(), sr_b.clone());

        // Registry names ONLY repo B — repo A (the seeded default) is
        // unregistered, mirroring a machine-checkout daemon cwd with one
        // registered product repo.
        let _guard = seed_temp_registry(&[dir_b.path()]);

        let dispatched = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(4299),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                workspace_root: None,
                force: false,
            },
            &tm,
            &db,
            &sr_default,
            &bus,
            &pool,
        );
        assert!(
            matches!(dispatched, Response::SweepDispatched { .. }),
            "expected SweepDispatched, got: {dispatched:?}"
        );

        // Repo B's registry sees the dispatched sweep...
        let listed_b = handle_request(
            Request::ListSweeps {
                state_filter: None,
                workspace_root: Some(dir_b.path().to_string_lossy().into_owned()),
                all_workspaces: false,
            },
            &tm,
            &db,
            &sr_default,
            &bus,
            &pool,
        );
        match listed_b {
            Response::SweepList { sweeps } => {
                assert_eq!(
                    sweeps.len(),
                    1,
                    "the single registered workspace must receive the sweep"
                )
            }
            other => panic!("Expected SweepList, got: {other:?}"),
        }

        // ...and the unregistered default (repo A / daemon cwd) does NOT.
        let listed_default = handle_request(
            Request::ListSweeps {
                state_filter: None,
                workspace_root: None,
                all_workspaces: false,
            },
            &tm,
            &db,
            &sr_default,
            &bus,
            &pool,
        );
        match listed_default {
            Response::SweepList { sweeps } => assert!(
                sweeps.is_empty(),
                "the daemon's own (unregistered) cwd must NOT receive the sweep"
            ),
            other => panic!("Expected SweepList, got: {other:?}"),
        }
    }

    /// Issue #4299 — with multiple registered workspaces and a seeded default
    /// that is itself unregistered, an absent `workspace_root` must return a
    /// structured ambiguity error naming every registered root, never a silent
    /// cwd fallback.
    #[test]
    #[serial_test::serial]
    fn test_dispatch_sweep_ambiguous_registry_errors_without_explicit_param() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr_default, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
        let (sr_b, dir_b, _rec_b) = setup_sweep_registry_in_tempdir();
        let (sr_c, dir_c, _rec_c) = setup_sweep_registry_in_tempdir();
        let root_a = crate::workspace_registry::normalize_path(dir_a.path());
        let root_b = crate::workspace_registry::normalize_path(dir_b.path());
        let root_c = crate::workspace_registry::normalize_path(dir_c.path());

        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(root_a, sr_default.clone());
        pool.seed(root_b.clone(), sr_b.clone());
        pool.seed(root_c.clone(), sr_c.clone());

        let _guard = seed_temp_registry(&[dir_b.path(), dir_c.path()]);

        let dispatched = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(4299),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                workspace_root: None,
                force: false,
            },
            &tm,
            &db,
            &sr_default,
            &bus,
            &pool,
        );
        match dispatched {
            Response::StructuredError(err) => {
                assert_eq!(err.code.0, crate::errors::ErrorCode::CONFIG_WORKSPACE_AMBIGUOUS);
                assert!(
                    err.message.contains(&root_b.display().to_string())
                        && err.message.contains(&root_c.display().to_string()),
                    "ambiguity error must name every registered root, got: {}",
                    err.message
                );
            }
            other => panic!("Expected StructuredError, got: {other:?}"),
        }
    }

    /// Issue #5210, AC #1 — an explicit `workspace_root` that names a path the
    /// daemon has never registered must return a structured
    /// `workspace_unregistered` error naming both the offending path and every
    /// registered root, instead of silently provisioning a registry for an
    /// arbitrary directory via `get_or_provision`.
    #[test]
    #[serial_test::serial]
    fn test_dispatch_sweep_unregistered_explicit_workspace_root_is_structured_error() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr_default, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
        let dir_unregistered = tempdir().unwrap();
        let root_a = crate::workspace_registry::normalize_path(dir_a.path());
        let unregistered = crate::workspace_registry::normalize_path(dir_unregistered.path());

        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(root_a, sr_default.clone());

        // Only repo A is registered; `dir_unregistered` is never added.
        let _guard = seed_temp_registry(&[dir_a.path()]);

        let dispatched = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(5210),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                workspace_root: Some(dir_unregistered.path().to_string_lossy().into_owned()),
                force: false,
            },
            &tm,
            &db,
            &sr_default,
            &bus,
            &pool,
        );
        match dispatched {
            Response::StructuredError(err) => {
                assert_eq!(err.code.0, crate::errors::ErrorCode::CONFIG_WORKSPACE_UNREGISTERED);
                assert!(
                    err.message.contains(&unregistered.display().to_string()),
                    "error must name the offending unregistered path, got: {}",
                    err.message
                );
                assert!(
                    err.message.contains(&dir_a.path().display().to_string())
                        || err
                            .details
                            .as_ref()
                            .and_then(|d| d.get("registered"))
                            .map(|v| v.to_string())
                            .unwrap_or_default()
                            .contains(&dir_a.path().display().to_string()),
                    "error must list the registered roots, got message={} details={:?}",
                    err.message,
                    err.details
                );
            }
            other => panic!("Expected StructuredError, got: {other:?}"),
        }
    }

    /// Issue #5345 — the `workspace_unregistered` recovery hint must branch on
    /// the **target** root's own `daemon.delegatedTo`, not the daemon
    /// process's cwd: an unregistered target that itself declares delegation
    /// gets pointed at its delegate repo instead of the generic "run
    /// `workspace add` here" suggestion. This is the triggering incident
    /// (dispatch into a delegated repo hitting this exact error) end-to-end.
    #[test]
    #[serial_test::serial]
    fn test_dispatch_sweep_unregistered_delegated_target_hint_names_delegate() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr_default, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
        let dir_unregistered = tempdir().unwrap();
        std::fs::create_dir_all(dir_unregistered.path().join(".loom")).unwrap();
        std::fs::write(
            dir_unregistered.path().join(".loom").join("config.json"),
            r#"{"daemon": {"delegatedTo": "/Users/alice/GitHub/other-repo"}}"#,
        )
        .unwrap();
        let root_a = crate::workspace_registry::normalize_path(dir_a.path());

        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(root_a, sr_default.clone());

        // Only repo A is registered; `dir_unregistered` (delegated) is never added.
        let _guard = seed_temp_registry(&[dir_a.path()]);

        let dispatched = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(5345),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                workspace_root: Some(dir_unregistered.path().to_string_lossy().into_owned()),
                force: false,
            },
            &tm,
            &db,
            &sr_default,
            &bus,
            &pool,
        );
        match dispatched {
            Response::StructuredError(err) => {
                assert_eq!(err.code.0, crate::errors::ErrorCode::CONFIG_WORKSPACE_UNREGISTERED);
                let hint = err.recovery_hint.expect("recovery hint must be present");
                assert!(
                    hint.contains("/Users/alice/GitHub/other-repo"),
                    "recovery hint must name the target's own delegate, got: {hint}"
                );
            }
            other => panic!("Expected StructuredError, got: {other:?}"),
        }
    }

    /// Issue #5345 AC — `daemon.delegatedTo` gates only the CLI admin
    /// entry points (`workspace add/set-priority/remove`, `tokens
    /// bootstrap`); `dispatch_sweep` into an **already-registered** target
    /// that happens to declare `daemon.delegatedTo` must dispatch exactly as
    /// it would without the key present — daemon-client actions are
    /// unaffected by delegation.
    #[test]
    #[serial_test::serial]
    fn test_dispatch_sweep_succeeds_into_a_registered_delegated_workspace() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr_default, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
        let (sr_b, dir_b, _rec_b) = setup_sweep_registry_in_tempdir();
        std::fs::write(
            dir_b.path().join(".loom").join("config.json"),
            r#"{"daemon": {"delegatedTo": "/Users/alice/GitHub/other-repo"}}"#,
        )
        .unwrap();
        let root_a = crate::workspace_registry::normalize_path(dir_a.path());
        let root_b = crate::workspace_registry::normalize_path(dir_b.path());

        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(root_a, sr_default.clone());
        pool.seed(root_b, sr_b.clone());

        // Repo B (delegated) is registered like any other managed workspace.
        let _guard = seed_temp_registry(&[dir_b.path()]);

        let dispatched = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(5345),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                workspace_root: Some(dir_b.path().to_string_lossy().into_owned()),
                force: false,
            },
            &tm,
            &db,
            &sr_default,
            &bus,
            &pool,
        );
        assert!(
            matches!(dispatched, Response::SweepDispatched { .. }),
            "dispatch_sweep must be unaffected by daemon.delegatedTo, got: {dispatched:?}"
        );
    }

    /// Issue #5210, AC #2/#3 — once an unregistered root is filtered out by AC
    /// #1, a spawn failure unrelated to registration (a *registered* workspace
    /// missing `spawn-worker.sh`) must still surface `resolve_spawn_bin`'s
    /// specific message through `dispatch_sweep failed: {e:#}` — distinct from
    /// the AC #1 registration error and no longer collapsed into the opaque
    /// "failed to spawn sweep child" outer context alone.
    #[test]
    #[serial_test::serial]
    fn test_dispatch_sweep_registered_workspace_missing_spawn_bin_surfaces_inner_error() {
        let (tm, db, _, bus) = setup_test_context();
        let dir = tempdir().unwrap();
        // Deliberately do NOT create `.loom/scripts/spawn-worker.sh` (or
        // `defaults/scripts/spawn-worker.sh`), and leave `spawn_bin` unset —
        // this workspace IS registered, but is misconfigured.
        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.skip_label_flip = true; // bypass runtime admission / #4027 guard, not spawn_bin resolution
        config.journal_path = Some(dir.path().join("test-sweeps-journal.json"));
        let sr = Arc::new(Mutex::new(SweepRegistry::new(config)));

        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        let root = crate::workspace_registry::normalize_path(dir.path());
        pool.seed(root, sr.clone());
        let _guard = seed_temp_registry(&[dir.path()]);

        // Isolate from a stray real `LOOM_SWEEP_SPAWN_BIN` in the test env.
        std::env::remove_var(crate::sweep_registry::SPAWN_BIN_ENV);

        let dispatched = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(5210),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                workspace_root: Some(dir.path().to_string_lossy().into_owned()),
                force: false,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &pool,
        );
        match dispatched {
            Response::Error { message } => {
                assert!(
                    message.contains("spawn-worker.sh not found under"),
                    "expected the specific resolve_spawn_bin message to survive `{{e:#}}`, got: {message}"
                );
                assert!(
                    message.contains("failed to spawn sweep child"),
                    "outer context should still be present alongside the inner detail, got: {message}"
                );
            }
            other => panic!("Expected Response::Error, got: {other:?}"),
        }
    }

    /// Issue #4299 — the #4027 wedge-loop guard must evaluate the *resolved*
    /// workspace (the single registration), not the daemon's own unregistered
    /// cwd: the error names repo B's root, not repo A's.
    #[test]
    #[serial_test::serial]
    fn test_dispatch_sweep_wedge_guard_names_resolved_workspace_not_cwd() {
        let (tm, db, _, bus) = setup_test_context();
        let dir_a = tempdir().unwrap();
        let dir_b = tempdir().unwrap();
        // Runtime admission is the first dispatch decision. Install a valid
        // zero-config Claude surface in both candidate roots so this fixture
        // reaches (and continues to assert) the downstream workspace-command
        // guard rather than bypassing admission.
        for root in [dir_a.path(), dir_b.path()] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir_all(root.join(".loom/roles")).unwrap();
            std::fs::create_dir_all(root.join(".loom/runtimes")).unwrap();
            std::fs::create_dir_all(root.join(".loom/scripts")).unwrap();
            std::fs::write(
                root.join(".loom/roles/builder.json"),
                r#"{"runtimeRequirements":["worktreeIsolation","mcp"]}"#,
            )
            .unwrap();
            std::fs::write(
                root.join(".loom/runtimes/claude.json"),
                r#"{"runtime":"claude","capabilities":{"worktreeIsolation":"yes","mcp":"yes"}}"#,
            )
            .unwrap();
            let adapter = root.join(".loom/scripts/spawn-claude.sh");
            std::fs::write(&adapter, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(adapter, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Neither workspace has `.claude/commands/loom/sweep.md`, and
        // `skip_label_flip` is left at its default `false` so the #4027 guard
        // is actually evaluated (unlike `setup_sweep_registry_in_tempdir`,
        // which sets `skip_label_flip = true` for its other fixtures).
        let sr_default = Arc::new(Mutex::new(SweepRegistry::new(SweepRegistryConfig::new(
            dir_a.path().to_path_buf(),
        ))));
        let sr_b = Arc::new(Mutex::new(SweepRegistry::new(SweepRegistryConfig::new(
            dir_b.path().to_path_buf(),
        ))));
        let root_a = crate::workspace_registry::normalize_path(dir_a.path());
        let root_b = crate::workspace_registry::normalize_path(dir_b.path());

        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(root_a.clone(), sr_default.clone());
        pool.seed(root_b.clone(), sr_b.clone());

        let _guard = seed_temp_registry(&[dir_b.path()]);

        let dispatched = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(4299),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                workspace_root: None,
                force: false,
            },
            &tm,
            &db,
            &sr_default,
            &bus,
            &pool,
        );
        match dispatched {
            Response::Error { message } => {
                // The guard message embeds `SweepRegistryConfig::workspace_root`
                // verbatim, which here is the *raw* tempdir path each `sr_*` was
                // constructed with (not the canonicalized `root_a`/`root_b` used
                // as the pool's dedup key) — assert against that raw form.
                assert!(
                    message.contains(&dir_b.path().display().to_string()),
                    "wedge-guard error must name the resolved workspace (repo B), got: {message}"
                );
                assert!(
                    !message.contains(&dir_a.path().display().to_string()),
                    "wedge-guard error must NOT name the daemon's own unregistered cwd, got: {message}"
                );
            }
            other => panic!("Expected Error (wedge-guard refusal), got: {other:?}"),
        }
    }

    #[test]
    fn runtime_rejection_response_is_structured_and_secret_free_on_the_wire() {
        let response = Response::RuntimeRejected(crate::runtime_admission::RuntimeRejection {
            role: "sweep-lifecycle".into(),
            runtime: "codex".into(),
            source: crate::runtime_admission::RuntimeSource::DefaultConfig,
            unmet_capabilities: vec!["worktreeIsolation".into()],
            reason: "unmet capabilities: worktreeIsolation".into(),
        });
        let wire = serde_json::to_string(&response).unwrap();
        assert!(wire.contains("\"type\":\"RuntimeRejected\""));
        assert!(wire.contains("\"source\":\"default-config\""));
        assert!(wire.contains("\"unmet_capabilities\":[\"worktreeIsolation\"]"));
        assert!(!wire.contains("oauth"));
        assert!(!wire.contains("token"));
        assert!(matches!(
            serde_json::from_str::<Response>(&wire).unwrap(),
            Response::RuntimeRejected(_)
        ));
    }

    #[test]
    #[serial_test::serial]
    fn test_handle_request_dispatch_sweep_happy_path() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
        // #4299: pin the registry to empty so `workspace_root: None` resolution
        // is deterministic regardless of the host's real registry.
        let _registry_guard = seed_temp_registry(&[]);

        let response = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(2024),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                workspace_root: None,
                force: false,
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
                all_workspaces: false,
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
        // #4299: pin the registry to empty so `workspace_root: None` resolution
        // is deterministic regardless of the host's real registry.
        let _registry_guard = seed_temp_registry(&[]);

        let response = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(3964),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                workspace_root: None,
                force: false,
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

    /// Issue #4666: `Request::DispatchSweep` previously consulted only the
    /// host-distress breaker (`host_breaker`), never the GitHub rate-limit
    /// breaker (`rate_limit_breaker`, #4429/#4440) — so a brand-new dispatch
    /// could still land while the shared forge API budget was in a known
    /// cooldown. These tests exercise [`rate_limit_dispatch_refusal`] — the
    /// exact decision `handle_request`'s `DispatchSweep` arm makes — directly
    /// with a manually constructed [`crate::rate_limit_breaker::RateLimitSnapshot`]
    /// rather than through the process-global breaker: see that function's
    /// doc comment for why (registering the real global would permanently
    /// poison every other `DispatchSweep` test sharing this test binary under
    /// plain `cargo test --workspace`, which this repo's CI still runs
    /// alongside `cargo nextest run`).
    mod rate_limit_dispatch_refusal_tests {
        use super::*;

        fn suppressed_snapshot(
            cooldown_until: Option<chrono::DateTime<Utc>>,
        ) -> crate::rate_limit_breaker::RateLimitSnapshot {
            crate::rate_limit_breaker::RateLimitSnapshot {
                enabled: true,
                phase: crate::rate_limit_breaker::BreakerPhase::Cooldown,
                suppressed: true,
                source: Some("test_source".to_string()),
                tripped_at: Some(Utc::now()),
                cooldown_until,
                trips_total: 1,
                core_remaining: None,
                graphql_remaining: None,
                budget_probed_at: None,
            }
        }

        /// No breaker registered at all (`global_snapshot()` returns `None`)
        /// must be a complete no-op — zero behavior change for daemons that
        /// never enabled the breaker.
        #[test]
        fn no_snapshot_never_refuses() {
            let kind = SweepKind::Issue(4666);
            assert!(rate_limit_dispatch_refusal(&kind, None, false).is_none());
            assert!(rate_limit_dispatch_refusal(&kind, None, true).is_none());
        }

        /// A registered breaker that is Closed (not suppressed) must not
        /// refuse either.
        #[test]
        fn closed_breaker_never_refuses() {
            let kind = SweepKind::Issue(4666);
            let snap = crate::rate_limit_breaker::RateLimitSnapshot {
                enabled: true,
                phase: crate::rate_limit_breaker::BreakerPhase::Closed,
                suppressed: false,
                source: None,
                tripped_at: None,
                cooldown_until: None,
                trips_total: 0,
                core_remaining: None,
                graphql_remaining: None,
                budget_probed_at: None,
            };
            assert!(rate_limit_dispatch_refusal(&kind, Some(&snap), false).is_none());
        }

        /// The core #4666 fix: a suppressed (Cooldown) snapshot refuses the
        /// dispatch by default, with a message that (a) names the rate-limit
        /// breaker and its cooldown release time, and (b) does not reuse the
        /// host-distress breaker's wording — the two must never be conflated
        /// since they have different root causes and different remediations.
        #[test]
        fn suppressed_breaker_refuses_with_distinct_message() {
            let kind = SweepKind::Issue(4666);
            let until = Utc::now() + chrono::Duration::seconds(600);
            let snap = suppressed_snapshot(Some(until));

            let response = rate_limit_dispatch_refusal(&kind, Some(&snap), false);
            match response {
                Some(Response::Error { message }) => {
                    assert!(
                        message.contains("rate-limit"),
                        "expected the rate-limit breaker refusal message, got: {message}"
                    );
                    assert!(
                        message.contains(&until.to_string()),
                        "expected the cooldown release time in the message, got: {message}"
                    );
                    assert!(
                        !message.contains("host circuit breaker")
                            && !message.contains("host distress"),
                        "rate-limit refusal must not be conflated with the host-distress \
                         breaker's wording: {message}"
                    );
                }
                other => panic!("Expected Some(Response::Error), got: {other:?}"),
            }
        }

        /// A suppressed snapshot with no probed cooldown time yet must still
        /// refuse, with an informative (not panicking/empty) fallback phrase.
        #[test]
        fn suppressed_breaker_without_cooldown_time_still_refuses() {
            let kind = SweepKind::Issue(4666);
            let snap = suppressed_snapshot(None);
            let response = rate_limit_dispatch_refusal(&kind, Some(&snap), false);
            assert!(
                matches!(response, Some(Response::Error { .. })),
                "expected a refusal even without a known cooldown release time, got: {response:?}"
            );
        }

        /// `force: true` overrides the rate-limit breaker independently of
        /// the host-distress breaker's own `force` handling, even while the
        /// snapshot itself remains suppressed throughout.
        #[test]
        fn force_true_overrides_suppressed_breaker() {
            let kind = SweepKind::Issue(4666);
            let snap = suppressed_snapshot(Some(Utc::now() + chrono::Duration::seconds(600)));
            assert!(
                rate_limit_dispatch_refusal(&kind, Some(&snap), true).is_none(),
                "force: true must bypass the rate-limit breaker refusal"
            );
        }
    }

    /// Issue #5340: `Request::DispatchSweep` — routed through `handle_client`,
    /// not `handle_request` — is the one dispatch producer whose admission was
    /// never actually gated on `DrainState`'s flag, unlike the work-finder,
    /// epic supervisor, and role runner, which all read it in-process each
    /// tick. These tests exercise [`drain_dispatch_refusal`] — the exact
    /// decision `handle_client` makes before ever calling `handle_request` —
    /// directly with a plain `bool`, matching the
    /// [`rate_limit_dispatch_refusal_tests`] pattern just above (a real
    /// `DrainState` is a `Mutex`-guarded singleton per daemon process, not
    /// something a unit test wants to mutate to exercise one decision).
    mod drain_dispatch_refusal_tests {
        use super::*;

        /// Not draining ⇒ never refuses, regardless of `force`. This is the
        /// overwhelmingly common case (no drain in progress) and must be a
        /// complete no-op.
        #[test]
        fn not_draining_never_refuses() {
            let kind = SweepKind::Issue(5340);
            assert!(drain_dispatch_refusal(&kind, false, false).is_none());
            assert!(drain_dispatch_refusal(&kind, false, true).is_none());
        }

        /// The core #5340 fix: an active drain refuses a plain (non-forced)
        /// explicit dispatch, with a message that names the drain, points at
        /// `loom-daemon status` to check progress, and `restart --abort-drain`
        /// to resume dispatch immediately.
        #[test]
        fn draining_refuses_without_force() {
            let kind = SweepKind::Issue(5340);
            let response = drain_dispatch_refusal(&kind, true, false);
            match response {
                Some(Response::Error { message }) => {
                    assert!(
                        message.contains("drain"),
                        "expected the drain refusal message, got: {message}"
                    );
                    assert!(
                        message.contains("loom-daemon status"),
                        "expected a pointer to checking status, got: {message}"
                    );
                    assert!(
                        message.contains("--abort-drain"),
                        "expected the abort-drain escape hatch, got: {message}"
                    );
                }
                other => panic!("Expected Some(Response::Error), got: {other:?}"),
            }
        }

        /// `force: true` overrides the drain refusal independently of the
        /// host-distress/rate-limit breakers' own `force` handling, even while
        /// `is_draining` remains `true` throughout — an operator can still push
        /// an urgent dispatch through a drain window.
        #[test]
        fn force_true_overrides_active_drain() {
            let kind = SweepKind::Issue(5340);
            assert!(
                drain_dispatch_refusal(&kind, true, true).is_none(),
                "force: true must bypass the drain refusal"
            );
        }
    }

    /// Issue #5342: `Request::DispatchSweep` accepts `SweepKind::PrSet` and
    /// spawns it through the exact same `handle_request` arm as `Issue`
    /// (no protocol change — the arm already forwarded `kind` generically to
    /// `sr.dispatch`).
    #[test]
    #[serial_test::serial]
    fn test_handle_request_dispatch_sweep_accepts_prset() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
        // #4299: pin the registry to empty so `workspace_root: None` resolution
        // is deterministic regardless of the host's real registry.
        let _registry_guard = seed_temp_registry(&[]);

        let response = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::PrSet(vec![100, 200]),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                workspace_root: None,
                force: false,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::SweepDispatched { sweep_id, .. } => {
                assert!(
                    sweep_id.contains("prs"),
                    "expected a PrSet-shaped sweep id; got: {sweep_id}"
                );
            }
            other => panic!("Expected SweepDispatched, got: {other:?}"),
        }
    }

    // ===== DispatchSweep IPC-level burst behavior (Issue #6592) =====

    /// Build a `SweepRegistry` whose fixture `spawn-claude.sh` stays alive
    /// and logs its account selection only after `poll_delay` — so
    /// `poll_and_classify_spawned_child`'s wait genuinely blocks for that
    /// long, the same fixture shape `dispatch.rs`'s
    /// `concurrent_issue_dispatches_do_not_serialize_on_the_account_selection_poll`
    /// test uses at the `SweepRegistry` layer. Registers `dir`'s path as the
    /// sole entry in a temp workspace registry (via `seed_temp_registry`, the
    /// caller's job — kept out of this helper so the returned guard's
    /// lifetime is the caller's to manage) is NOT done here; see call sites.
    fn slow_poll_sweep_registry_in_tempdir(
        poll_delay: Duration,
    ) -> (Arc<Mutex<SweepRegistry>>, tempfile::TempDir) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let scripts_dir = dir.path().join(".loom").join("scripts");
        std::fs::create_dir_all(&scripts_dir).unwrap();
        let fake_bin = scripts_dir.join("spawn-claude.sh");
        let script = format!(
            "#!/usr/bin/env bash\nset -euo pipefail\nsleep {:.2}\n\
             echo \"spawn-claude: using OAuth account 'agent-ipc-burst' (mode=random)\" >&2\n\
             sleep 5\n",
            poll_delay.as_secs_f64()
        );
        std::fs::write(&fake_bin, script).unwrap();
        let mut perms = std::fs::metadata(&fake_bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_bin, perms).unwrap();

        let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
        config.spawn_bin = Some(fake_bin);
        config.skip_label_flip = true;
        config.journal_path = Some(dir.path().join("test-sweeps-journal.json"));
        let sr = Arc::new(Mutex::new(SweepRegistry::new(config)));
        (sr, dir)
    }

    /// Issue #6592, AC2: a burst of 10+ concurrent `dispatch_sweep` calls
    /// must all ack well under the client's 30s deadline. Drives the actual
    /// IPC-layer entry point (`dispatch_sweep_nonblocking`, what
    /// `handle_client` calls for a real `DispatchSweep` request) concurrently
    /// via `tokio::spawn`, against a fixture whose spawn script blocks the
    /// account-selection poll for `POLL_DELAY` — proving the burst does not
    /// serialize behind the registry mutex (which would take
    /// `BURST * POLL_DELAY`, ~7s for 10x700ms, dwarfed by 30s only by
    /// coincidence of this test's chosen delay — the point is the burst
    /// completes in close to ONE delay, not N).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[serial_test::serial]
    async fn dispatch_sweep_nonblocking_burst_acks_well_under_the_client_deadline() {
        const BURST: u32 = 10;
        let poll_delay = Duration::from_millis(700);
        let (sr, dir) = slow_poll_sweep_registry_in_tempdir(poll_delay);
        let _guard = seed_temp_registry(&[dir.path()]);
        let bus = Arc::new(EventBus::new());
        let pool = Arc::new(WorkspacePool::new(bus.clone(), test_runtime_handle()));

        let start = std::time::Instant::now();
        let mut handles = Vec::new();
        for i in 0..BURST {
            let sr = sr.clone();
            let bus = bus.clone();
            let pool = pool.clone();
            handles.push(tokio::spawn(async move {
                dispatch_sweep_nonblocking(
                    &sr,
                    &pool,
                    &bus,
                    SweepKind::Issue(83_000 + i),
                    None,
                    None,
                    None,
                    None,
                    None,
                    false,
                )
                .await
            }));
        }
        let mut sweep_ids = Vec::new();
        for h in handles {
            match h.await.expect("dispatch task panicked") {
                Response::SweepDispatched { sweep_id, .. } => sweep_ids.push(sweep_id),
                other => panic!("Expected SweepDispatched, got: {other:?}"),
            }
        }
        let elapsed = start.elapsed();
        assert_eq!(sweep_ids.len(), BURST as usize);

        let serialized_bound = poll_delay * BURST;
        assert!(
            elapsed < serialized_bound / 2,
            "burst of {BURST} concurrent dispatch_sweep calls took {elapsed:?} — looks \
             serialized behind the registry mutex (serialized bound ~{serialized_bound:?})"
        );
        assert!(
            elapsed < Duration::from_secs(30),
            "burst took {elapsed:?}, at or over the 30s client ack deadline (AC2)"
        );

        for id in &sweep_ids {
            let mut sr = sr.lock().unwrap();
            let _ = sr.cancel(id, Duration::from_millis(50));
        }
    }

    /// Issue #6592, AC1/AC2's second half: a `ListSweeps` request issued
    /// WHILE a `DispatchSweep` burst is in flight must not be starved behind
    /// it. Runs `ListSweeps` (via the ordinary synchronous `handle_request`,
    /// on a `spawn_blocking` thread — exactly how it reaches the registry
    /// mutex in production) concurrently with the same burst as the test
    /// above, and asserts it returns quickly rather than waiting out the
    /// whole burst.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[serial_test::serial]
    async fn list_sweeps_is_not_starved_behind_a_concurrent_dispatch_burst() {
        const BURST: u32 = 10;
        let poll_delay = Duration::from_millis(700);
        let (sr, dir) = slow_poll_sweep_registry_in_tempdir(poll_delay);
        let _guard = seed_temp_registry(&[dir.path()]);
        let bus = Arc::new(EventBus::new());
        let pool = Arc::new(WorkspacePool::new(bus.clone(), test_runtime_handle()));

        let mut handles = Vec::new();
        for i in 0..BURST {
            let sr = sr.clone();
            let bus = bus.clone();
            let pool = pool.clone();
            handles.push(tokio::spawn(async move {
                dispatch_sweep_nonblocking(
                    &sr,
                    &pool,
                    &bus,
                    SweepKind::Issue(84_000 + i),
                    None,
                    None,
                    None,
                    None,
                    None,
                    false,
                )
                .await
            }));
        }

        // Give the burst a moment to actually acquire the registry mutex at
        // least once (begin_issue_dispatch's lock-scoped phase), so this
        // `ListSweeps` genuinely races a live burst rather than running
        // before it starts.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let list_start = std::time::Instant::now();
        let sr_for_list = sr.clone();
        let bus_for_list = bus.clone();
        let pool_for_list = pool.clone();
        let list_response = tokio::task::spawn_blocking(move || {
            handle_request(
                Request::ListSweeps {
                    state_filter: None,
                    workspace_root: None,
                    all_workspaces: false,
                },
                &Arc::new(Mutex::new(TerminalManager::new())),
                &Arc::new(Mutex::new(
                    ActivityDb::new(tempdir().unwrap().path().join("list-sweeps-activity.db"))
                        .unwrap(),
                )),
                &sr_for_list,
                &bus_for_list,
                &pool_for_list,
            )
        })
        .await
        .expect("ListSweeps task panicked");
        let list_elapsed = list_start.elapsed();
        assert!(
            matches!(list_response, Response::SweepList { .. }),
            "expected SweepList, got: {list_response:?}"
        );

        // Well under the burst's serialized-would-be duration (~7s for
        // 10x700ms) — a starved ListSweeps would take close to that; a
        // healthy one returns in low milliseconds regardless of the burst.
        assert!(
            list_elapsed < poll_delay * BURST / 2,
            "ListSweeps took {list_elapsed:?} while a dispatch_sweep burst was in flight — \
             looks starved behind the registry mutex"
        );

        for h in handles {
            if let Ok(Response::SweepDispatched { sweep_id, .. }) = h.await {
                let mut sr = sr.lock().unwrap();
                let _ = sr.cancel(&sweep_id, Duration::from_millis(50));
            }
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
                force: _,
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
            force: false,
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
                force: _,
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
            force: false,
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
            force: false,
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
            force: false,
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
            force: false,
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

        // Issue #4466: the documented child-published `sweep.issue.{N}.phase`
        // topic is upgraded to the typed `Event::SweepPhase` variant (was
        // previously delivered as `Event::Generic`, which the narration sink
        // never narrated).
        let ev = sub.recv().await.unwrap();
        match ev {
            Event::SweepPhase {
                issue,
                phase,
                pr_number,
                repo,
            } => {
                assert_eq!(issue, 123);
                assert_eq!(phase, "builder");
                assert_eq!(pr_number, None);
                assert_eq!(repo, None);
            }
            other => panic!("Expected SweepPhase event, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_handle_request_publish_event_upgrades_blocker_topic() {
        // Issue #4466: `sweep.issue.{N}.blocker` upgrades to `Event::SweepBlocker`
        // with the full documented payload (incl. the optional `repo`).
        let (tm, db, sr, bus) = setup_test_context();
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        handle_request(
            Request::PublishEvent {
                topic: "sweep.issue.456.blocker".to_string(),
                payload: serde_json::json!({
                    "reason": "needs human decision",
                    "label_added": "loom:operator-only",
                    "repo": "/work/loom",
                }),
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );

        match sub.recv().await.unwrap() {
            Event::SweepBlocker {
                issue,
                reason,
                label_added,
                repo,
            } => {
                assert_eq!(issue, 456);
                assert_eq!(reason, "needs human decision");
                assert_eq!(label_added, "loom:operator-only");
                assert_eq!(repo.as_deref(), Some("/work/loom"));
            }
            other => panic!("Expected SweepBlocker event, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_handle_request_publish_event_phase_carries_pr_and_repo() {
        // Issue #4466: the optional `pr_number` + `repo` fields survive the
        // typed upgrade (the phase narration line appends ` · PR #M open`).
        let (tm, db, sr, bus) = setup_test_context();
        let mut sub = bus.subscribe::<[&str; 0], &str>([]);

        handle_request(
            Request::PublishEvent {
                topic: "sweep.issue.789.phase".to_string(),
                payload: serde_json::json!({
                    "phase": "judge",
                    "pr_number": 501,
                    "repo": "/work/loom",
                }),
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );

        match sub.recv().await.unwrap() {
            Event::SweepPhase {
                issue,
                phase,
                pr_number,
                repo,
            } => {
                assert_eq!(issue, 789);
                assert_eq!(phase, "judge");
                assert_eq!(pr_number, Some(501));
                assert_eq!(repo.as_deref(), Some("/work/loom"));
            }
            other => panic!("Expected SweepPhase event, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_handle_request_publish_event_malformed_and_unknown_stay_generic() {
        // Issue #4466: publish is fire-and-forget advisory — a malformed
        // payload (missing required `phase`), an unknown sweep sub-topic, a
        // non-integer issue segment, and an entirely unrelated topic all stay
        // `Event::Generic` with the payload passed through UNCHANGED.
        let (tm, db, sr, bus) = setup_test_context();

        let cases: &[(&str, serde_json::Value)] = &[
            // Documented topic, but the required `phase` field is missing.
            ("sweep.issue.123.phase", serde_json::json!({"pr_number": 5})),
            // Documented blocker topic, but `label_added` is missing.
            ("sweep.issue.123.blocker", serde_json::json!({"reason": "x"})),
            // Unknown sweep sub-topic.
            ("sweep.issue.123.other", serde_json::json!({"phase": "builder"})),
            // Non-integer issue segment.
            ("sweep.issue.abc.phase", serde_json::json!({"phase": "builder"})),
            // Entirely unrelated topic.
            ("custom.topic", serde_json::json!({"phase": "builder"})),
        ];

        for (topic, payload) in cases {
            let mut sub = bus.subscribe::<[&str; 0], &str>([]);
            handle_request(
                Request::PublishEvent {
                    topic: (*topic).to_string(),
                    payload: payload.clone(),
                },
                &tm,
                &db,
                &sr,
                &bus,
                &test_pool(),
            );
            match sub.recv().await.unwrap() {
                Event::Generic {
                    topic: got_topic,
                    payload: got_payload,
                } => {
                    assert_eq!(&got_topic, topic, "topic preserved for {topic}");
                    assert_eq!(&got_payload, payload, "payload passed through for {topic}");
                }
                other => panic!("Expected Generic event for {topic}, got: {other:?}"),
            }
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

    // ===== RecordDispatchFailure (Issue #6192) =====

    #[test]
    fn test_handle_request_record_dispatch_failure_arms_backoff() {
        // Issue #6192: a build-gate step timeout (or any other script-side
        // caller with no direct registry access) records a failed dispatch
        // via IPC and gets back the resulting consecutive count + window,
        // mirroring the reaper's own automatic bookkeeping (#4485).
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
        let response = handle_request(
            Request::RecordDispatchFailure {
                issue: 6192,
                reason: Some("build-gate timeout: cargo test (1800s elapsed)".to_string()),
                workspace_root: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::DispatchFailureRecorded {
                issue,
                consecutive,
                backoff_secs,
            } => {
                assert_eq!(issue, 6192);
                assert_eq!(consecutive, 1);
                assert!(
                    backoff_secs.is_some_and(|s| s > 0),
                    "expected a positive backoff window (default config is enabled), got: \
                     {backoff_secs:?}"
                );
            }
            other => panic!("Expected DispatchFailureRecorded, got: {other:?}"),
        }
        assert_eq!(sr.lock().unwrap().dispatch_failure_count(6192), 1);
    }

    #[test]
    fn test_handle_request_record_dispatch_failure_accumulates_consecutive() {
        // Two calls for the same issue accumulate — mirrors the reaper
        // calling `record_dispatch_failure` on repeated failed dispatches.
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
        for _ in 0..2 {
            handle_request(
                Request::RecordDispatchFailure {
                    issue: 6193,
                    reason: None,
                    workspace_root: None,
                },
                &tm,
                &db,
                &sr,
                &bus,
                &test_pool(),
            );
        }
        let response = handle_request(
            Request::RecordDispatchFailure {
                issue: 6193,
                reason: None,
                workspace_root: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::DispatchFailureRecorded { consecutive, .. } => {
                assert_eq!(consecutive, 3, "three calls -> three consecutive failures");
            }
            other => panic!("Expected DispatchFailureRecorded, got: {other:?}"),
        }
    }

    #[test]
    fn test_handle_request_record_dispatch_failure_disabled_is_noop() {
        // A repo/operator with the backoff mechanism disabled gets an
        // idempotent no-op: `consecutive` stays 0 and `backoff_secs` is None,
        // never a hard error — mirrors `record_dispatch_failure`'s own early
        // return when `dispatch_backoff_config.enabled` is false.
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
        {
            let mut reg = sr.lock().unwrap();
            let mut cfg = reg.dispatch_backoff_config();
            cfg.enabled = false;
            reg.set_dispatch_backoff_config(cfg);
        }
        let response = handle_request(
            Request::RecordDispatchFailure {
                issue: 6194,
                reason: None,
                workspace_root: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &test_pool(),
        );
        match response {
            Response::DispatchFailureRecorded {
                issue,
                consecutive,
                backoff_secs,
            } => {
                assert_eq!(issue, 6194);
                assert_eq!(consecutive, 0, "disabled backoff never records a state entry");
                assert!(backoff_secs.is_none(), "disabled backoff reports no window");
            }
            other => panic!("Expected DispatchFailureRecorded, got: {other:?}"),
        }
    }

    // ===== ListQuarantines (Issue #4215) =====
    //
    // `workspace_root: None` enumerates every registered workspace (unlike
    // `ClearQuarantine`'s `None` == default-workspace-only), so these tests
    // seed the pool with the default registry at its own workspace root —
    // exactly the way `main.rs` wires `workspace_pool.seed(sweep_workspace,
    // sweep_registry)` in production — and pin `REGISTRY_PATH_ENV` to an empty
    // file so `effective_roots` resolves to exactly `[root]` regardless of any
    // real `~/.loom/workspaces.json` on the host running the test.

    #[test]
    #[serial_test::serial]
    fn test_handle_request_list_quarantines_empty_registry() {
        use crate::workspace_registry::REGISTRY_PATH_ENV;

        let (tm, db, _, bus) = setup_test_context();
        let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
        let root = dir.path().to_path_buf();
        let empty_reg = dir.path().join("no-such-workspaces.json");
        std::env::set_var(REGISTRY_PATH_ENV, &empty_reg);

        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(root, sr.clone());

        let response = handle_request(
            Request::ListQuarantines {
                workspace_root: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &pool,
        );
        std::env::remove_var(REGISTRY_PATH_ENV);

        match response {
            Response::QuarantineList { entries } => {
                assert!(entries.is_empty(), "no quarantines seeded -> empty list");
            }
            other => panic!("Expected QuarantineList, got: {other:?}"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_handle_request_list_quarantines_seeded_entries() {
        use crate::workspace_registry::REGISTRY_PATH_ENV;

        let (tm, db, _, bus) = setup_test_context();
        let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
        let root = dir.path().to_path_buf();
        let empty_reg = dir.path().join("no-such-workspaces.json");
        std::env::set_var(REGISTRY_PATH_ENV, &empty_reg);

        let applied_at = Utc::now();
        {
            let mut reg = sr.lock().unwrap();
            reg.seed_quarantine_with_details_for_test(4215, applied_at, 2);
        }

        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(root.clone(), sr.clone());

        let response = handle_request(
            Request::ListQuarantines {
                workspace_root: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &pool,
        );
        std::env::remove_var(REGISTRY_PATH_ENV);

        match response {
            Response::QuarantineList { entries } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].issue, 4215);
                assert_eq!(entries[0].workspace_root, root);
                assert_eq!(entries[0].insta_crash_count, 2);
                assert_eq!(entries[0].quarantined_at, applied_at);
                assert!(
                    entries[0].ttl_remaining_secs > 0,
                    "freshly-applied quarantine should have TTL remaining"
                );
            }
            other => panic!("Expected QuarantineList, got: {other:?}"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_handle_request_list_quarantines_ttl_clamps_to_zero() {
        use crate::workspace_registry::REGISTRY_PATH_ENV;

        let (tm, db, _, bus) = setup_test_context();
        let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
        let root = dir.path().to_path_buf();
        let empty_reg = dir.path().join("no-such-workspaces.json");
        std::env::set_var(REGISTRY_PATH_ENV, &empty_reg);

        // Default TTL is 3600s (Issue #3939) — quarantine this issue as though
        // it were applied 2 hours ago, well past TTL. `reap_once` (the actual
        // expiry sweep) never runs in this test, so the stale entry survives in
        // memory; `ttl_remaining_secs` must still clamp to 0 rather than
        // reporting a nonsensical negative remainder.
        let long_ago = Utc::now() - chrono::Duration::seconds(7200);
        {
            let mut reg = sr.lock().unwrap();
            reg.seed_quarantine_with_details_for_test(9001, long_ago, 5);
        }

        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(root, sr.clone());

        let response = handle_request(
            Request::ListQuarantines {
                workspace_root: None,
            },
            &tm,
            &db,
            &sr,
            &bus,
            &pool,
        );
        std::env::remove_var(REGISTRY_PATH_ENV);

        match response {
            Response::QuarantineList { entries } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].issue, 9001);
                assert_eq!(entries[0].ttl_remaining_secs, 0, "past-TTL entry must clamp to 0");
            }
            other => panic!("Expected QuarantineList, got: {other:?}"),
        }
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

    /// Issue #4980 acceptance criterion 3: `loom-daemon cancel` (CLI) and
    /// `cancel_sweep` (MCP) must **share** the termination implementation.
    ///
    /// The structural guarantee is that both surfaces put the *same frame* on
    /// the wire, so there is exactly one server-side path and nothing to
    /// diverge. This asserts it at the byte level: the JSON `mcp-loom`'s
    /// `cancelSweep` sends and the JSON the CLI serializes from
    /// `build_cancel_request` deserialize to the identical
    /// `Request::CancelSweep`.
    #[test]
    fn test_cancel_sweep_cli_and_mcp_frames_are_identical_on_the_wire() {
        // Exactly what `mcp-loom/src/tools/sweeps.ts` `cancelSweep` sends
        // (`sendDaemonRequest` writes the `{type, payload}` shape verbatim).
        let mcp_frame = r#"{"type":"CancelSweep","payload":{"sweep_id":"sweep-issue-4980-1","grace_secs":30,"workspace_root":null}}"#;
        let from_mcp: Request = serde_json::from_str(mcp_frame).expect("MCP frame must parse");

        // Exactly what the `loom-daemon cancel` CLI serializes.
        let from_cli: Request = serde_json::from_str(
            &serde_json::to_string(&Request::CancelSweep {
                sweep_id: "sweep-issue-4980-1".to_string(),
                grace_secs: 30,
                workspace_root: None,
            })
            .unwrap(),
        )
        .expect("CLI frame must parse");

        match (&from_mcp, &from_cli) {
            (
                Request::CancelSweep {
                    sweep_id: mcp_id,
                    grace_secs: mcp_grace,
                    workspace_root: mcp_ws,
                },
                Request::CancelSweep {
                    sweep_id: cli_id,
                    grace_secs: cli_grace,
                    workspace_root: cli_ws,
                },
            ) => {
                assert_eq!(mcp_id, cli_id);
                assert_eq!(mcp_grace, cli_grace);
                assert_eq!(mcp_ws, cli_ws);
            }
            other => panic!("expected two CancelSweep requests, got {other:?}"),
        }
    }

    /// Issue #4980: a CLI-invoked cancel runs the full daemon-side termination
    /// path — terminal transition plus claim-lock release — exercised through a
    /// frame parsed off the wire rather than one constructed in-process, so a
    /// wire-format regression fails here too.
    #[test]
    #[serial_test::serial]
    fn test_handle_request_cancel_sweep_from_cli_frame_runs_the_shared_path() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
        let _registry_guard = seed_temp_registry(&[]);

        let dispatched = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(4980),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                // `None` resolves to the default registry `sr` — the same
                // tempdir-rooted registry the cancel below targets.
                workspace_root: None,
                force: false,
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
        let lock_dir = dir.path().join(".loom").join("locks").join("issue-4980");
        assert!(lock_dir.exists(), "dispatch should have taken the claim lock");

        // Parse the CLI's frame off the wire, exactly as `handle_client` would.
        let frame = serde_json::to_string(&Request::CancelSweep {
            sweep_id: sweep_id.clone(),
            grace_secs: 1,
            workspace_root: None,
        })
        .unwrap();
        let request: Request = serde_json::from_str(&frame).expect("CLI frame must parse");

        let response = handle_request(request, &tm, &db, &sr, &bus, &test_pool());
        match response {
            Response::SweepCancelled {
                sweep_id: cancelled,
                ..
            } => assert_eq!(cancelled, sweep_id),
            other => panic!("Expected SweepCancelled, got: {other:?}"),
        }

        let state = sr
            .lock()
            .unwrap()
            .get(&sweep_id)
            .expect("entry should still be tracked")
            .state
            .clone();
        assert!(
            matches!(state, crate::types::SweepState::Exited { .. }),
            "a CLI-invoked cancel must run the same terminal transition the MCP tool does, \
             got {state:?}"
        );
        assert!(
            !lock_dir.exists(),
            "a CLI-invoked cancel must release the claim lock, exactly like the MCP path"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_handle_request_get_sweep_status_returns_existing() {
        let (tm, db, _, bus) = setup_test_context();
        let (sr, _dir, _rec) = setup_sweep_registry_in_tempdir();
        // #4299: pin the registry to empty so `workspace_root: None` resolution
        // is deterministic regardless of the host's real registry.
        let _registry_guard = seed_temp_registry(&[]);

        // Dispatch a sweep to get a real entry in the registry.
        let dispatched = handle_request(
            Request::DispatchSweep {
                kind: SweepKind::Issue(444),
                idempotency_key: None,
                model: None,
                effort: None,
                depends_on: None,
                workspace_root: None,
                force: false,
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
            journal_adopted_at_startup: 0,
            in_flight: vec![],
            unregistered_locked: vec![],
            token_pool_size: 4,
            token_pool_dir: Some(std::path::PathBuf::from("/repo/a/.loom/tokens")),
            disk_headroom: 10,
            ram_headroom: 10,
            logical_cpus: 8,
            loadavg_1m: Some(1.25),
            cpu_idle_fraction: Some(0.90),
            capacity_bound: false,
            preflight_advisory_active: false,
            preflight_advisory_message: None,
            preflight_advisory_changed_at: None,
            configured_max: 5,
            dynamic_cap: 3,
            main_health_gate_halted: true,
            main_health_gate_not_evaluated: false,
            main_health_gate_not_evaluated_reason: None,
            main_health_gate_enabled: Some(true),
            main_health_gate_verdict_at: Some(chrono::Utc::now()),
            main_health_gate_deferred: false,
            main_health_gate_deferred_reason: None,
            main_health_gate_verdict_tier: Some("full".to_string()),
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
                root_missing: false,
                health_gate_deferred: false,
                health_gate_deferred_reason: None,
                health_gate_verdict_tier: Some("full".to_string()),
                role_runner_enabled: true,
                role_runner_roles: vec!["champion".to_string()],
                role_runner_on_idle_roles: vec![],
                role_runner_env_override: None,
                token_pool_dir: Some(std::path::PathBuf::from("/repo/a/.loom/tokens")),
                ranking_present: true,
                ranking_age_secs: Some(120),
                stash_total_count: 0,
                stash_quarantine_count: 0,
                stash_oldest_age_secs: None,
                sweep_command_missing: false,
            }],
            role_runner_host_env_override: None,
            credential_preflight: Some(test_credential_preflight()),
            draining: false,
            drain_deadline: None,
            drain_note: None,
            auto_update_enabled: true,
            auto_update_last_check: Some(chrono::Utc::now()),
            auto_update_last_roll: Some(chrono::Utc::now()),
            auto_update_consecutive_failures: 2,
            auto_update_backoff_secs: Some(120),
            auto_update_terminal_reason: None,
            auto_update_note: Some("within settle window".to_string()),
            host_breaker: None,
            admission_brake: None,
            rate_limit_breaker: None,
            safehouse: Some(crate::types::SafehouseStatus {
                state: "connected".to_string(),
                socket: Some(std::path::PathBuf::from("/tmp/safehoused.sock")),
                room: Some("fleet".to_string()),
                reason: None,
            }),
            work_finder_enabled: Some(true),
            last_work_finder_tick: Some(crate::types::WorkFinderTickSummary {
                at: chrono::Utc::now(),
                max_concurrent: 3,
                seen: 9,
                dispatched: 1,
                skipped_in_flight: 8,
                ..Default::default()
            }),
            role_tick_records: vec![crate::types::RoleTickRecord {
                root: std::path::PathBuf::from("/repo/a"),
                role: "champion".to_string(),
                at: chrono::Utc::now(),
                ok: true,
                detail: None,
            }],
            role_last_tick: vec![crate::types::RoleLastTick {
                root: std::path::PathBuf::from("/repo/a"),
                role: "champion".to_string(),
                at: chrono::Utc::now(),
                ok: true,
                detail: None,
                consecutive_identical_failures: 0,
            }],
            active_role_agents: 3,
            role_agent_max_concurrent: Some(7),
            daemon_pid: Some(99917),
            pid_file: Some(std::path::PathBuf::from("/repo/a/.loom/.daemon.pid")),
            daemon_build_commit: Some("18887b5c".to_string()),
            daemon_built_at_raw: Some("2026-08-02T03:09:51Z".to_string()),
            work_finder_interval_secs: Some(60),
            observability_host_id_mismatch: Some(crate::types::ObservabilityHostIdMismatch {
                daemon_host_id: "robb-studio".to_string(),
                ingest_host_id: "robb-pro".to_string(),
                first_seen_at: chrono::Utc::now(),
            }),
            observability_export: Some(crate::types::ObservabilityExportStatus {
                state: crate::types::ObservabilityExportState::HostIdMismatch,
                host_id: Some("robb-studio".to_string()),
                ingest_host_id: Some("robb-pro".to_string()),
                endpoint: Some("https://dashboard.example/ingest".to_string()),
                exporter: Some("https".to_string()),
                started_at: Some(chrono::Utc::now()),
                last_success_at: Some(chrono::Utc::now()),
                records_exported: 128,
                ..Default::default()
            }),
            peer_claims: None,
            deep_clean: Vec::new(),
            idle_exit: Some(crate::types::IdleExitStatus {
                enabled: true,
                eligible: false,
                trigger: None,
                idle_minutes: 60,
                in_flight_sweeps: 0,
                active_role_runs: 0,
                healthy_tokens: 3,
                total_tokens: 4,
                idle_elapsed_secs: 900,
                starved_elapsed_secs: 0,
                starvation_enabled: true,
                observed_at: Some(chrono::Utc::now()),
            }),
        };
        let resp = Response::DaemonStatus(Box::new(report));
        let json = serde_json::to_string(&resp).expect("serialize response");
        let back: Response = serde_json::from_str(&json).expect("deserialize response");
        match back {
            Response::DaemonStatus(r) => {
                assert_eq!(r.token_pool_size, 4);
                assert_eq!(
                    r.token_pool_dir,
                    Some(std::path::PathBuf::from("/repo/a/.loom/tokens"))
                );
                assert_eq!(r.disk_headroom, 10);
                assert_eq!(r.logical_cpus, 8);
                assert!(r.auto_update_enabled);
                assert_eq!(r.auto_update_consecutive_failures, 2);
                assert_eq!(r.auto_update_backoff_secs, Some(120));
                assert_eq!(r.auto_update_note.as_deref(), Some("within settle window"));
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
                // #4830: the host-identity mismatch survives the wire so a
                // `health` client in another process can render the note.
                let mismatch = r
                    .observability_host_id_mismatch
                    .as_ref()
                    .expect("mismatch round-trips");
                assert_eq!(mismatch.daemon_host_id, "robb-studio");
                assert_eq!(mismatch.ingest_host_id, "robb-pro");
                // #5083: the positive export record survives the wire too —
                // this is what lets a `status`/`health` client in another
                // process state that telemetry IS (or is not) landing rather
                // than infer it from the absence of a warning.
                let export = r
                    .observability_export
                    .as_ref()
                    .expect("export status round-trips");
                assert_eq!(export.state, crate::types::ObservabilityExportState::HostIdMismatch);
                assert_eq!(export.host_id.as_deref(), Some("robb-studio"));
                assert_eq!(export.ingest_host_id.as_deref(), Some("robb-pro"));
                assert_eq!(export.records_exported, 128);
                assert_eq!(r.per_repo[0].health_gate_enabled, Some(true));
                assert!(r.per_repo[0].health_gate_verdict_at.is_some());
                assert_eq!(
                    r.credential_preflight
                        .as_ref()
                        .map(|c| c.mechanism.as_str()),
                    Some("test-fixture")
                );
                assert_eq!(r.work_finder_enabled, Some(true));
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
    /// daemon proves it is supervised (launchd or systemd) via
    /// `LOOM_DAEMON_SUPERVISOR`; an unsupervised host refuses and stays running.
    /// Also pins the shutdown-intent exit-code contract (#4054): only the
    /// restart primitive exits 0, so under a supervisor's "successful exit
    /// restarts" policy (launchd `KeepAlive:SuccessfulExit`, systemd
    /// `Restart=on-success`) it is the only path that relaunches.
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
        // #4531: the self-reported startup-failure exit must stay non-zero (no
        // relaunch) AND keep the value `ExitCode::FAILURE` used to produce, so
        // callers that only knew the old `Termination`-driven exit see no change.
        assert_ne!(EXIT_STARTUP_FAILURE, 0, "startup failure must be non-zero (no relaunch)");
        assert_eq!(EXIT_STARTUP_FAILURE, 1, "startup failure must match ExitCode::FAILURE");

        // Supervised: scheduled + do_exit.
        std::env::set_var("LOOM_DAEMON_SUPERVISOR", "launchd");
        assert_eq!(detect_supervisor().as_deref(), Some("launchd"));
        let (resp, do_exit) = build_restart_decision(0);
        assert!(do_exit, "supervised daemon must end its process for a relaunch");
        match resp {
            Response::DaemonRestart {
                scheduled,
                supervisor,
                message,
            } => {
                assert!(scheduled);
                assert_eq!(supervisor.as_deref(), Some("launchd"));
                // #5119: on launchd, sweeps GENUINELY survive (children reparent
                // to pid 1) — the message must still say so.
                assert!(
                    message.contains("survive"),
                    "launchd restart message must state sweeps survive: {message}"
                );
                assert!(
                    !message.contains("do NOT survive"),
                    "launchd restart message must NOT claim sweeps are terminated: {message}"
                );
            }
            other => panic!("Expected DaemonRestart, got: {other:?}"),
        }

        // Case-insensitive acceptance.
        std::env::set_var("LOOM_DAEMON_SUPERVISOR", "LaunchD");
        assert_eq!(detect_supervisor().as_deref(), Some("launchd"));

        // Unsupervised (var unset): refuse, keep running.
        std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
        assert!(detect_supervisor().is_none());
        let (resp, do_exit) = build_restart_decision(0);
        assert!(!do_exit, "unsupervised daemon must NOT exit — nothing would relaunch it");
        match resp {
            Response::DaemonRestart {
                scheduled,
                supervisor,
                message,
            } => {
                assert!(!scheduled);
                assert!(supervisor.is_none());
                // #4640: the refusal must mention the systemd retrofit for a
                // fleet worker provisioned before the fix (missing
                // LOOM_DAEMON_SUPERVISOR despite being systemd-supervised).
                assert!(
                    message.contains("LOOM_DAEMON_SUPERVISOR=systemd"),
                    "restart refusal must mention the systemd retrofit: {message}"
                );
                assert!(
                    message.contains("Restart=on-success"),
                    "restart refusal retrofit hint must include the corrected Restart= policy: {message}"
                );
            }
            other => panic!("Expected DaemonRestart, got: {other:?}"),
        }

        // systemd (#4267): recognized alongside launchd, case-insensitive —
        // ⇒ Some("systemd"), scheduled + do_exit, and a message that does not
        // hardcode "launchd".
        std::env::set_var("LOOM_DAEMON_SUPERVISOR", "systemd");
        assert_eq!(detect_supervisor().as_deref(), Some("systemd"));
        let (resp, do_exit) = build_restart_decision(3);
        assert!(do_exit, "systemd-supervised daemon must end its process for a relaunch");
        match resp {
            Response::DaemonRestart {
                scheduled,
                supervisor,
                message,
            } => {
                assert!(scheduled);
                assert_eq!(supervisor.as_deref(), Some("systemd"));
                assert!(
                    !message.contains("launchd"),
                    "systemd restart message must not hardcode launchd wording: {message}"
                );
                // #5119: the systemd message must be HONEST — sweeps are reaped
                // with the cgroup, NOT preserved. It must not print the old
                // macOS-only "In-flight sweeps survive by design" claim, it must
                // name the in-flight count it is about to destroy, and it must
                // point at --drain as the preserving alternative.
                assert!(
                    message.contains("do NOT survive"),
                    "systemd restart message must state in-flight sweeps do NOT survive: {message}"
                );
                assert!(
                    !message.contains("survive by design"),
                    "systemd restart message must not repeat the false 'survive by design' claim: {message}"
                );
                assert!(
                    message.contains("cgroup"),
                    "systemd restart message must name the cgroup as the reason: {message}"
                );
                assert!(
                    message.contains("3 sweep(s)"),
                    "systemd ack must name the in-flight count it is about to destroy: {message}"
                );
                assert!(
                    message.contains("--drain"),
                    "systemd restart message must point at --drain to preserve sweeps: {message}"
                );
            }
            other => panic!("Expected DaemonRestart, got: {other:?}"),
        }

        // Mixed-case systemd.
        std::env::set_var("LOOM_DAEMON_SUPERVISOR", "SyStEmD");
        assert_eq!(detect_supervisor().as_deref(), Some("systemd"));

        // Empty string is not a recognized supervisor.
        std::env::set_var("LOOM_DAEMON_SUPERVISOR", "");
        assert!(detect_supervisor().is_none());

        // Whitespace-only value is not a recognized supervisor (no trimming).
        std::env::set_var("LOOM_DAEMON_SUPERVISOR", "  ");
        assert!(detect_supervisor().is_none());

        // A genuinely unrelated value is also unsupervised.
        std::env::set_var("LOOM_DAEMON_SUPERVISOR", "runit");
        assert!(detect_supervisor().is_none());
        std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
    }

    /// Issue #5119 AC2: the two supervisors have OPPOSITE in-flight semantics,
    /// and the restart primitive must say which one it is on. Pure, so both
    /// wordings are pinned here without a supervisor on the host.
    ///
    /// This exercises [`restart_scheduled_message`] directly — the single
    /// canonical composer for this ack. An earlier revision of this PR carried a
    /// second, near-duplicate `restart_in_flight_fate()` composing the same
    /// wording; it was folded into this one function during the rebase onto main
    /// so there is exactly one place the honest-restart wording can drift.
    #[test]
    fn restart_scheduled_message_is_supervisor_specific() {
        // launchd: the survival claim is TRUE there — children reparent to pid 1
        // and keep running (verified repeatedly, #5081). The count is deliberately
        // NOT named: nothing is at risk, so there is nothing to warn about.
        let launchd = restart_scheduled_message("launchd", 4);
        assert!(launchd.contains("In-flight sweeps survive by design"), "got: {launchd}");
        assert!(!launchd.contains("WARNING"), "got: {launchd}");
        assert!(!launchd.contains("sweep(s)"), "got: {launchd}");

        // systemd: the claim is FALSE (the stop job reaps the unit's cgroup),
        // so the message must say so plainly, name the count, and point at the
        // alternative that genuinely preserves the work.
        let systemd = restart_scheduled_message("systemd", 4);
        assert!(!systemd.contains("survive by design"), "got: {systemd}");
        assert!(!systemd.contains("launchd"), "got: {systemd}");
        assert!(systemd.contains("WARNING:"), "got: {systemd}");
        assert!(systemd.contains("do NOT survive"), "got: {systemd}");
        assert!(systemd.contains("cgroup"), "got: {systemd}");
        assert!(systemd.contains("4 sweep(s)"), "got: {systemd}");
        // Both kill shapes are named: the canonical KillMode=mixed unit (#4862)
        // and the older KillMode=control-group one a pre-#4862 worker may still
        // be running.
        assert!(systemd.contains("KillMode=mixed"), "got: {systemd}");
        assert!(systemd.contains("KillMode=control-group"), "got: {systemd}");
        // Role runs are the compounding factor from the 2026-08-03 incident and
        // have no registry entry, so the count must not be presented as covering
        // them.
        assert!(systemd.contains("role runs"), "got: {systemd}");
        assert!(systemd.contains("restart --drain"), "got: {systemd}");

        // Zero in flight still warns: role runs are uncounted, and the daemon
        // launches them on a timer, so "0 sweeps" never means "nothing to lose".
        let idle = restart_scheduled_message("systemd", 0);
        assert!(idle.contains("0 sweep(s)"), "got: {idle}");
        assert!(idle.contains("role runs"), "got: {idle}");
        assert!(idle.contains("nothing to lose"), "got: {idle}");
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
        // Absent pre-#3978 fields default rather than failing to parse. (The
        // retired `cpu_headroom` field a pre-#4512 daemon still SENDS is
        // likewise tolerated: serde ignores unknown fields, so an old daemon
        // and a new CLI stay wire-compatible in both directions.)
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

    /// #5305: `capacity.token_bound` must be reachable again — it was
    /// hardcoded `false` after #5304, permanently dead-ending the
    /// `status_render.rs` add-accounts guidance branch. It means genuine
    /// starvation (zero healthy accounts in the ranking), NOT "tokens bound
    /// the dynamic cap" (that cross-axis meaning was retired by #5270).
    #[test]
    #[serial_test::serial]
    fn test_build_daemon_status_token_bound_reflects_zero_healthy_accounts() {
        use crate::main_health_gate::WorkspaceHealthStates;
        use crate::workspace_registry::REGISTRY_PATH_ENV;

        let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
        let root = dir.path().to_path_buf();
        let empty_reg = dir.path().join("no-such-workspaces.json");
        std::env::set_var(REGISTRY_PATH_ENV, &empty_reg);
        let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
        std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");

        let tokens_dir = root.join(".loom").join("tokens");
        std::fs::create_dir_all(&tokens_dir).unwrap();
        std::fs::write(tokens_dir.join("acct-a.token"), "sk-ant-oat01-a").unwrap();
        std::fs::write(tokens_dir.join("acct-b.token"), "sk-ant-oat01-b").unwrap();
        std::fs::write(tokens_dir.join("acct-c.token"), "sk-ant-oat01-c").unwrap();

        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(root.clone(), sr.clone());
        let health = WorkspaceHealthStates::new();

        // A partially-exhausted pool (1/3 healthy) must NOT report token_bound
        // — this is the false add-accounts advisory this issue fixes (#5304
        // over-removal, item 1/2): a healthy account remains, so this is not
        // starvation, regardless of how the dynamic cap (disk/ram/ceiling)
        // happens to compare.
        std::fs::write(
            tokens_dir.join(".ranking"),
            "acct-a|available|0.1\nacct-b|exhausted|0.99\nacct-c|exhausted|0.99\n",
        )
        .unwrap();
        let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
        assert_eq!(report.capacity.healthy_accounts, 1);
        assert!(
            !report.capacity.token_bound,
            "one healthy account remains ⇒ not starved, no add-accounts advisory"
        );

        // Every account exhausted/blocked ⇒ genuinely starved: `token_bound`
        // must be reachable and true so the operator guidance branch fires.
        std::fs::write(
            tokens_dir.join(".ranking"),
            "acct-a|exhausted|0.99\nacct-b|blocked|0.99\nacct-c|exhausted|0.99\n",
        )
        .unwrap();
        let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
        assert_eq!(report.capacity.healthy_accounts, 0);
        assert!(
            report.capacity.token_bound,
            "zero healthy accounts ⇒ genuinely starved, guidance branch must be reachable"
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
        // The tempdir has no `.loom/tokens/`, so the pool is 0 — but since
        // #5270 the token axis no longer participates in the dynamic cap, so
        // an empty token pool no longer pins `dynamic_cap` to 0. The cap is
        // `min(disk_headroom, ram_headroom, configured_max)`, where
        // `configured_max` is resolved the same way `build_daemon_status`
        // resolves it (not assumed to be the built-in default — a host-level
        // config tier may set it, exactly the ambient config this assertion
        // must tolerate).
        assert_eq!(report.token_pool_size, 0);
        let expected_configured_max = crate::work_finder::resolve_max_concurrent_with_config(
            &crate::work_finder::read_work_finder_config(&root),
        );
        assert_eq!(
            report.dynamic_cap,
            report
                .disk_headroom
                .min(report.ram_headroom)
                .min(expected_configured_max),
            "dynamic cap is min(disk, ram, configured_max) regardless of the empty token pool"
        );
        // #4345: a pool that never called start_safehouse_narration /
        // start_peer_coordination still reports a live safehouse state — the
        // cell's own default, not a missing/`None` field.
        let safehouse = report
            .safehouse
            .as_ref()
            .expect("safehouse status always present");
        assert_eq!(safehouse.state, "not_configured");
        assert!(safehouse.socket.is_none());
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

    /// Regression test for Issue #4279 (the silent-EOF `status` incident): once
    /// the sweep-registry `Mutex` is **poisoned** (a thread panicked while
    /// holding it), `build_daemon_status` must still return a report by
    /// recovering the guard — NOT re-panic on every subsequent call. Before the
    /// fix, the `.expect("Sweep registry mutex poisoned")` turned one panic into
    /// a permanent server-side failure: every later `status` request panicked in
    /// its detached per-connection task and dropped the socket with zero bytes
    /// written, which the client saw as an empty response.
    #[test]
    #[serial_test::serial]
    fn test_build_daemon_status_recovers_from_poisoned_registry() {
        use crate::main_health_gate::WorkspaceHealthStates;
        use crate::workspace_registry::REGISTRY_PATH_ENV;

        let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
        let root = dir.path().to_path_buf();
        let empty_reg = dir.path().join("no-such-workspaces.json");
        std::env::set_var(REGISTRY_PATH_ENV, &empty_reg);
        let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
        std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");

        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(root.clone(), sr.clone());
        let health = WorkspaceHealthStates::new();

        // Poison the registry mutex exactly as a panic under the lock would: a
        // helper thread takes the lock and panics, leaving the `Mutex` poisoned.
        let poison_target = sr.clone();
        let joined = std::thread::spawn(move || {
            let _guard = poison_target.lock().expect("lock to poison");
            panic!("intentional panic to poison the registry mutex");
        })
        .join();
        assert!(joined.is_err(), "the poisoning thread must have panicked");
        assert!(sr.is_poisoned(), "registry mutex should now be poisoned");

        // The core invariant: a poisoned registry no longer crashes the status
        // build. It returns a report (recovering the guard) so `status` stays
        // answerable rather than EOF-ing every connection for the process's life.
        let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
        assert_eq!(report.per_repo.len(), 1, "single-workspace report still built");
        assert_eq!(report.per_repo[0].root, root);

        match prev_shared {
            Some(v) => std::env::set_var("LOOM_SHARED_TOKENS_DIR", v),
            None => std::env::remove_var("LOOM_SHARED_TOKENS_DIR"),
        }
        std::env::remove_var(REGISTRY_PATH_ENV);
    }

    /// Regression test for Issue #4214 (the "vanish window" incident): a sweep
    /// whose per-issue lock is live (`owner_pid` alive) but which has **no**
    /// matching in-flight registry entry — the exact shape of the observed
    /// incident, where the in-memory union of live entries silently lost track
    /// of a sweep the filesystem lock proves is still alive — must be surfaced
    /// via `DaemonStatusReport::unregistered_locked`, not silently omitted from
    /// `in_flight` with no trace at all. Also exercises the full JSON
    /// round-trip so a client (CLI, monitor script) sees the same shape.
    #[test]
    #[serial_test::serial]
    fn test_build_daemon_status_surfaces_unregistered_locked_sweep() {
        use crate::main_health_gate::WorkspaceHealthStates;
        use crate::workspace_registry::REGISTRY_PATH_ENV;

        let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
        let root = dir.path().to_path_buf();
        let empty_reg = dir.path().join("no-such-workspaces.json");
        std::env::set_var(REGISTRY_PATH_ENV, &empty_reg);
        let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
        std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");

        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(root.clone(), sr.clone());
        let health = WorkspaceHealthStates::new();

        // Baseline: no lock, nothing in flight, nothing unregistered.
        let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
        assert!(report.in_flight.is_empty());
        assert!(report.unregistered_locked.is_empty());

        // Simulate the observed incident: a live, locked sweep (owner_pid alive,
        // lock dir valid) with NO corresponding registry entry — i.e. it never
        // went through `reconstruct()` or `dispatch()` in this process's
        // lifetime, exactly the read-path-gap shape the issue's forensics
        // pointed to (a registry mutation would have shown up as a Crashed/
        // Exited terminal entry instead, not a bare absence).
        let locks_dir = root.join(".loom").join("locks").join("issue-4201");
        std::fs::create_dir_all(&locks_dir).unwrap();
        // `LockOwner` is private to `sweep_registry`; write its wire schema
        // directly (mirrors what `acquire_lock` writes) rather than reaching
        // across the module boundary.
        let owner = serde_json::json!({
            "issue": 4201,
            "owner_pid": std::process::id(),
            "acquired_at": chrono::Utc::now().to_rfc3339(),
            "sweep_id": "sweep-issue-4201-1785221507",
        });
        std::fs::write(locks_dir.join("owner.json"), serde_json::to_string_pretty(&owner).unwrap())
            .unwrap();

        let report = build_daemon_status(&pool, &health, &root, &test_credential_preflight());
        assert!(
            report.in_flight.is_empty(),
            "the sweep never went through dispatch/reconstruct in this test, so it \
             is deliberately still absent from in_flight -- that's the omission \
             this test targets"
        );
        assert_eq!(
            report.unregistered_locked.len(),
            1,
            "a live-locked issue with no registry entry must surface as unregistered_locked, \
             got: {:?}",
            report.unregistered_locked
        );
        let entry = &report.unregistered_locked[0];
        assert_eq!(entry.issue, 4201);
        assert_eq!(entry.owner_pid, std::process::id());
        assert_eq!(entry.root, root);

        // JSON round-trip: the field survives serialize -> deserialize (the
        // wire contract `loom-daemon status --json` and any monitor script rely
        // on), and stays byte-identical to a re-parse of the legacy fixture
        // from `test_daemon_status_backward_compat_missing_capacity` (an absent
        // field there must still default to empty -- covered by that test; this
        // one only asserts our populated case survives the round trip).
        let json = serde_json::to_string(&report).expect("serialize DaemonStatusReport");
        let back: DaemonStatusReport =
            serde_json::from_str(&json).expect("deserialize DaemonStatusReport");
        assert_eq!(back.unregistered_locked.len(), 1);
        assert_eq!(back.unregistered_locked[0].issue, 4201);
        assert_eq!(back.unregistered_locked[0].owner_pid, std::process::id());

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

    /// Issue #5269: a daemon whose `fallback_root` (launch CWD) is repo A must
    /// still report repo B's OWN `.ranking` freshness in `per_repo`, reading
    /// repo B's own per-repo pool — NOT repo A's, and not whatever the
    /// top-level `fallback_root`-anchored `token_pool_dir`/`capacity` fields
    /// resolved to. This is the exact scope mismatch the issue reports: an
    /// operator asking about repo B from a daemon anchored at repo A
    /// previously got no answer about repo B's pool at all.
    #[test]
    #[serial_test::serial]
    fn test_build_daemon_status_per_repo_ranking_reflects_each_repos_own_pool() {
        use crate::main_health_gate::WorkspaceHealthStates;
        use crate::workspace_registry::{normalize_path, WorkspaceRegistry, REGISTRY_PATH_ENV};

        let (sr_a, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
        let (sr_b, dir_b, _rec_b) = setup_sweep_registry_in_tempdir();
        let root_a = dir_a.path().to_path_buf();
        let root_b = dir_b.path().to_path_buf();

        // Disable the shared machine-level pool fallback so each repo's own
        // per-repo `.loom/tokens/` is the only pool `resolve_tokens_dir` can
        // find for it — otherwise a repo with no per-repo pool would silently
        // fall through to the host's real `~/.loom/tokens` (or a stale value
        // left by another `#[serial]` test) instead of reporting "absent".
        let prev_shared = std::env::var("LOOM_SHARED_TOKENS_DIR").ok();
        std::env::set_var("LOOM_SHARED_TOKENS_DIR", "");

        // A registry listing BOTH managed repos, exactly like the sibling
        // multi-workspace test above.
        let reg_path = dir_a.path().join("workspaces.json");
        let mut reg = WorkspaceRegistry::default();
        reg.add(&root_a, None).unwrap();
        reg.add(&root_b, None).unwrap();
        reg.save(&reg_path).unwrap();
        std::env::set_var(REGISTRY_PATH_ENV, &reg_path);

        let canon_a = normalize_path(&root_a);
        let canon_b = normalize_path(&root_b);

        // Repo A's own pool: present but STALE (mtime forced far in the past).
        let tokens_a = canon_a.join(".loom").join("tokens");
        std::fs::create_dir_all(&tokens_a).unwrap();
        std::fs::write(tokens_a.join("acct-a.token"), "sk-ant-oat01-a").unwrap();
        let ranking_a_path = tokens_a.join(".ranking");
        std::fs::write(&ranking_a_path, "acct-a|available|0.1\n").unwrap();
        let stale_mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        std::fs::File::options()
            .write(true)
            .open(&ranking_a_path)
            .unwrap()
            .set_modified(stale_mtime)
            .unwrap();

        // Repo B's own pool: present and FRESH.
        let tokens_b = canon_b.join(".loom").join("tokens");
        std::fs::create_dir_all(&tokens_b).unwrap();
        std::fs::write(tokens_b.join("acct-b.token"), "sk-ant-oat01-b").unwrap();
        std::fs::write(tokens_b.join(".ranking"), "acct-b|available|0.1\n").unwrap();

        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(canon_a.clone(), sr_a.clone());
        pool.seed(canon_b.clone(), sr_b.clone());
        let health = WorkspaceHealthStates::new();

        // The daemon's own `fallback_root`/launch CWD is repo A.
        let report = build_daemon_status(&pool, &health, &root_a, &test_credential_preflight());
        assert_eq!(report.per_repo.len(), 2, "both managed repos are listed");

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

        // Each repo's own resolved pool is its OWN per-repo directory, not the
        // daemon's single anchored `token_pool_dir`.
        assert_eq!(a.token_pool_dir.as_deref(), Some(tokens_a.as_path()));
        assert_eq!(b.token_pool_dir.as_deref(), Some(tokens_b.as_path()));

        // Repo A's own ranking is present but stale.
        assert!(a.ranking_present, "repo A has its own .ranking");
        assert!(
            a.ranking_age_secs.unwrap_or(0) >= 7000,
            "repo A's own ranking must read as ~2h old, got {:?}",
            a.ranking_age_secs
        );

        // Repo B's own ranking is present and fresh — this is the exact
        // scenario the bug report describes ("worker-1: 5h-stale machine
        // pool" while the operator's own repo's self-refresh loop kept its
        // OWN pool current): the per-repo report must reflect repo B's own
        // freshness, independent of the daemon's `fallback_root`-anchored
        // primary-workspace value.
        assert!(b.ranking_present, "repo B has its own .ranking");
        assert!(
            b.ranking_age_secs.unwrap_or(u64::MAX) < 60,
            "repo B's own ranking must read as fresh, got {:?}",
            b.ranking_age_secs
        );

        match prev_shared {
            Some(v) => std::env::set_var("LOOM_SHARED_TOKENS_DIR", v),
            None => std::env::remove_var("LOOM_SHARED_TOKENS_DIR"),
        }
        std::env::remove_var(REGISTRY_PATH_ENV);
    }

    /// Issue #4326: a registry entry whose root directory has been deleted
    /// (without a matching `workspace remove`) is flagged `root_missing: true`
    /// in `build_daemon_status`'s per-repo breakdown, while a sibling whose
    /// directory still exists is unaffected — this is the daemon-side half of
    /// the missing-root hygiene backstop (the work-finder's per-tick skip is
    /// covered separately in `work_finder::tests`).
    #[test]
    #[serial_test::serial]
    fn test_build_daemon_status_flags_missing_root() {
        use crate::main_health_gate::WorkspaceHealthStates;
        use crate::workspace_registry::{normalize_path, WorkspaceRegistry, REGISTRY_PATH_ENV};

        let (sr_a, dir_a, _rec_a) = setup_sweep_registry_in_tempdir();
        let root_a = dir_a.path().to_path_buf();
        // A second root that exists at registration time, then gets removed
        // from disk — exactly the dangling-entry shape from #4326 (a scratch
        // dir was deleted without `loom-daemon workspace remove`).
        let dangling_dir = tempfile::tempdir().unwrap();
        let root_dangling = dangling_dir.path().to_path_buf();
        let canon_dangling = normalize_path(&root_dangling);

        let reg_path = dir_a.path().join("workspaces.json");
        let mut reg = WorkspaceRegistry::default();
        reg.add(&root_a, None).unwrap();
        reg.add(&root_dangling, None).unwrap();
        reg.save(&reg_path).unwrap();
        std::env::set_var(REGISTRY_PATH_ENV, &reg_path);

        // Delete the second root's directory after registration — the entry
        // itself stays registered (warn-and-skip, never auto-remove).
        drop(dangling_dir);
        assert!(!canon_dangling.exists(), "precondition: dangling root is gone");

        let canon_a = normalize_path(&root_a);
        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(canon_a.clone(), sr_a.clone());

        let health = WorkspaceHealthStates::new();
        let report = build_daemon_status(&pool, &health, &root_a, &test_credential_preflight());

        assert_eq!(report.per_repo.len(), 2);
        let a = report
            .per_repo
            .iter()
            .find(|r| r.root == canon_a)
            .expect("repo A present");
        let dangling = report
            .per_repo
            .iter()
            .find(|r| r.root == canon_dangling)
            .expect("dangling entry still present in the registry");
        assert!(!a.root_missing, "repo A's directory still exists");
        assert!(dangling.root_missing, "the deleted root is flagged missing");

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
        let (gen1, deadline) = match drain.begin(Duration::from_secs(120), false, false) {
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
        match drain.begin(Duration::from_secs(9999), true, false) {
            DrainBegin::AlreadyDraining {
                active_then_exit,
                escalated,
                force_escalated,
            } => {
                assert!(!active_then_exit, "active drain is still a relaunch drain");
                assert!(!escalated, "a then_exit=false request escalates nothing");
                assert!(
                    !force_escalated,
                    "#6007: force escalation applies only to a PENDING roll — a \
                     first-attempt drain's force flag stays pinned (#4521)"
                );
            }
            other => panic!("expected AlreadyDraining, got {other:?}"),
        }
        assert!(
            !drain.snapshot().force_after_timeout,
            "#4521 invariant: the active first-attempt drain's force flag is pinned"
        );
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
        let _ = drain.begin(Duration::from_secs(1), false, false);
        drain.resolve_timeout("timed out".to_string());
        assert!(!drain.is_draining());
        assert_eq!(drain.snapshot().note.as_deref(), Some("timed out"));
        assert!(drain.generation() > gen_before);
    }

    /// Issue #4521 AC1 — `then_exit` on the already-draining path is escalated
    /// **one way** (relaunch → stay-down) and the outcome reported back is the
    /// ACTIVE drain's terminal action, never a blind echo of the request.
    #[test]
    fn test_drain_then_exit_escalates_one_way() {
        // A relaunch-drain is in flight (this is the auto-update roll's shape:
        // `then_exit=false`).
        let drain = DrainState::new();
        let deadline = match drain.begin(Duration::from_secs(120), false, false) {
            DrainBegin::Started { deadline, .. } => deadline,
            other => panic!("expected Started, got {other:?}"),
        };
        assert!(!drain.snapshot().then_exit);

        // An operator teardown request lands mid-roll: it must NOT be silently
        // ignored (the #4521 defect) — the active drain escalates to stay-down.
        match drain.begin(Duration::from_secs(9999), true, true) {
            DrainBegin::AlreadyDraining {
                active_then_exit,
                escalated,
                force_escalated,
            } => {
                assert!(active_then_exit, "the active drain now stays down");
                assert!(escalated, "the escalation must be reported to the caller");
                assert!(
                    !force_escalated,
                    "no roll is pending, so force stays pinned (#4521 / #6007)"
                );
            }
            other => panic!("expected AlreadyDraining, got {other:?}"),
        }
        assert!(
            drain.snapshot().then_exit,
            "the escalation must be visible to the already-running supervisor, \
             which re-reads the descriptor"
        );
        // Everything else about the active drain is still pinned.
        assert_eq!(drain.snapshot().deadline, Some(deadline));
        assert!(!drain.snapshot().force_after_timeout);

        // Escalating again is a no-op that still reports the truth.
        match drain.begin(Duration::from_secs(1), false, true) {
            DrainBegin::AlreadyDraining {
                active_then_exit,
                escalated,
                ..
            } => {
                assert!(active_then_exit);
                assert!(!escalated, "already stay-down — nothing to escalate");
            }
            other => panic!("expected AlreadyDraining, got {other:?}"),
        }

        // A relaunch request against an active teardown drain must NOT downgrade
        // it: the reply still says "will stay down".
        match drain.begin(Duration::from_secs(1), false, false) {
            DrainBegin::AlreadyDraining {
                active_then_exit,
                escalated,
                ..
            } => {
                assert!(active_then_exit, "then-exit is never downgraded");
                assert!(!escalated);
            }
            other => panic!("expected AlreadyDraining, got {other:?}"),
        }
        assert!(drain.snapshot().then_exit);

        // After an abort, a fresh drain starts from the requested terminal
        // action again (the escalation does not leak across drains).
        assert!(drain.abort());
        match drain.begin(Duration::from_secs(30), false, false) {
            DrainBegin::Started { .. } => {}
            other => panic!("expected Started, got {other:?}"),
        }
        assert!(!drain.snapshot().then_exit, "a fresh drain honors its own then_exit");
    }

    /// Issue #4521 AC3 — the drain-completion exit-code contract: a then-exit
    /// drain must exit `EXIT_SHUTDOWN` (143, **non-zero**) so a launchd job with
    /// `KeepAlive:{SuccessfulExit:true}` stays down; a relaunch drain exits
    /// `EXIT_RESTART` (0) so it comes straight back. Exiting 0 on the then-exit
    /// path is the "drained, then relaunched anyway" failure.
    #[test]
    fn test_drain_exit_code_selection() {
        assert_eq!(drain_exit_code(true), EXIT_SHUTDOWN);
        assert_eq!(drain_exit_code(true), 143);
        assert_ne!(drain_exit_code(true), 0, "then-exit must never exit 0");
        assert_eq!(drain_exit_code(false), EXIT_RESTART);
        assert_eq!(drain_exit_code(false), 0);
    }

    /// Issue #4521 AC3 — the supervisor's branch selection follows the LIVE
    /// descriptor, not a value captured when it was spawned. This is what makes
    /// a mid-drain escalation effective: the supervisor re-reads `then_exit`
    /// from `DrainState` on each tick (see `run_drain_supervisor`), so the same
    /// read modeled here flips 0 → 143 after an escalation.
    #[test]
    fn test_supervisor_branch_follows_live_then_exit() {
        let drain = DrainState::new();
        let _ = drain.begin(Duration::from_secs(120), false, false);

        // Tick 1 (pre-escalation): zero in-flight ⇒ Complete ⇒ relaunch exit.
        assert_eq!(evaluate_drain_tick(0, false, false), DrainTick::Complete);
        assert_eq!(drain_exit_code(drain.snapshot().then_exit), EXIT_RESTART);

        // An operator teardown request escalates the drain in place.
        let _ = drain.begin(Duration::from_secs(120), false, true);

        // Tick 2 (post-escalation, same supervisor): the very same read now
        // selects the stay-down exit.
        assert_eq!(evaluate_drain_tick(0, false, false), DrainTick::Complete);
        assert_eq!(drain_exit_code(drain.snapshot().then_exit), EXIT_SHUTDOWN);

        // The forced-timeout terminal branch reads the same field.
        assert_eq!(evaluate_drain_tick(2, true, true), DrainTick::TimedOutForce);
        assert_eq!(drain_exit_code(drain.snapshot().then_exit), EXIT_SHUTDOWN);
    }

    /// Issue #4521 AC4 (regression pin) — the two drain-complete log lines stay
    /// **distinct**, so a host log tells an operator which terminal action fired
    /// without guessing. Asserted against the exact bodies
    /// `run_drain_supervisor`'s `DrainTick::Complete` arm emits.
    #[test]
    fn test_drain_complete_log_lines_remain_distinct() {
        let then_exit_line = drain_complete_log_line(true, "launchd");
        let relaunch_line = drain_complete_log_line(false, "launchd");
        assert_ne!(then_exit_line, relaunch_line);
        assert!(then_exit_line.contains("staying down"));
        assert!(then_exit_line.contains("143"));
        assert!(relaunch_line.contains("supervised relaunch"));
        assert!(!relaunch_line.contains("staying down"));
    }

    /// Issue #5340 (AC: the `TimedOutRefuse` message names the exact local
    /// retry command instead of leaving the operator to guess at a nonexistent
    /// bare `drain` subcommand or the unrelated `fleet drain <ssh_host>`
    /// remote-decommission command). Asserted against the exact body
    /// `run_drain_supervisor`'s `DrainTick::TimedOutRefuse` arm records via
    /// `DrainState::resolve_timeout` and that `loom-daemon status` renders
    /// verbatim as `Drain: not draining (last: <note>)`.
    #[test]
    fn test_drain_timeout_refuse_note_names_exact_retry_command() {
        let note = drain_timeout_refuse_note(3);
        assert!(
            note.contains("3 sweep(s) still in flight"),
            "expected the in-flight count in the note, got: {note}"
        );
        assert!(
            note.contains("loom-daemon restart --drain --force-after-timeout --timeout <secs>"),
            "expected the exact LOCAL retry command (no `fleet` prefix, no ssh_host arg), \
             got: {note}"
        );
        assert!(
            !note.contains("fleet drain"),
            "must not point at the unrelated remote worker-decommission command: {note}"
        );
        // Regression pin (#4090): the original refusal wording survives verbatim
        // as a prefix so `loom-daemon status`'s rendering and any log-scraping
        // tooling keyed on it keep matching.
        assert!(
            note.starts_with(
                "drain timed out with 3 sweep(s) still in flight — refused restart \
                 (no --force-after-timeout); dispatch resumed, daemon stays up."
            ),
            "expected the original refusal prefix to survive verbatim, got: {note}"
        );
    }

    // ===== Pending roll: drain/work-finder livelock (Issue #6007) =====

    /// The refusal policy is a *widen-then-give-up* sequence, not an unbounded
    /// hold: each refusal re-arms a geometrically wider window (the operator's
    /// manual "re-run with a bigger --timeout" workaround, automated), capped by
    /// [`MAX_DRAIN_RETRY_WINDOW_SECS`] and by whatever total paused-dispatch
    /// budget remains — and once the budget is spent it abandons the roll so a
    /// wedged sweep can never starve the host of work forever.
    #[test]
    fn test_drain_refusal_decision_widens_then_abandons() {
        let base = Duration::from_secs(1800);
        let budget = drain_pending_budget(base);
        assert_eq!(budget, Duration::from_secs(7200), "4 × the requested timeout");

        // First refusal, at the original 1800s deadline: re-arm 2 × base.
        match drain_refusal_decision(base, 0, Duration::from_secs(1800)) {
            RefusalDecision::Defer { window } => {
                assert_eq!(window, Duration::from_secs(3600), "widened to 2 × base");
            }
            other => panic!("expected Defer, got {other:?}"),
        }
        // Second refusal, 5400s in: 4 × base would be 7200s but only 1800s of
        // budget remains, so the window is clamped to the remaining budget.
        match drain_refusal_decision(base, 1, Duration::from_secs(5400)) {
            RefusalDecision::Defer { window } => {
                assert_eq!(window, Duration::from_secs(1800), "clamped to remaining budget");
            }
            other => panic!("expected Defer, got {other:?}"),
        }
        // Budget spent ⇒ abandon (dispatch resumes — the pre-#6007 outcome, but
        // only after the roll genuinely tried).
        assert_eq!(
            drain_refusal_decision(base, 2, Duration::from_secs(7200)),
            RefusalDecision::Abandon
        );
        // Less than a useful window left ⇒ abandon rather than arm a stub window.
        assert_eq!(
            drain_refusal_decision(base, 2, Duration::from_secs(7200 - 30)),
            RefusalDecision::Abandon
        );

        // A single window never exceeds the hard cap, however large the base.
        match drain_refusal_decision(Duration::from_secs(3600), 3, Duration::from_secs(60)) {
            RefusalDecision::Defer { window } => {
                assert_eq!(window, Duration::from_secs(MAX_DRAIN_RETRY_WINDOW_SECS));
            }
            other => panic!("expected Defer, got {other:?}"),
        }
    }

    /// The budget follows the operator's own `--timeout` (so a deliberately short
    /// drain stays short) and is capped in absolute terms (so an enormous
    /// `--timeout` cannot quiesce a host for a day).
    #[test]
    fn test_drain_pending_budget_scales_and_caps() {
        assert_eq!(drain_pending_budget(Duration::from_secs(60)), Duration::from_secs(240));
        assert_eq!(
            drain_pending_budget(Duration::from_secs(DEFAULT_DRAIN_TIMEOUT_SECS)),
            Duration::from_secs(7200)
        );
        assert_eq!(
            drain_pending_budget(Duration::from_secs(100_000)),
            Duration::from_secs(MAX_DRAIN_PENDING_BUDGET_SECS)
        );
        // A zero timeout buys no pending window at all — the very first refusal
        // abandons, i.e. exactly the pre-#6007 behavior.
        assert_eq!(drain_pending_budget(Duration::ZERO), Duration::ZERO);
        assert_eq!(
            drain_refusal_decision(Duration::ZERO, 0, Duration::ZERO),
            RefusalDecision::Abandon
        );
    }

    /// **The livelock regression test.** A refused deadline on a relaunch (roll)
    /// drain must NOT hand the admission window back to the work finder: the
    /// pause flag stays set, the roll is marked pending, the generation is
    /// unchanged (so the *same* supervisor keeps polling), and the deadline is
    /// re-armed. Only when the budget is spent does it clear the flag.
    ///
    /// Pre-#6007 this path called `resolve_timeout`, which cleared the flag on
    /// the very first refusal — the work finder then admitted more sweeps and the
    /// next drain was strictly harder to satisfy, so a busy host never rolled.
    #[test]
    fn test_roll_refusal_keeps_dispatch_paused_and_retains_the_roll() {
        let drain = DrainState::new();
        let base = Duration::from_secs(1800);
        let gen = match drain.begin(base, false, false) {
            DrainBegin::Started { generation, .. } => generation,
            other => panic!("expected Started, got {other:?}"),
        };
        assert!(drain.is_draining());
        let started = drain
            .snapshot()
            .started_at
            .expect("begin records started_at");

        // First deadline refusal.
        match drain.refuse_roll_deadline(started + chrono::Duration::seconds(1800)) {
            RollRefusal::Deferred {
                attempt,
                window,
                budget,
                ..
            } => {
                assert_eq!(attempt, 1);
                assert_eq!(window, Duration::from_secs(3600));
                assert_eq!(budget, Duration::from_secs(7200));
            }
            other => panic!("expected Deferred, got {other:?}"),
        }
        assert!(
            drain.is_draining(),
            "#6007: a refused roll must NOT resume dispatch — that is the livelock"
        );
        assert!(drain.snapshot().roll_pending, "the roll intent survives the refusal");
        assert!(drain.snapshot().active, "the drain is still active");
        assert_eq!(
            drain.generation(),
            gen,
            "the same supervisor must keep supervising (no generation bump)"
        );
        assert_eq!(
            drain.snapshot().deadline,
            Some(started + chrono::Duration::seconds(1800) + chrono::Duration::seconds(3600)),
            "the deadline is re-armed, not cleared"
        );

        // Second refusal: still pending, still paused, attempt counter advances.
        match drain.refuse_roll_deadline(started + chrono::Duration::seconds(5400)) {
            RollRefusal::Deferred {
                attempt, window, ..
            } => {
                assert_eq!(attempt, 2);
                assert_eq!(window, Duration::from_secs(1800));
            }
            other => panic!("expected Deferred, got {other:?}"),
        }
        assert!(drain.is_draining());
        assert_eq!(drain.generation(), gen);

        // Budget spent: the roll gives up, dispatch resumes, and the supervisor
        // is retired via a generation bump.
        match drain.refuse_roll_deadline(started + chrono::Duration::seconds(7200)) {
            RollRefusal::Abandoned {
                attempts, elapsed, ..
            } => {
                assert_eq!(attempts, 2, "two re-arms happened before giving up");
                assert_eq!(elapsed, Duration::from_secs(7200));
            }
            other => panic!("expected Abandoned, got {other:?}"),
        }
        assert!(!drain.is_draining(), "an abandoned roll resumes dispatch");
        assert!(!drain.snapshot().roll_pending);
        assert!(!drain.snapshot().active);
        assert!(drain.generation() > gen, "the stale supervisor must be retired");
    }

    /// AC2 — a pending roll converges without an operator: the retained roll's
    /// supervisor is still the current generation, so the very next tick that
    /// observes zero in-flight completes the restart. Driven through the same
    /// pure decision function the supervisor uses.
    #[test]
    fn test_pending_roll_rearms_and_completes_when_in_flight_hits_zero() {
        let drain = DrainState::new();
        let gen = match drain.begin(Duration::from_secs(1800), false, false) {
            DrainBegin::Started { generation, .. } => generation,
            other => panic!("expected Started, got {other:?}"),
        };
        let started = drain.snapshot().started_at.expect("started_at");

        // Deadline passes with work in flight ⇒ refuse ⇒ roll retained.
        assert_eq!(evaluate_drain_tick(3, true, false), DrainTick::TimedOutRefuse);
        assert!(matches!(
            drain.refuse_roll_deadline(started + chrono::Duration::seconds(1800)),
            RollRefusal::Deferred { .. }
        ));

        // Dispatch is still paused, so the in-flight set can actually reach zero.
        // The next tick that sees zero completes — with the relaunch exit code,
        // and from the SAME supervisor generation (nothing re-issued the command).
        assert_eq!(evaluate_drain_tick(0, true, false), DrainTick::Complete);
        assert_eq!(drain.generation(), gen);
        assert_eq!(drain_exit_code(drain.snapshot().then_exit), EXIT_RESTART);
    }

    /// Only a **relaunch (roll)** drain retains its intent. A then-exit teardown
    /// keeps the historical refuse-and-resume behavior, because `fleet drain`
    /// detects a remote refusal by observing `drain.draining == false` on a
    /// still-reachable daemon and reports it as its documented exit code 2.
    #[test]
    fn test_teardown_drain_keeps_the_historical_refuse_and_resume_path() {
        assert_eq!(drain_refusal_path(false), RefusalPath::RetainRoll);
        assert_eq!(drain_refusal_path(true), RefusalPath::ResumeDispatch);

        let drain = DrainState::new();
        let _ = drain.begin(Duration::from_secs(1800), false, true);
        assert!(drain.is_draining());
        // The supervisor's then-exit arm: resolve_timeout, exactly as before.
        drain.resolve_timeout(drain_timeout_refuse_note(2));
        assert!(!drain.is_draining(), "a refused teardown resumes dispatch immediately");
        assert!(!drain.snapshot().roll_pending);
        assert!(!drain.snapshot().active);
    }

    /// AC4 — the refusal message says what happens about the **recurrence**. The
    /// pre-#6007 note's advice ("re-run with a larger --timeout") is precisely
    /// what reproduced the livelock on a busy host, so the pending note must not
    /// give it, must not claim dispatch resumed, and must name both operator
    /// escape hatches.
    #[test]
    fn test_drain_roll_pending_note_addresses_the_recurrence() {
        let note =
            drain_roll_pending_note(3, 1, Duration::from_secs(3600), Duration::from_secs(7200));
        assert!(note.contains("3 sweep(s) still in flight"), "got: {note}");
        assert!(note.contains("ROLL PENDING (retry 1)"), "got: {note}");
        assert!(note.contains("stays PAUSED"), "got: {note}");
        assert!(note.contains("re-arms itself"), "got: {note}");
        assert!(note.contains("Nothing to re-run"), "got: {note}");
        assert!(note.contains("Next deadline in 3600s"), "got: {note}");
        assert!(note.contains("budget 7200s"), "got: {note}");
        assert!(note.contains("loom-daemon restart --abort-drain"), "got: {note}");
        assert!(
            note.contains("loom-daemon restart --drain --force-after-timeout"),
            "got: {note}"
        );
        // The two lies the old wording would tell on this path:
        assert!(
            !note.contains("dispatch resumed"),
            "dispatch is NOT resumed on a pending roll: {note}"
        );
        assert!(
            !note.contains("re-run with a larger --timeout if"),
            "must not re-issue the advice that reproduces the livelock: {note}"
        );
        assert!(!note.contains("fleet drain"), "got: {note}");
    }

    /// The give-up note keeps #5340's contract (same prefix, same exact retry
    /// command) and adds the recurrence advice that actually applies once the
    /// roll has already waited out its whole budget with dispatch paused: the
    /// sweep is stuck, so cancel it rather than widening the window again.
    #[test]
    fn test_drain_roll_abandoned_note_keeps_the_5340_contract() {
        let note = drain_roll_abandoned_note(2, 2, Duration::from_secs(7200));
        assert!(
            note.starts_with(
                "drain timed out with 2 sweep(s) still in flight — refused restart \
                 (no --force-after-timeout); dispatch resumed, daemon stays up."
            ),
            "the #4090/#5340 prefix must survive verbatim, got: {note}"
        );
        assert!(
            note.contains("loom-daemon restart --drain --force-after-timeout --timeout <secs>"),
            "got: {note}"
        );
        assert!(note.contains("re-armed 2 time(s)"), "got: {note}");
        assert!(note.contains("7200s of PAUSED dispatch"), "got: {note}");
        assert!(note.contains("ABANDONED"), "got: {note}");
        assert!(note.contains("loom-daemon cancel --sweep <id>"), "got: {note}");
        assert!(!note.contains("fleet drain"), "got: {note}");
    }

    /// The pending note tells the operator to run
    /// `restart --drain --force-after-timeout`; that command must actually do
    /// something. On a **pending** roll it escalates the force flag one-way and
    /// pulls the re-armed deadline in to now, so the next supervisor tick reaches
    /// `TimedOutForce` — while a first-attempt drain keeps #4521's pinning.
    #[test]
    fn test_force_escalation_applies_only_to_a_pending_roll() {
        let drain = DrainState::new();
        let base = Duration::from_secs(1800);
        let first_deadline = match drain.begin(base, false, false) {
            DrainBegin::Started { deadline, .. } => deadline,
            other => panic!("expected Started, got {other:?}"),
        };

        // Before any refusal: force stays pinned and the deadline does not move.
        match drain.begin(base, true, false) {
            DrainBegin::AlreadyDraining {
                force_escalated, ..
            } => assert!(!force_escalated, "no roll pending yet ⇒ #4521 pinning holds"),
            other => panic!("expected AlreadyDraining, got {other:?}"),
        }
        assert!(!drain.snapshot().force_after_timeout);
        assert_eq!(drain.snapshot().deadline, Some(first_deadline));

        // Refuse once ⇒ the roll is pending.
        let started = drain.snapshot().started_at.expect("started_at");
        assert!(matches!(
            drain.refuse_roll_deadline(started + chrono::Duration::seconds(1800)),
            RollRefusal::Deferred { .. }
        ));
        let rearmed = drain.snapshot().deadline.expect("re-armed deadline");

        // Now the documented force command takes effect immediately.
        match drain.begin(base, true, false) {
            DrainBegin::AlreadyDraining {
                force_escalated, ..
            } => assert!(force_escalated, "a pending roll honors --force-after-timeout"),
            other => panic!("expected AlreadyDraining, got {other:?}"),
        }
        assert!(drain.snapshot().force_after_timeout);
        let pulled_in = drain.snapshot().deadline.expect("deadline");
        assert!(
            pulled_in < rearmed,
            "the re-armed deadline must be pulled in to now ({pulled_in} vs {rearmed})"
        );
        assert!(pulled_in <= Utc::now(), "already past ⇒ the next tick forces through");
        assert_eq!(evaluate_drain_tick(2, true, true), DrainTick::TimedOutForce);
        assert!(
            drain
                .snapshot()
                .note
                .as_deref()
                .is_some_and(|n| n.contains("--force-after-timeout")),
            "the escalation must be visible in status"
        );
    }

    /// An operator's `--abort-drain` is the way OUT of a retained roll: it clears
    /// the pending state, resumes dispatch, and says so (a bare "dispatch
    /// resumed" would read identically to aborting a first-attempt drain).
    #[test]
    fn test_abort_clears_a_pending_roll_and_explains_itself() {
        let drain = DrainState::new();
        let _ = drain.begin(Duration::from_secs(1800), false, false);
        let started = drain.snapshot().started_at.expect("started_at");
        assert!(matches!(
            drain.refuse_roll_deadline(started + chrono::Duration::seconds(1800)),
            RollRefusal::Deferred { .. }
        ));
        assert!(drain.snapshot().roll_pending);

        assert!(drain.abort());
        assert!(!drain.is_draining());
        assert!(!drain.snapshot().roll_pending);
        let note = drain.snapshot().note.expect("abort records a note");
        assert!(note.contains("aborted"), "got: {note}");
        assert!(note.contains("pending roll"), "got: {note}");
    }

    /// A fresh drain never inherits a previous roll's retry history — otherwise a
    /// host that abandoned one roll would give up faster on the next.
    #[test]
    fn test_fresh_drain_resets_the_pending_roll_bookkeeping() {
        let drain = DrainState::new();
        let _ = drain.begin(Duration::from_secs(1800), false, false);
        let started = drain.snapshot().started_at.expect("started_at");
        let _ = drain.refuse_roll_deadline(started + chrono::Duration::seconds(1800));
        assert_eq!(drain.snapshot().refusals, 1);
        assert!(drain.abort());

        let _ = drain.begin(Duration::from_secs(600), false, false);
        let snap = drain.snapshot();
        assert_eq!(snap.refusals, 0, "retry history must not leak across drains");
        assert!(!snap.roll_pending);
        assert_eq!(snap.base_timeout, Duration::from_secs(600));
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

        let resp = handle_drain_request(&drain, &pool, &root, &bus, Some(60), false, false);
        match resp {
            Response::DaemonDrain {
                accepted,
                supervisor,
                message,
                ..
            } => {
                assert!(!accepted, "unsupervised host must refuse the drain");
                assert!(supervisor.is_none());
                // #4640: the refusal must mention the systemd retrofit for a
                // fleet worker provisioned before the fix (missing
                // LOOM_DAEMON_SUPERVISOR despite being systemd-supervised).
                assert!(
                    message.contains("LOOM_DAEMON_SUPERVISOR=systemd"),
                    "drain refusal must mention the systemd retrofit: {message}"
                );
                assert!(
                    message.contains("Restart=on-success"),
                    "drain refusal retrofit hint must include the corrected Restart= policy: {message}"
                );
            }
            other => panic!("expected DaemonDrain, got {other:?}"),
        }
        assert!(!drain.is_draining(), "refused drain must NOT pause dispatch");
        assert_eq!(drain.generation(), 0, "refused drain must not bump generation");

        std::env::remove_var(crate::workspace_registry::REGISTRY_PATH_ENV);
    }

    /// Issue #4521 AC1 — the `AlreadyDraining` ack reports the ACTIVE drain's
    /// terminal action, not the requested one.
    ///
    /// The pre-fix behavior echoed the request: an operator's
    /// `--drain --then-exit` landing on an in-progress auto-update roll-drain
    /// was acked `then_exit: true` ("will stop") while the daemon exited 0 and
    /// launchd relaunched it — the exact incident shape.
    ///
    /// No supervisor task is spawned on this path (only `DrainBegin::Started`
    /// spawns one), so the drain state is primed with `DrainState::begin`
    /// directly and the process is never at risk of the supervisor's `exit`.
    #[test]
    #[serial_test::serial]
    fn test_already_draining_ack_reports_active_terminal_action() {
        let (sr, dir, _rec) = setup_sweep_registry_in_tempdir();
        let root = dir.path().to_path_buf();
        let empty_reg = dir.path().join("no-such-workspaces.json");
        std::env::set_var(crate::workspace_registry::REGISTRY_PATH_ENV, &empty_reg);
        let pool = Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), test_runtime_handle()));
        pool.seed(root.clone(), sr);
        let bus = Arc::new(EventBus::new());

        // An auto-update roll-drain is already in flight (`then_exit=false`).
        let drain = Arc::new(DrainState::new());
        let _ = drain.begin(Duration::from_secs(600), false, false);

        // Operator teardown request during that window. `then_exit=true` skips
        // the supervisor gate, so no `LOOM_DAEMON_SUPERVISOR` is needed.
        let resp = handle_drain_request(&drain, &pool, &root, &bus, Some(60), false, true);
        match resp {
            Response::DaemonDrain {
                accepted,
                then_exit,
                ref message,
                ..
            } => {
                assert!(accepted);
                assert!(
                    then_exit,
                    "the ack must report the ACTIVE drain's terminal action (escalated to \
                     stay-down), not a blind echo"
                );
                assert!(message.contains("ESCALATED"), "message was: {message}");
            }
            other => panic!("expected DaemonDrain, got {other:?}"),
        }
        assert!(drain.snapshot().then_exit, "the active drain now stays down");

        // The reverse: a plain relaunch drain request against the now-teardown
        // drain must be acked with `then_exit: true` — it is NOT downgraded, and
        // the ack must not promise a restart that will never happen.
        std::env::set_var("LOOM_DAEMON_SUPERVISOR", "launchd");
        let resp = handle_drain_request(&drain, &pool, &root, &bus, Some(60), false, false);
        match resp {
            Response::DaemonDrain {
                accepted,
                then_exit,
                ref message,
                ..
            } => {
                assert!(accepted);
                assert!(then_exit, "then-exit is never downgraded");
                assert!(
                    message.contains("not be honored") || message.contains("NOT relaunch"),
                    "message was: {message}"
                );
            }
            other => panic!("expected DaemonDrain, got {other:?}"),
        }
        assert!(drain.snapshot().then_exit);

        std::env::remove_var("LOOM_DAEMON_SUPERVISOR");
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
        // Pre-#4055 payload has no auto_update fields either — they default.
        assert!(!report.auto_update_enabled, "absent auto_update (#4055) defaults to disabled");
        assert_eq!(report.auto_update_last_check, None);
        assert_eq!(report.auto_update_last_roll, None);
        assert_eq!(report.auto_update_consecutive_failures, 0);
        assert_eq!(report.auto_update_backoff_secs, None);
        assert_eq!(report.auto_update_terminal_reason, None);
        assert_eq!(report.auto_update_note, None);
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
        let deadline = match drain.begin(Duration::from_secs(300), false, false) {
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
            then_exit: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        match back {
            Request::DrainAndRestartDaemon {
                timeout_secs,
                force_after_timeout,
                then_exit,
            } => {
                assert_eq!(timeout_secs, Some(600));
                assert!(force_after_timeout);
                assert!(!then_exit);
            }
            other => panic!("expected DrainAndRestartDaemon, got {other:?}"),
        }

        // Pre-#4343 wire data (no `then_exit` key at all) still parses, as
        // `then_exit: false` — the original #4090 restart-when-drained
        // behavior.
        let legacy_json = r#"{"type":"DrainAndRestartDaemon","payload":{"timeout_secs":600,"force_after_timeout":true}}"#;
        let back: Request = serde_json::from_str(legacy_json).unwrap();
        match back {
            Request::DrainAndRestartDaemon { then_exit, .. } => assert!(!then_exit),
            other => panic!("expected DrainAndRestartDaemon, got {other:?}"),
        }

        // AbortDrain round-trips (unit-with-payload-none shape).
        let json = serde_json::to_string(&Request::AbortDrain).unwrap();
        assert_eq!(json, r#"{"type":"AbortDrain"}"#);
        let back: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Request::AbortDrain));

        // The DaemonDrain response round-trips, including `then_exit`.
        let resp = Response::DaemonDrain {
            accepted: true,
            supervisor: Some("launchd".to_string()),
            in_flight: 3,
            message: "draining".to_string(),
            then_exit: true,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: Response = serde_json::from_str(&json).unwrap();
        match back {
            Response::DaemonDrain {
                accepted,
                supervisor,
                in_flight,
                then_exit,
                ..
            } => {
                assert!(accepted);
                assert_eq!(supervisor.as_deref(), Some("launchd"));
                assert_eq!(in_flight, 3);
                assert!(then_exit);
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
        let gen = match drain.begin(Duration::from_secs(300), false, false) {
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
