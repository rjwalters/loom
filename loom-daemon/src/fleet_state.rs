//! Enforcement of the fleet store's desired run state (Issue #9598).
//!
//! [`crate::fleet_store::state`] *reads* `fleet/state.yml` and
//! `loom-daemon fleet-config state` *reports* it. Nothing acted on it: an
//! operator who set `stopped` still had to stop each host by hand, and a host
//! that rebooted — or that a watchdog revived — came back dispatching whatever
//! the store said. This module closes that gap.
//!
//! # The three states
//!
//! | Store says | This host does |
//! |---|---|
//! | `running` | nothing — normal dispatch. Releases a hold this module placed. |
//! | `paused` | stays up with **new dispatch held**; in-flight work finishes. |
//! | `stopped` | **refuses to start**, or (already running) drains and exits. |
//!
//! `paused` and `stopped` both go through #4090's existing drain machinery
//! ([`crate::ipc::DrainState`]) rather than a second flag:
//!
//! - `paused` is [`DrainState::hold_for_fleet_state`] — the same
//!   supervisor-less hold #9588 uses for an operator-stop record on a
//!   relaunch, which sets the one atomic the work finder, role runner and epic
//!   supervisor already fold into their halt checks. "In-flight work finishes"
//!   is then free: the flag gates *admission*, never a running sweep.
//! - `stopped` at runtime is [`crate::ipc::handle_drain_request`] with
//!   `then_exit`, exactly what `fleet drain` (#4343) and
//!   `restart --drain --then-exit` use — so it also writes #9588's
//!   operator-stop record, which is what stops the watchdog from reviving the
//!   host it just took down.
//!
//! # Safety invariants
//!
//! - **Unset `fleet.repo` changes nothing.** [`crate::fleet_sync::start`]
//!   returns `None` before this module is consulted, so a host that has not
//!   opted in never reads a state, never holds and never refuses to start.
//! - **An unreachable forge never silently resumes a stopped host.** The read
//!   is [`Policy::AllowStale`], so the last good *cached* snapshot answers, with
//!   the staleness warning attached. If even the cache is gone,
//!   [`state_pass`] falls back to the state this host last **recorded** (the
//!   `fleet-sync-status.json` snapshot) rather than to `running`. Only a host
//!   that has never successfully read a state at all proceeds normally — there
//!   is nothing to honour, and refusing to boot on no evidence would strand the
//!   fleet on its first bad fetch.
//! - **A local operator stop outranks the store.** A #9588 operator-stop hold,
//!   a supervised `restart --drain` and an auto-update roll are never released
//!   by a store that says `running`; only a hold *this* module placed is
//!   ([`DrainState::release_fleet_hold`]).

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::fleet_store::fetch::{self, Freshness, Policy, Transport};
use crate::fleet_store::state::{self, RunState};
use crate::fleet_store::{self as store, StoreLocation};

/// Exit code for a startup refused because the store says `stopped`.
///
/// Deliberately **not** `0`: `0` is [`crate::ipc::EXIT_RESTART`], the one code
/// that asks a supervisor to relaunch — the exact opposite of the intent here.
/// Every non-zero code means "do not relaunch" to launchd
/// `KeepAlive:SuccessfulExit` and systemd `Restart=on-success`, and this one is
/// distinct from [`crate::ipc::EXIT_STARTUP_FAILURE`] (`1`) so an operator, a
/// log scraper or a wrapper script can tell "refused by fleet policy" from
/// "crashed on boot" without parsing prose.
pub const EXIT_FLEET_STOPPED: i32 = 79;

/// What a desired run state requires of this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Enforcement {
    /// Dispatch normally. `running`, and every case where no state is known.
    Proceed,
    /// Stay up, admit no new dispatch, let in-flight work finish. `paused`.
    Hold,
    /// Do not dispatch at all: refuse to start, or drain and exit. `stopped`.
    Stop,
}

impl Enforcement {
    /// Stable machine name (`status --json`, the host-level snapshot, events).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proceed => "proceed",
            Self::Hold => "hold",
            Self::Stop => "stop",
        }
    }
}

