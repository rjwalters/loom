//! Opt-in, supervised recovery for a LIVE daemon that has stopped answering
//! IPC (#7855). **Default OFF: an unchanged host stays report-only.**
//!
//! # Why this is opt-in and narrow
//!
//! #4398 made a CONFIRMED hang report-only on purpose: the only real fix for a
//! wedged-but-alive process is ending it, and the same failed round-trip is
//! also what a daemon under heavy legitimate load looks like (the 2026-09-16
//! robb-pro wedge sat at load 16.8 on 18 cores). An automatic restart keyed on
//! IPC alone would misfire on exactly those hosts. The owner's 2026-09-18
//! ruling therefore accepted recovery only under all of:
//!
//! 1. **Opt-in.** `LOOM_WATCHDOG_HANG_RECOVER` or
//!    `autonomous.watchdogHangRecover.enabled` at start, persisted to the
//!    autonomy marker's `watchdog_hang_recover=` field (which is how the
//!    setting reaches the scheduled job — see [`Settings`]), default false.
//! 2. **Two independent signals, sustained.** At least
//!    [`HANG_RECOVER_MIN_CONFIRMATIONS`] consecutive CONFIRMED ticks on which
//!    the IPC round-trip failed AND the heartbeat is *positively* stale for the
//!    current boot ([`classify_heartbeat`]). A fresh, missing, unreadable or
//!    prior-boot heartbeat is not evidence of a wedge, so it never counts —
//!    and it resets the streak. This is a separate counter from the raw
//!    IPC-failure streak (#4398), which keeps its own threshold and meaning.
//! 3. **Durably rate-limited.** At most one restart per cooldown window
//!    (floor 30 min), recorded in a state file that is NOT keyed by pid — it
//!    has to survive the restart it causes and any watchdog restart — and
//!    serialised by a lock so two concurrent watchdog invocations cannot both
//!    act. Restarts without an intervening healthy tick are also capped.
//! 4. **Supervised only.** `launchctl kickstart -k` of the loaded launchd job,
//!    or `systemctl --user restart` of the known unit. Never a bare signal,
//!    never an unsupervised fallback, never anything over the wedged socket.
//!    A pid-file-only daemon has no supervisor, so it stays report-only.
//!
//! Every decision here is a value the caller reports; nothing in this module
//! writes the watchdog log, so the decision is testable without a daemon.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::daemon_install_state::HeartbeatFreshness;

use super::consts::{
    HANG_RECOVER_DEFAULT_MAX_UNHEALED, HANG_RECOVER_MIN_CONFIRMATIONS,
    HANG_RECOVER_MIN_COOLDOWN_SECS,
};
use super::env;
use super::liveness::Source;

/// The opt-in knob. Environment wins over the marker; see [`Settings`].
pub const ENV_ENABLE: &str = "LOOM_WATCHDOG_HANG_RECOVER";
pub const ENV_CONFIRMATIONS: &str = "LOOM_WATCHDOG_HANG_RECOVER_CONFIRMATIONS";
pub const ENV_COOLDOWN: &str = "LOOM_WATCHDOG_HANG_RECOVER_COOLDOWN_SECS";
pub const ENV_MAX_UNHEALED: &str = "LOOM_WATCHDOG_HANG_RECOVER_MAX_UNHEALED";

/// The marker fields `loom-daemon-start.sh` persists from the variables above.
pub const MARKER_ENABLE: &str = "watchdog_hang_recover";
pub const MARKER_CONFIRMATIONS: &str = "watchdog_hang_recover_confirmations";
pub const MARKER_COOLDOWN: &str = "watchdog_hang_recover_cooldown_secs";
pub const MARKER_MAX_UNHEALED: &str = "watchdog_hang_recover_max_unhealed";