/// One read of `fleet/state.yml`, and what it requires.
///
/// Owns its strings (unlike [`crate::fleet_store::state::HostState`], whose
/// `source` is a `&'static str`) because this is persisted into the host-level
/// `fleet-sync-status.json` snapshot and read back by `loom-daemon status`.
/// Container-level `#[serde(default)]` so a snapshot written by a daemon that
/// knew fewer of these fields still reads back — the same forward-compatibility
/// the `#[serde(default)]` on [`crate::fleet_sync::FleetSyncStatus::state`]
/// gives the field as a whole.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StatePass {
    /// The desired state, when one could be resolved.
    pub desired: Option<RunState>,
    /// `host` (this host's own entry) or `fleet` (the fleet-wide default).
    pub source: Option<String>,
    /// Whether the snapshot came from the cache rather than the forge.
    pub cached: bool,
    /// Whether [`desired`](Self::desired) came from this host's previously
    /// recorded state rather than from any snapshot — the no-cache fallback.
    pub from_last_recorded: bool,
    /// The staleness warning for a cached snapshot, when there is one.
    pub staleness: Option<String>,
    /// `since`, from the entry that set the state.
    pub since: Option<String>,
    /// `by`, from the entry that set the state.
    pub by: Option<String>,
    /// `reason`, from the entry that set the state.
    pub reason: Option<String>,
    /// Why no state could be read, when none could.
    pub error: Option<String>,
}

impl StatePass {
    /// What this read requires of the host.
    ///
    /// The whole decision, and pure: `paused` holds, `stopped` stops, and
    /// **everything else proceeds** — `running`, a store with no
    /// `fleet/state.yml`, an unparseable one, a state naming no default for
    /// this host, and an unreachable forge with nothing cached. A `desired`
    /// resolved from a cached snapshot or from the last recorded state is
    /// honoured exactly as a live one is; that is the invariant that keeps a
    /// forge outage from resuming a `stopped` host.
    #[must_use]
    pub fn enforcement(&self) -> Enforcement {
        match self.desired {
            Some(RunState::Paused) => Enforcement::Hold,
            Some(RunState::Stopped) => Enforcement::Stop,
            Some(RunState::Running) | None => Enforcement::Proceed,
        }
    }

    /// Where the answer came from, for a human-readable message.
    fn provenance(&self) -> &'static str {
        if self.from_last_recorded {
            "this host's last recorded state (no snapshot was readable)"
        } else if self.cached {
            "the last good CACHED snapshot"
        } else {
            "the store"
        }
    }

    /// How the entry that set the state described itself, e.g.
    /// `" (host entry, by operator at 2026-01-01T00:00Z: draining for maintenance)"`.
    /// Empty when the store recorded no metadata at all.
    fn attribution(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(s) = &self.source {
            parts.push(if s == "host" {
                "host entry".to_string()
            } else {
                "fleet default".to_string()
            });
        }
        if let Some(by) = &self.by {
            parts.push(format!("by {by}"));
        }
        if let Some(at) = &self.since {
            parts.push(format!("since {at}"));
        }
        if let Some(r) = &self.reason {
            parts.push(format!("reason: {r}"));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(" ({})", parts.join(", "))
        }
    }
}

/// Read `host`'s desired run state from the store.
///
/// Never returns `Err`: a failure lands in [`StatePass::error`] so a caller on
/// the boot path can log it and carry on. `last_recorded` is this host's
/// previously recorded state (from the `fleet-sync-status.json` snapshot) and is
/// consulted **only** when no snapshot could be read at all — the invariant that
/// an unreachable forge with a wiped cache still does not resume a host the
/// operator stopped.
pub fn state_pass(
    transport: &dyn Transport,
    cache_dir: &Path,
    location: &StoreLocation,
    host: &str,
    last_recorded: Option<RunState>,
    now: DateTime<Utc>,
) -> StatePass {
    let loaded = match fetch::load(transport, cache_dir, location, Policy::AllowStale, now) {
        Ok(l) => l,
        Err(e) => {
            return StatePass {
                desired: last_recorded,
                from_last_recorded: last_recorded.is_some(),
                cached: true,
                error: Some(format!("{e:#}")),
                ..StatePass::default()
            }
        }
    };
    let cached = matches!(loaded.freshness, Freshness::Cached { .. });
    let staleness = loaded.staleness_warning(now);
    match state::resolve_snapshot(&loaded.snapshot, host) {
        Ok(None) => StatePass {
            cached,
            staleness,
            error: Some(format!(
                "{} — nothing to enforce",
                state::missing_message(&loaded.snapshot)
            )),
            ..StatePass::default()
        },
        Ok(Some(hs)) => StatePass {
            desired: Some(hs.state),
            source: Some(hs.source.to_string()),
            cached,
            from_last_recorded: false,
            staleness,
            since: hs.since,
            by: hs.by,
            reason: hs.reason,
            error: None,
        },
        Err(e) => StatePass {
            cached,
            staleness,
            error: Some(format!("{e:#}")),
            ..StatePass::default()
        },
    }
}

/// The message printed to stderr, and logged, when a start is refused because
/// the store says `stopped`.
///
/// Says all four things an operator needs at 3am: that this is deliberate and
/// not a crash, which host entry decided it, where the answer came from (live,
/// cached, or last recorded), and the one command that changes it.
#[must_use]
pub fn refusal_message(pass: &StatePass, host: &str, repo: &str) -> String {
    let mut out = format!(
        "loom-daemon: refusing to start — the fleet store {repo} says host `{host}` is \
         stopped{}. This is the operator's recorded intent, NOT a crash: the daemon exits \
         {EXIT_FLEET_STOPPED} and no supervisor should revive it.\n  read from: {}\n  to start \
         this host again, set it running in the store:\n    loom-daemon fleet-config propose \
         state running --host {host} --reason '<why>'\n  then merge that PR. To start once \
         without changing the store, unset `{}` / `{}` for this process.",
        pass.attribution(),
        pass.provenance(),
        store::FLEET_REPO_KEY,
        store::FLEET_REPO_ENV,
    );
    if let Some(w) = &pass.staleness {
        out.push_str(&format!("\n  {w}"));
    }
    if let Some(e) = &pass.error {
        out.push_str(&format!("\n  note: {e}"));
    }
    out
}

/// The drain note recorded on a `paused` hold, surfaced by `loom-daemon status`.
#[must_use]
pub fn hold_note(pass: &StatePass, host: &str, repo: &str) -> String {
    let stale = pass
        .staleness
        .as_ref()
        .map(|w| format!(" [{w}]"))
        .unwrap_or_default();
    format!(
        "dispatch HELD: the fleet store {repo} says host `{host}` is paused{}, read from {}. \
         In-flight work finishes; no new sweep or role tick is admitted. Set it running in the \
         store (`loom-daemon fleet-config propose state running --host {host}`) to resume; \
         `restart --abort-drain` resumes only until the next sync pass.{stale}",
        pass.attribution(),
        pass.provenance(),
    )
}

/// The reason recorded on a drain-and-exit triggered by a `stopped` state that
/// landed while the daemon was already running.
#[must_use]
pub fn stop_reason(pass: &StatePass, host: &str, repo: &str) -> String {
    format!(
        "the fleet store {repo} says host `{host}` is stopped{} (read from {}) — draining and \
         exiting; in-flight work finishes first",
        pass.attribution(),
        pass.provenance(),
    )
}

// ============================================================================
// The boot decision
// ============================================================================