/// The matching `.loom/config.json` keys. Read by the START (which has the repo
/// root and the operator's config tiers), never by the scheduled watchdog job,
/// which reads only the marker — see [`Settings`].
pub const CONFIG_ENABLE: &str = "autonomous.watchdogHangRecover.enabled";
pub const CONFIG_CONFIRMATIONS: &str = "autonomous.watchdogHangRecover.confirmations";
pub const CONFIG_COOLDOWN: &str = "autonomous.watchdogHangRecover.cooldownSecs";
pub const CONFIG_MAX_UNHEALED: &str = "autonomous.watchdogHangRecover.maxUnhealed";

/// `(environment variable, marker field, config key)` for every setting, in one
/// place so the start-side writer and the watchdog-side reader cannot drift
/// apart.
pub const SETTING_KEYS: [(&str, &str, &str); 4] = [
    (ENV_ENABLE, MARKER_ENABLE, CONFIG_ENABLE),
    (ENV_CONFIRMATIONS, MARKER_CONFIRMATIONS, CONFIG_CONFIRMATIONS),
    (ENV_COOLDOWN, MARKER_COOLDOWN, CONFIG_COOLDOWN),
    (ENV_MAX_UNHEALED, MARKER_MAX_UNHEALED, CONFIG_MAX_UNHEALED),
];

/// A lock older than this is reaped. Far above the bounded hang-restart command
/// (120s), so a live holder is never mistaken for a crashed one.
const LOCK_STALE: Duration = Duration::from_secs(600);

/// The resolved hang-recovery settings for this tick.
///
/// # How the setting reaches the scheduled job
///
/// The watchdog runs from a launchd `StartInterval` job / systemd timer whose
/// environment is rendered once by `daemon_start::watchdog_job` and carries
/// only paths — an operator's shell variables never reach it. What it DOES
/// read on every tick is the autonomy-desired marker, which
/// `loom-daemon-start.sh` rewrites on each start. So the start resolves
/// `LOOM_WATCHDOG_HANG_RECOVER` > `autonomous.watchdogHangRecover.enabled` >
/// the prior marker's value (and the same for the three tunables), persists the
/// result into the marker, and this resolves **environment > marker > default
/// (off)**: the environment only matters for a hand-run watchdog or a test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    /// `env`, `marker` or `default` — reported so an operator can see WHY.
    pub source: &'static str,
    pub confirmations: u64,
    pub cooldown_secs: u64,
    pub max_unhealed: u64,
}

impl Settings {
    /// Resolve from injected lookups, so tests never touch process env.
    #[must_use]
    pub fn resolve(
        env_lookup: impl Fn(&str) -> Option<String>,
        marker_lookup: impl Fn(&str) -> Option<String>,
    ) -> Self {
        let (enabled, source) = match env_lookup(ENV_ENABLE).as_deref() {
            Some(v) if env::is_true(v) => (true, "env"),
            Some(v) if env::is_false(v) => (false, "env"),
            _ => match marker_lookup(MARKER_ENABLE).as_deref() {
                Some(v) if env::is_true(v) => (true, "marker"),
                Some(v) if env::is_false(v) => (false, "marker"),
                _ => (false, "default"),
            },
        };
        let num = |env_key: &str, marker_key: &str| -> Option<u64> {
            let digits = |s: String| {
                (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                    .then(|| s.parse::<u64>().ok())
                    .flatten()
            };
            env_lookup(env_key)
                .and_then(digits)
                .or_else(|| marker_lookup(marker_key).and_then(digits))
        };
        Self {
            enabled,
            source,
            // Floors, not just defaults: the ruling's bounds are minimums.
            confirmations: num(ENV_CONFIRMATIONS, MARKER_CONFIRMATIONS)
                .unwrap_or(HANG_RECOVER_MIN_CONFIRMATIONS)
                .max(HANG_RECOVER_MIN_CONFIRMATIONS),
            cooldown_secs: num(ENV_COOLDOWN, MARKER_COOLDOWN)
                .unwrap_or(HANG_RECOVER_MIN_COOLDOWN_SECS)
                .max(HANG_RECOVER_MIN_COOLDOWN_SECS),
            max_unhealed: num(ENV_MAX_UNHEALED, MARKER_MAX_UNHEALED)
                .unwrap_or(HANG_RECOVER_DEFAULT_MAX_UNHEALED)
                .max(1),
        }
    }

    /// Production resolution: the watchdog's own environment, then the marker.
    #[must_use]
    pub fn from_env_and_marker(marker: &Path) -> Self {
        Self::resolve(env::var, |k| super::marker::get_nonempty(marker, k))
    }
}

/// What this tick's heartbeat says about the wedge question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeartbeatSignal {
    /// Positive evidence: the heartbeat is older than its threshold AND was
    /// provably written by the CURRENT process (it is no older than the
    /// process itself).
    StaleCurrentBoot { age_secs: u64, threshold_secs: u64 },
    /// Anything else — carried as the reason it does not count.
    NotEvidence(String),
}