/// Act on the desired run state at boot, and return the `paused` hold note.
///
/// Called from [`crate::fleet_sync::start`], which is the earliest point at
/// which the state is known and still before any dispatch producer, IPC
/// listener or role loop exists.
///
/// - `stopped` **never returns**: the refusal goes to the log *and* to stderr
///   (a wrapper script's own transcript is often all an operator has at 3am),
///   and the process exits [`EXIT_FLEET_STOPPED`] through the observability
///   shutdown path so queued telemetry still flushes. There is nothing to
///   drain — nothing has started.
/// - `paused` returns the note for [`crate::ipc::DrainState::with_fleet_hold`];
///   the caller cannot apply it here because the drain state does not exist yet.
/// - `running`, an unreadable state and an unset `fleet.repo` all return `None`.
pub async fn enforce_at_boot(pass: &StatePass, host: &str, repo: &str) -> Option<String> {
    match pass.enforcement() {
        Enforcement::Proceed => None,
        Enforcement::Hold => Some(hold_note(pass, host, repo)),
        Enforcement::Stop => {
            let message = refusal_message(pass, host, repo);
            log::warn!("{message}");
            eprintln!("{message}");
            crate::observability::shutdown::exit(EXIT_FLEET_STOPPED).await
        }
    }
}

/// Build the daemon's [`crate::ipc::DrainState`] and arm run-state enforcement
/// on it (#9598) — the whole wiring of this module into `run_daemon`, in one
/// call so the boot sequence carries a single line for it.
///
/// `started` is [`crate::fleet_sync::start`]'s result: `None` on every host with
/// no `fleet.repo`, in which case this is exactly the pre-#9598
/// `DrainState::with_default_ledger()` and no timer is armed at all.
/// Otherwise the returned drain state starts held when the store says `paused`,
/// and the `fleet.syncIntervalSecs` timer enforces every later change through
/// it: holding, releasing a hold *this* mechanism placed, or draining to exit.
///
/// The returned [`tokio::task::JoinHandle`] is deliberately dropped: a dropped
/// handle detaches the task, which then runs for the life of the process —
/// which is exactly as long as the timer is wanted.
#[must_use]
pub fn wire(
    started: Option<crate::fleet_sync::Started>,
    workspace_pool: &std::sync::Arc<crate::workspace_pool::WorkspacePool>,
    event_bus: &std::sync::Arc<crate::event_bus::EventBus>,
) -> std::sync::Arc<crate::ipc::DrainState> {
    let hold = started.as_ref().and_then(|s| s.hold_note.clone());
    let drain =
        std::sync::Arc::new(crate::ipc::DrainState::with_default_ledger().with_fleet_hold(hold));
    if let Some(started) = started {
        let enforcer: std::sync::Arc<dyn Enforcer> = std::sync::Arc::new(IpcEnforcer::new(
            drain.clone(),
            workspace_pool.clone(),
            started.workspace().to_path_buf(),
            event_bus.clone(),
            tokio::runtime::Handle::current(),
        ));
        drop(started.spawn_timer(Some(enforcer)));
    }
    drain
}

// ============================================================================
// The timer decision
// ============================================================================

/// What a sync pass must do about the state it just read, on a daemon that is
/// already up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerAction {
    /// Nothing to do — the host is already in the state the store wants.
    None,
    /// Place a fleet-state hold: `paused`, and not already held.
    Hold,
    /// Release the fleet-state hold: `running` again, and currently held.
    Release,
    /// Drain and exit: `stopped`.
    Stop,
}

/// The whole timer decision, pure.
///
/// `fleet_held` is [`DrainState::is_fleet_held`] — whether dispatch is held by
/// *this* mechanism specifically. That scoping is the point: a `running` store
/// releases only a hold this module placed, never a #9588 operator-stop hold, a
/// supervised operator drain or an auto-update roll.
///
/// `Hold` is emitted even when some other drain is already pausing dispatch;
/// [`DrainState::hold_for_fleet_state`] is the idempotent guard there and
/// declines rather than stealing an operator's drain.
#[must_use]
pub fn timer_action(enforcement: Enforcement, fleet_held: bool) -> TimerAction {
    match (enforcement, fleet_held) {
        (Enforcement::Stop, _) => TimerAction::Stop,
        (Enforcement::Hold, false) => TimerAction::Hold,
        (Enforcement::Hold, true) => TimerAction::None,
        (Enforcement::Proceed, true) => TimerAction::Release,
        (Enforcement::Proceed, false) => TimerAction::None,
    }
}

// ============================================================================
// The enforcement seam
// ============================================================================

/// How a sync pass acts on a change of desired run state.
///
/// A trait so the timer's decision can be tested without a daemon: production
/// passes [`IpcEnforcer`], tests pass a recorder. Every method reports whether
/// it changed anything, so a pass can log the transition rather than the
/// state.
pub trait Enforcer: Send + Sync {
    /// Hold new dispatch (`paused`). `true` when this call newly held.
    fn hold(&self, note: String) -> bool;

    /// Release a hold this mechanism placed (`running` again). `true` when a
    /// fleet-state hold was actually released.
    fn release(&self) -> bool;

    /// Whether dispatch is currently held by this mechanism.
    fn is_held(&self) -> bool;

    /// Drain and exit (`stopped`). `true` when the drain was accepted.
    fn stop(&self, reason: String) -> bool;

    /// What the workspace resync's host gate reads about dispatch and rolls
    /// (#10718). The default knows only about this mechanism's own hold.
    fn drain_facts(&self) -> DrainFacts {
        DrainFacts {
            draining: self.is_held(),
            ..DrainFacts::default()
        }
    }
}

/// The drain-state facts the workspace resync's host gate reads (#10718).
///
/// Named fields rather than a tuple so a later signal (PR 3's
/// `resume_pending`, #10832) is one more field, not a reshuffled tuple.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrainFacts {
    /// Dispatch is paused for any reason: a drain, a pause roll, a fleet-state
    /// `paused` hold or an operator-stop hold.
    pub draining: bool,
    /// A pause roll is armed, committed or in progress (#10831): see
    /// [`crate::ipc::DrainState::pause_roll_in_progress`]. Never set by a fleet-state hold
    /// or an operator drain.
    pub roll_in_progress: bool,
}

/// The production [`Enforcer`]: #4090's drain primitives, nothing new.
///
/// `hold`/`release` are [`DrainState`]'s supervisor-less fleet hold.
/// [`Self::stop`] is [`crate::ipc::handle_drain_request`] with
/// `then_exit = true` and `force_after_timeout = false` — the same call
/// `fleet drain` (#4343) and `restart --drain --then-exit` make, so it records
/// #9588's operator-stop marker (the watchdog then leaves the host down) and
/// **never cancels a sweep**: if in-flight work has not finished by the drain
/// deadline the operator drain holds dispatch paused rather than killing it.
pub struct IpcEnforcer {
    drain: std::sync::Arc<crate::ipc::DrainState>,
    workspace_pool: std::sync::Arc<crate::workspace_pool::WorkspacePool>,
    fallback_root: std::path::PathBuf,
    event_bus: std::sync::Arc<crate::event_bus::EventBus>,
    handle: tokio::runtime::Handle,
}

impl IpcEnforcer {
    /// Wire an enforcer to the daemon's live drain state.
    #[must_use]
    pub fn new(
        drain: std::sync::Arc<crate::ipc::DrainState>,
        workspace_pool: std::sync::Arc<crate::workspace_pool::WorkspacePool>,
        fallback_root: std::path::PathBuf,
        event_bus: std::sync::Arc<crate::event_bus::EventBus>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            drain,
            workspace_pool,
            fallback_root,
            event_bus,
            handle,
        }
    }
}

impl Enforcer for IpcEnforcer {
    fn hold(&self, note: String) -> bool {
        self.drain.hold_for_fleet_state(note)
    }

    fn release(&self) -> bool {
        self.drain.release_fleet_hold()
    }

    fn is_held(&self) -> bool {
        self.drain.is_fleet_held()
    }

    fn drain_facts(&self) -> DrainFacts {
        DrainFacts {
            draining: self.drain.is_draining(),
            roll_in_progress: self.drain.pause_roll_in_progress(),
        }
    }