/// Classify, reusing `daemon_install_state`'s freshness verdict.
///
/// Stricter than that verdict in one place on purpose: `check_heartbeat`
/// degrades to `Stale` when the process age is unreadable, because for a
/// REPORT that is the useful answer. For a RESTART it is not — without the
/// process age nothing proves the stale file belongs to this boot.
#[must_use]
pub fn classify_heartbeat(
    freshness: Option<HeartbeatFreshness>,
    age_secs: Option<u64>,
    threshold_secs: Option<u64>,
    process_age_secs: Option<u64>,
) -> HeartbeatSignal {
    let not = |s: &str| HeartbeatSignal::NotEvidence(s.to_string());
    match freshness {
        Some(HeartbeatFreshness::Stale) => match (age_secs, threshold_secs, process_age_secs) {
            (Some(age), Some(threshold), Some(proc_age)) if age > threshold && age <= proc_age => {
                HeartbeatSignal::StaleCurrentBoot {
                    age_secs: age,
                    threshold_secs: threshold,
                }
            }
            (_, _, None) => not(
                "the heartbeat looks stale but the process age is unreadable, so nothing proves \
                 it was written by the current boot",
            ),
            _ => not("the heartbeat's age does not place it inside the current boot"),
        },
        Some(HeartbeatFreshness::Fresh) => not("the heartbeat is FRESH"),
        Some(HeartbeatFreshness::PriorBoot) => {
            not("the heartbeat is from a PREVIOUS boot (no evidence about this process)")
        }
        Some(HeartbeatFreshness::Unknown) if age_secs.is_some() => {
            not("the heartbeat mtime is unreadable")
        }
        Some(HeartbeatFreshness::Unknown) | None => {
            not("there is no heartbeat file (heartbeat disabled or not yet written)")
        }
    }
}

/// Advance (or reset) the per-pid dual-signal streak and return it.
///
/// Same `<pid> <count>` file shape as the #4398 streak, so a restarted daemon
/// (new pid) reads as no history.
pub fn advance_streak(path: &Path, pid: u32, signal: &HeartbeatSignal) -> u64 {
    if matches!(signal, HeartbeatSignal::StaleCurrentBoot { .. }) {
        let n = super::probe_state::fail_streak(path, pid) + 1;
        super::probe_state::write_fail_streak(path, pid, n);
        n
    } else {
        reset_streak(path);
        0
    }
}

/// End the dual-signal streak: any tick that is not a CONFIRMED dual-signal
/// tick interrupts it.
pub fn reset_streak(path: &Path) {
    super::probe_state::clear_fail_streak(path);
}

/// The supervised restart command for this daemon, or why there is none.
///
/// "Known loaded service" is checked against the liveness evidence itself:
/// the supervisor must have reported THIS pid alive under THIS service name
/// (`launchd job <svc> alive (pid N)` / `systemd unit <unit> alive (pid N)`).
/// A launchd job found only under a fallback domain, or any mismatch, has no
/// proven target and stays report-only.
///
/// # Errors
/// Returns the report-only reason when no supervisor provably owns the process.
pub fn restart_argv(
    source: Source,
    service: Option<&str>,
    liveness_detail: &str,
    pid: u32,
) -> Result<Vec<String>, String> {
    let svc = service.filter(|s| !s.is_empty());
    let proven = |shape: String| liveness_detail.contains(&shape);
    match (source, svc) {
        (Source::Launchd, Some(svc))
            if svc.contains('/') && proven(format!("launchd job {svc} alive (pid {pid})")) =>
        {
            Ok(super::supervisor_cmd::launchd_hang_restart_argv(svc))
        }
        (Source::Systemd, Some(unit))
            if proven(format!("systemd unit {unit} alive (pid {pid})")) =>
        {
            Ok(super::supervisor_cmd::systemd_hang_restart_argv(unit))
        }
        (Source::PidFile, _) => {
            Err("no supervisor owns this daemon (pid-file liveness tier), and \
                                     hang recovery never falls back to a bare kill"
                .to_string())
        }
        _ => Err(format!(
            "the supervisor did not report pid {pid} alive under a known service name ({}), so \
             there is no proven job to restart",
            svc.unwrap_or("none")
        )),
    }
}

/// The durable record. Deliberately NOT keyed by pid.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    /// Unix seconds of the last restart ATTEMPT (written before it runs).
    pub last_attempt_at: u64,
    pub last_attempt_pid: u32,
    /// Attempts since the last healthy IPC tick.
    pub unhealed: u64,
    pub last_result: String,
}

/// Read the record. A file that exists but carries no readable timestamp is
/// aged from its mtime, never as "no prior attempt": a corrupt record must not
/// open the cooldown.
#[must_use]
pub fn read_state(path: &Path) -> State {
    let Ok(text) = std::fs::read_to_string(path) else {
        return State::default();
    };
    let field = |k: &str| {
        let prefix = format!("{k}=");
        text.lines()
            .find(|l| l.starts_with(&prefix))
            .map(|l| l[prefix.len()..].trim().to_string())
    };
    let last_attempt_at = field("last_attempt_at")
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| mtime_secs(path).unwrap_or(u64::MAX));
    State {
        last_attempt_at,
        last_attempt_pid: field("last_attempt_pid")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        unhealed: field("unhealed").and_then(|v| v.parse().ok()).unwrap_or(0),
        last_result: field("last_result").unwrap_or_default(),
    }
}

fn mtime_secs(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Write the record atomically (tmp + rename), so a reader never sees half.
pub fn write_state(path: &Path, s: &State) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let body = format!(
        "# loom-daemon watchdog hang-recovery record (#7855). Delete to re-arm.\n\
         last_attempt_at={}\nlast_attempt_pid={}\nunhealed={}\nlast_result={}\n",
        s.last_attempt_at,
        s.last_attempt_pid,
        s.unhealed,
        s.last_result.replace('\n', " ")
    );
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

fn lock_path(state_path: &Path) -> PathBuf {
    let mut p = state_path.as_os_str().to_owned();
    p.push(".lock");
    PathBuf::from(p)
}

/// Seconds of cooldown left, or `None` when a restart is allowed. A record
/// from the future (clock skew) counts as "just now".
#[must_use]
pub fn cooldown_remaining(s: &State, cooldown_secs: u64, now: u64) -> Option<u64> {
    if s.last_attempt_at == 0 {
        return None;
    }
    let elapsed = now.saturating_sub(s.last_attempt_at);
    (elapsed < cooldown_secs).then(|| cooldown_secs - elapsed)
}

/// The pure gate: may this CONFIRMED tick restart the daemon?
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    Disabled,
    NotQualifying { reason: String },
    Accumulating { streak: u64, needed: u64 },
    NoSupervisor { reason: String },
    BreakerOpen { unhealed: u64, max: u64 },
    CoolingDown { remaining: u64, last_at: u64 },
    Restart { argv: Vec<String> },
}