    fn stop(&self, reason: String) -> bool {
        // A fleet-state hold has no supervisor behind it, so hand the pause over
        // to a real supervised drain: `begin_as` replaces a `startup_hold`
        // rather than acking it, and only a supervisor can perform the exit.
        // Enter the runtime so that spawn resolves one from a blocking thread —
        // the same reason the auto-updater's roll trigger does (#4090).
        let _guard = self.handle.enter();
        log::warn!("fleet_state: {reason}");
        let resp = crate::ipc::handle_drain_request(
            &self.drain,
            &self.workspace_pool,
            &self.fallback_root,
            &self.event_bus,
            // Default drain deadline; `force_after_timeout = false` is the
            // fail-safe — an operator drain past its deadline holds dispatch
            // paused (#9588) instead of cancelling in-flight sweeps.
            None,
            false,
            true,
            crate::ipc::DrainOrigin::Operator,
        );
        if let crate::types::Response::DaemonDrain {
            accepted: false,
            message,
            ..
        } = &resp
        {
            // The one expected refusal is "no supervisor detected": exiting then
            // would leave the host down with nothing to relaunch it, which is
            // what the store wants — but the *hold* still applies, so fall back
            // to it rather than leaving the host dispatching.
            log::warn!(
                "fleet_state: the drain-and-exit for a `stopped` state was refused ({message}) — \
                 holding dispatch instead, so this host admits no new work while it stays up"
            );
            self.drain.hold_for_fleet_state(reason);
            return false;
        }
        true
    }
}

/// The outcome of one [`apply`] call: what actually happened, not what was
/// asked for.
///
/// The two halves answer different questions and are deliberately separate:
/// `log` is non-`None` only on a **transition** (so a steady-state paused host
/// is silent tick after tick), while `effective` is reported **every** tick as
/// the *actual* half of `status`'s desired-vs-actual pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// A line for the log and the event bus, when this call changed something.
    pub log: Option<String>,
    /// What this host is actually doing now. Equal to the requirement in every
    /// case except a refused drain-and-exit, which degrades `Stop` to `Hold`.
    pub effective: Enforcement,
}

/// Apply `action` through `enforcer`.
///
/// `required` is what the store asked for ([`StatePass::enforcement`]); the
/// returned [`Applied::effective`] is what the host ended up in. They differ in
/// exactly one case — [`Enforcer::stop`] refused (no supervisor to perform the
/// exit) and [`IpcEnforcer`] fell back to holding dispatch instead — and that
/// divergence is the whole reason `status` reports both.
pub fn apply(
    action: TimerAction,
    required: Enforcement,
    enforcer: &dyn Enforcer,
    pass: &StatePass,
    host: &str,
    repo: &str,
) -> Applied {
    match action {
        TimerAction::None => Applied {
            log: None,
            effective: required,
        },
        TimerAction::Hold => {
            let note = hold_note(pass, host, repo);
            Applied {
                log: enforcer
                    .hold(note.clone())
                    .then(|| format!("fleet state now `paused` — {note}")),
                effective: Enforcement::Hold,
            }
        }
        TimerAction::Release => Applied {
            log: enforcer.release().then(|| {
                format!("fleet state now `running` — released the fleet-state hold on `{host}`")
            }),
            effective: Enforcement::Proceed,
        },
        TimerAction::Stop => {
            let reason = stop_reason(pass, host, repo);
            if enforcer.stop(reason.clone()) {
                Applied {
                    log: Some(format!("fleet state now `stopped` — {reason}")),
                    effective: Enforcement::Stop,
                }
            } else {
                // The drain was refused. `IpcEnforcer` holds dispatch instead,
                // so the host admits no new work even though it stays up —
                // report that honestly rather than claiming it stopped.
                let effective = if enforcer.is_held() {
                    Enforcement::Hold
                } else {
                    Enforcement::Proceed
                };
                Applied {
                    log: Some(format!(
                        "fleet state is `stopped` but the drain-and-exit was refused — this host \
                         is {} instead. {reason}",
                        match effective {
                            Enforcement::Hold => "HOLDING new dispatch",
                            _ => "still dispatching",
                        }
                    )),
                    effective,
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "fleet_state/tests.rs"]
mod tests;