#[must_use]
pub fn gate(
    settings: &Settings,
    signal: &HeartbeatSignal,
    dual_streak: u64,
    argv: Result<Vec<String>, String>,
    state: &State,
    now: u64,
) -> Gate {
    if !settings.enabled {
        return Gate::Disabled;
    }
    if let HeartbeatSignal::NotEvidence(reason) = signal {
        return Gate::NotQualifying {
            reason: reason.clone(),
        };
    }
    if dual_streak < settings.confirmations {
        return Gate::Accumulating {
            streak: dual_streak,
            needed: settings.confirmations,
        };
    }
    let argv = match argv {
        Ok(a) => a,
        Err(reason) => return Gate::NoSupervisor { reason },
    };
    if state.unhealed >= settings.max_unhealed {
        return Gate::BreakerOpen {
            unhealed: state.unhealed,
            max: settings.max_unhealed,
        };
    }
    if let Some(remaining) = cooldown_remaining(state, settings.cooldown_secs, now) {
        return Gate::CoolingDown {
            remaining,
            last_at: state.last_attempt_at,
        };
    }
    Gate::Restart { argv }
}

/// What [`attempt`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attempted {
    /// The supervisor command ran. `rc` is `None` when it could not be run or
    /// exceeded its bound.
    Ran { rc: Option<i32>, unhealed: u64 },
    /// Another invocation holds the lock; it is the one acting.
    Busy,
    /// The lock could not be created — fail closed.
    LockUnavailable(String),
    /// Re-read under the lock: someone else acted first.
    Refused(Gate),
    /// A deliberate-stop / opt-out guard tripped between decision and action.
    GuardTripped(String),
    /// The durable record could not be written, so the restart was NOT run:
    /// an attempt the cooldown cannot see is an unbounded one.
    StateUnwritable(String),
}

/// Perform a [`Gate::Restart`]: under the lock, re-check the record, re-check
/// the guards, persist the attempt FIRST, then run the supervisor command.
///
/// `guard` and `run` are injected so tests never touch a real supervisor.
pub fn attempt(
    state_path: &Path,
    settings: &Settings,
    argv: &[String],
    pid: u32,
    now: u64,
    guard: impl FnOnce() -> Result<(), String>,
    run: impl FnOnce(&[String]) -> Option<i32>,
) -> Attempted {
    if let Some(dir) = state_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let lock = lock_path(state_path);
    let _held = match crate::tokens_pool::locking::MkdirLock::try_acquire(&lock, LOCK_STALE) {
        Ok(Some(l)) => l,
        Ok(None) => return Attempted::Busy,
        Err(e) => return Attempted::LockUnavailable(e),
    };

    let mut s = read_state(state_path);
    if s.unhealed >= settings.max_unhealed {
        return Attempted::Refused(Gate::BreakerOpen {
            unhealed: s.unhealed,
            max: settings.max_unhealed,
        });
    }
    if let Some(remaining) = cooldown_remaining(&s, settings.cooldown_secs, now) {
        return Attempted::Refused(Gate::CoolingDown {
            remaining,
            last_at: s.last_attempt_at,
        });
    }
    if let Err(reason) = guard() {
        return Attempted::GuardTripped(reason);
    }

    s.last_attempt_at = now;
    s.last_attempt_pid = pid;
    s.unhealed += 1;
    s.last_result = format!("in progress: {}", argv.join(" "));
    if let Err(e) = write_state(state_path, &s) {
        return Attempted::StateUnwritable(e.to_string());
    }

    let rc = run(argv);
    s.last_result = match rc {
        Some(0) => format!("'{}' exited 0", argv.join(" ")),
        Some(c) => format!("'{}' exited {c}", argv.join(" ")),
        None => format!("'{}' could not run or timed out", argv.join(" ")),
    };
    let _ = write_state(state_path, &s);
    Attempted::Ran {
        rc,
        unhealed: s.unhealed,
    }
}

/// A healthy IPC tick re-arms the breaker. The cooldown timestamp is kept: it
/// bounds restarts per window regardless of what happened in between.
///
/// Best-effort and no-op on a host that has never attempted a restart.
pub fn on_healthy(state_path: &Path) {
    if !state_path.exists() || read_state(state_path).unhealed == 0 {
        return;
    }
    if let Ok(Some(_held)) =
        crate::tokens_pool::locking::MkdirLock::try_acquire(&lock_path(state_path), LOCK_STALE)
    {
        let mut s = read_state(state_path);
        s.unhealed = 0;
        let _ = write_state(state_path, &s);
    }
}

/// The deliberate-stop guards, re-asked under the lock immediately before a
/// restart: a stop (marker gone) or a drain / operator stop (#9588) recorded
/// since the tick began must win over a restart decided earlier.
///
/// # Errors
/// Returns the reason a restart must not run.
pub fn intent_guard(marker: &Path) -> Result<(), String> {
    if !marker.exists() {
        return Err("the autonomy-desired marker is gone (a deliberate stop)".to_string());
    }
    if crate::operator_stop::is_recorded(marker) {
        return Err("an operator stop / drain is on record (#9588)".to_string());
    }
    Ok(())
}

/// Everything the evidence line needs, gathered by the caller.
pub struct Evidence<'a> {
    pub pid: u32,
    pub ipc_detail: &'a str,
    pub ipc_streak: u64,
    pub ipc_threshold: u64,
    pub load: &'a str,
}

/// One CONFIRMED tick's hang-recovery outcome, rendered for the caller.
pub struct Outcome {
    /// Replaces the CONFIRMED line's remediation sentence.
    pub note: String,
    /// A separate evidence line when a restart actually ran.
    pub restart_line: Option<String>,
}

/// Run the whole CONFIRMED-branch decision: streak, gate, and (maybe) restart.
#[allow(clippy::too_many_arguments)]
pub fn on_confirmed(
    settings: &Settings,
    streak_path: &Path,
    state_path: &Path,
    signal: &HeartbeatSignal,
    argv: Result<Vec<String>, String>,
    ev: &Evidence,
    now: u64,
    guard: impl FnOnce() -> Result<(), String>,
    run: impl FnOnce(&[String]) -> Option<i32>,
) -> Outcome {
    if !settings.enabled {
        return Outcome {
            note: note_for(&Gate::Disabled, settings, state_path, now),
            restart_line: None,
        };
    }
    let streak = advance_streak(streak_path, ev.pid, signal);
    let state = read_state(state_path);
    let g = gate(settings, signal, streak, argv, &state, now);
    let Gate::Restart { argv } = &g else {
        return Outcome {
            note: note_for(&g, settings, state_path, now),
            restart_line: None,
        };
    };
    let lock = lock_path(state_path);
    let note = |s: &str| Outcome {
        note: format!("No automatic kill/restart is attempted by this invocation: {s} (#7855)."),
        restart_line: None,
    };
    match attempt(state_path, settings, argv, ev.pid, now, guard, run) {
        Attempted::Busy => note(&format!(
            "another watchdog invocation holds the hang-recovery lock {}",
            lock.display()
        )),
        Attempted::LockUnavailable(e) => note(&format!(
            "the hang-recovery lock {} could not be created ({e}), so recovery fails closed",
            lock.display()
        )),
        Attempted::StateUnwritable(e) => note(&format!(
            "the durable hang-recovery record {} could not be written ({e}), and a restart the \
             cooldown cannot see would be unbounded",
            state_path.display()
        )),
        Attempted::GuardTripped(reason) => note(&reason),
        Attempted::Refused(g) => Outcome {
            note: note_for(&g, settings, state_path, now),
            restart_line: None,
        },
        Attempted::Ran { rc, unhealed } => {
            reset_streak(streak_path);
            let (age, threshold) = match signal {
                HeartbeatSignal::StaleCurrentBoot {
                    age_secs,
                    threshold_secs,
                } => (*age_secs, *threshold_secs),
                HeartbeatSignal::NotEvidence(_) => (0, 0),
            };
            let result = match rc {
                Some(0) => "exited 0".to_string(),
                Some(c) => format!("FAILED (exit {c})"),
                None => "FAILED (could not run, or exceeded its bound)".to_string(),
            };
            Outcome {
                note: "AUTO-RECOVERY ATTEMPTED through the supervisor (see the HANG RECOVERY line \
                       that follows) — no bare kill, nothing sent over the wedged socket (#7855)."
                    .to_string(),
                restart_line: Some(format!(
                    "HANG RECOVERY (#7855, opt-in via {source}): issued supervised restart '{cmd}' \
                     for wedged pid {pid}; supervisor command {result}. Evidence: IPC — {detail} \
                     ({ipc} consecutive failed round-trips, threshold {ipc_t}); heartbeat — \
                     current-boot STALE, {age}s old > {threshold}s threshold, for {streak} \
                     consecutive CONFIRMED ticks (required {n}); host load average {load}. Bounds: next \
                     automatic restart no sooner than {cool}s from now; {unhealed} of {max} \
                     restarts used without an intervening healthy tick (record {state}).",
                    source = settings.source,
                    cmd = argv.join(" "),
                    pid = ev.pid,
                    detail = ev.ipc_detail,
                    ipc = ev.ipc_streak,
                    ipc_t = ev.ipc_threshold,
                    n = settings.confirmations,
                    load = ev.load,
                    cool = settings.cooldown_secs,
                    max = settings.max_unhealed,
                    state = state_path.display(),
                )),
            }
        }
    }
}

/// The CONFIRMED line's remediation sentence for a gate that did not restart.
///
/// Every variant keeps the literal "No automatic kill/restart" — the retained
/// suite and operators' log-scrapers key on it.
#[must_use]
pub fn note_for(g: &Gate, settings: &Settings, state_path: &Path, now: u64) -> String {
    let body = match g {
        Gate::Disabled => "opt-in supervised hang recovery is OFF on this host \
                           (LOOM_WATCHDOG_HANG_RECOVER / marker watchdog_hang_recover, default \
                           off), so a wedged-but-alive daemon is REPORT-ONLY here — deliberately, \
                           because a failed round-trip is also what a daemon under heavy \
                           legitimate load looks like (#4398)"
            .to_string(),
        Gate::NotQualifying { reason } => format!(
            "hang recovery is enabled but requires a positively STALE current-boot heartbeat \
             alongside the failed IPC, and {reason} — the dual-signal streak is reset"
        ),
        Gate::Accumulating { streak, needed } => format!(
            "hang recovery is enabled; this is dual-signal CONFIRMED tick {streak} of {needed} \
             (failed IPC AND a current-boot stale heartbeat), and it acts only once {needed} \
             consecutive ticks agree"
        ),
        Gate::NoSupervisor { reason } => {
            format!("hang recovery uses ONLY a supervised restart, and {reason}")
        }
        Gate::BreakerOpen { unhealed, max } => format!(
            "the hang-recovery breaker is OPEN — {unhealed} supervised restarts (max {max}) \
             without an intervening healthy IPC tick; it re-arms when a tick round-trips \
             cleanly or {} is deleted",
            state_path.display()
        ),
        Gate::CoolingDown { remaining, last_at } => format!(
            "a supervised hang restart already ran {}s ago and at most one is issued per {}s \
             ({remaining}s remaining; record {})",
            now.saturating_sub(*last_at),
            settings.cooldown_secs,
            state_path.display()
        ),
        Gate::Restart { .. } => "a restart is pending".to_string(),
    };
    format!("No automatic kill/restart is attempted: {body} (#7855).")
}

#[cfg(test)]
#[path = "hang_recover_tests.rs"]
mod tests;
