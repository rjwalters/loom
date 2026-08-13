//! Post-restart relaunch verification + supervisor self-heal for the **bare**
//! `loom-daemon restart` primitive (Issue #5119).
//!
//! # Why this exists
//!
//! `loom-daemon restart` (#4054) is fire-and-forget by construction: the
//! running daemon acks the IPC request and then exits [`EXIT_RESTART`] (0),
//! trusting its supervisor's "relaunch on a successful exit" policy
//! (launchd `KeepAlive:{SuccessfulExit:true}` / systemd `Restart=on-success`)
//! to bring it back. **That ack is the daemon's promise, not proof the
//! supervisor honored it**, and both supervisors have a documented way to
//! break the promise:
//!
//! - **launchd** can simply fail to relaunch the job (#4232 — observed
//!   2026-07-28; the host sat daemonless for ~4 minutes until an operator ran
//!   `launchctl kickstart` by hand).
//! - **systemd** processes the exit by running the unit's *stop job over the
//!   whole cgroup*. If leftover cgroup members (sweep / role-run children)
//!   outlive `TimeoutStopSec`, systemd SIGKILLs them and marks the unit
//!   `failed (Result: timeout)` — and `Restart=on-success` does **not** match
//!   `Result=timeout`, so the relaunch never fires (#4950 — observed
//!   2026-08-02 and again 2026-08-03 on loom-worker-1, this issue's incident).
//!   systemd also refuses to auto-restart a unit that is *already* `failed`,
//!   so the host stays down until `systemctl --user reset-failed && start`.
//!
//! Both self-heals already existed — but **only inside
//! `loom-daemon-update.sh`** (`wait_for_new_launchd_pid` / `launchctl
//! kickstart` for #4232, `wait_for_new_systemd_pid` / `reset-failed && start`
//! for #4950). Anything that invokes the primitive directly — an operator, the
//! `loom restart` wrapper, a fleet script — got no verification and no
//! recovery at all. This module is that logic, moved to where the primitive
//! itself lives so every caller inherits it.
//!
//! # Residual: an in-cgroup caller cannot self-heal
//!
//! This verification runs in the **invoking** process. A caller that lives
//! inside the daemon's own systemd cgroup (the daemon's autonomous self-update
//! loop, most notably) is signalled by the very stop job it is trying to
//! survive, so it may die before it can poll or recover. That limit is not new
//! — `loom-daemon-update.sh`'s self-heal has it for the same reason — and the
//! out-of-band backstop is `loom-daemon-watchdog.sh`, which runs from its own
//! systemd timer/cgroup and applies the same `reset-failed && start`. An
//! operator-invoked restart (an SSH shell, a fleet script) is outside the
//! cgroup and self-heals in-band.
//!
//! # Deliberately mirrors the shell implementation
//!
//! The poll contract ("a NEW pid that is also *alive*"), the env knobs
//! (`LOOM_DAEMON_RESTART_POLL_SECS`, `LOOM_DAEMON_RESTART_POLL_INTERVAL`,
//! `LOOM_DAEMON_RESTART_KICKSTART_POLL_SECS`), the plain-never-`-k` kickstart,
//! and the `ActiveState == failed`-gated systemd recovery are all lifted 1:1
//! from `loom-daemon-update.sh` so the two paths cannot drift into different
//! answers on the same host.
//!
//! # A negative launchd probe is not proof of anything (#6101)
//!
//! [`resolve_launchd_service_detailed`] already probes `gui/<uid>` before
//! falling back to `user/<uid>` (#4130) — but a single `gui/<uid>`
//! reachability probe cannot distinguish "genuinely no GUI/Aqua session" from
//! "a transient flake, or a GUI session still coming up" (e.g. mid
//! unattended-crash recovery). [`supervised_pid`] therefore cross-checks the
//! skipped `gui/<uid>` domain before treating a `user/<uid>` miss as "no
//! pid", mirroring the identical #4694 fix already shipped for the
//! watchdog-provisioning probe in `daemon_install_state.rs`. [`log_diagnostics`]
//! also dumps that skipped domain and annotates every failed probe so a
//! healthy relaunch queried in the wrong-for-that-instant domain no longer
//! renders as "THIS HOST MAY BE DAEMONLESS" without qualification.
//!
//! [`EXIT_RESTART`]: crate::ipc::EXIT_RESTART

use std::process::Command;
use std::time::{Duration, Instant};

// ============================================================================
// Knobs
// ============================================================================

/// Opt out of post-restart verification entirely. Accepts `0` / `false` / `no`
/// (case-insensitive). Exists so a caller that runs its *own* verification —
/// `loom-daemon-update.sh` does, and reports far richer provisioning context —
/// can skip a redundant second poll, and so a test harness can keep the
/// primitive purely synchronous.
pub const VERIFY_ENV: &str = "LOOM_DAEMON_RESTART_VERIFY";

/// Seconds to wait for the supervisor's OWN relaunch before self-healing.
/// Same name and default (30) as `loom-daemon-update.sh`.
pub const POLL_SECS_ENV: &str = "LOOM_DAEMON_RESTART_POLL_SECS";
/// Default for [`POLL_SECS_ENV`].
pub const DEFAULT_POLL_SECS: u64 = 30;

/// Seconds to wait for the *self-heal* to take effect, after the supervisor's
/// own relaunch did not. Same name and default (15) as the update script.
pub const RECOVERY_POLL_SECS_ENV: &str = "LOOM_DAEMON_RESTART_KICKSTART_POLL_SECS";
/// Default for [`RECOVERY_POLL_SECS_ENV`].
pub const DEFAULT_RECOVERY_POLL_SECS: u64 = 15;

/// Seconds between polls. Same name and default (1) as the update script.
pub const POLL_INTERVAL_SECS_ENV: &str = "LOOM_DAEMON_RESTART_POLL_INTERVAL";
/// Default for [`POLL_INTERVAL_SECS_ENV`].
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 1000;

/// Wall-clock bound on every query-only subprocess probe here (`systemctl
/// show`, `launchctl print`, `kill -0`, `id -u`). Mirrors
/// `daemon_install_state`'s #4548 rationale: a wedged `systemd --user` bus can
/// make `systemctl` block on its D-Bus connect, and an unbounded probe would
/// wedge the very command an operator runs to recover.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

// ============================================================================
// Supervisor
// ============================================================================

/// The supervisor the daemon reported in its `DaemonRestart` ack.
///
/// Parsed from the reply rather than re-detected locally: the daemon's own
/// `LOOM_DAEMON_SUPERVISOR` is the authority on how it is supervised, and the
/// CLI process (an operator's shell) frequently does not have that var at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Supervisor {
    /// macOS `launchd` LaunchAgent (`KeepAlive:{SuccessfulExit:true}`).
    Launchd,
    /// Linux `systemd --user` unit (`Restart=on-success`).
    Systemd,
}

impl Supervisor {
    /// Parse the ack's `supervisor` field. Unknown/absent ⇒ `None`, which the
    /// caller treats as "nothing to verify against" (never as a failure).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        if raw.eq_ignore_ascii_case("launchd") {
            Some(Supervisor::Launchd)
        } else if raw.eq_ignore_ascii_case("systemd") {
            Some(Supervisor::Systemd)
        } else {
            None
        }
    }

    /// Display name, matching `detect_supervisor`'s canonical spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Supervisor::Launchd => "launchd",
            Supervisor::Systemd => "systemd",
        }
    }
}

// ============================================================================
// Pure decision logic
// ============================================================================

/// Is verification enabled for this invocation? Default **on**; only an
/// explicit `0` / `false` / `no` (case-insensitive) in [`VERIFY_ENV`] turns it
/// off, so a host that never sets the var keeps the safe behavior.
#[must_use]
pub fn verification_enabled(raw: Option<&str>) -> bool {
    match raw {
        Some(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no"),
        None => true,
    }
}

/// Is `cur_pid` proof of a relaunch relative to `pre_pid`?
///
/// **All three degenerate readings must be rejected**, exactly as
/// `wait_for_new_systemd_pid` rejects them in shell:
/// - no pid at all (systemd reports `MainPID=0` for a stopped unit),
/// - a pid equal to `pre_pid` (the old process still lingering mid-teardown
///   during the poll window),
/// - a pid that differs but is not alive (a race artifact / already-reaped).
///
/// A `pre_pid` of `None` (the pre-restart query itself failed) still accepts
/// any live pid — that is strictly better than refusing to ever confirm.
#[must_use]
pub fn is_confirmed_relaunch(pre_pid: Option<u32>, cur_pid: Option<u32>, alive: bool) -> bool {
    match cur_pid {
        None => false,
        Some(0) => false,
        Some(pid) => alive && Some(pid) != pre_pid,
    }
}

/// What to do when systemd did not relaunch on its own, keyed off the unit's
/// `ActiveState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemdRecovery {
    /// `ActiveState=failed` — the exact incident shape. systemd will NEVER
    /// auto-relaunch a unit already in `failed`, even under
    /// `Restart=on-success`, so clear the failure and start it.
    ResetFailedAndStart,
    /// Any other state — refuse to guess. `activating`/`deactivating` means the
    /// transition is still in progress (a `start` would race it), and `active`
    /// with an unchanged MainPID means something stranger is going on than this
    /// recovery models.
    RefuseUnknownState,
}

/// Decide the systemd recovery action from the unit's `ActiveState` (#4950's
/// gate, lifted verbatim: recover ONLY on a confirmed `failed`).
#[must_use]
pub fn systemd_recovery_for(active_state: &str) -> SystemdRecovery {
    if active_state.trim().eq_ignore_ascii_case("failed") {
        SystemdRecovery::ResetFailedAndStart
    } else {
        SystemdRecovery::RefuseUnknownState
    }
}

/// Advisory warning about a LIVE systemd unit whose `KillMode` predates
/// #4862's `KillMode=mixed` fix (Issue #5119).
///
/// This is the root cause of the 2026-08-03 outage: a plain `restart` never
/// re-renders the unit file, so a host provisioned before #4862 keeps running
/// the default `KillMode=control-group` forever. Under that mode the stop job
/// SIGTERMs the whole cgroup and waits the FULL `TimeoutStopSec` before
/// SIGKILLing it — which both destroys in-flight sweeps/role runs *and* lands
/// the unit in `failed (Result: timeout)`, where `Restart=on-success` can never
/// fire.
///
/// Returns `None` for the healthy `mixed` unit and for any unreadable/absent
/// property (advisory only — it must never block or fail a restart).
#[must_use]
pub fn stale_unit_warning(kill_mode: Option<&str>, timeout_stop: Option<&str>) -> Option<String> {
    let mode = kill_mode?.trim();
    if !mode.eq_ignore_ascii_case("control-group") {
        return None;
    }
    let timeout = timeout_stop
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("the unit's TimeoutStopSec");
    Some(format!(
        "WARNING: this systemd unit still has KillMode=control-group — it predates the \
         KillMode=mixed fix (#4862) and a plain `restart` never re-renders the unit file.\n\
         \x20 On exit, systemd SIGTERMs the ENTIRE cgroup (every sweep / role-run child) and \
         waits {timeout} before SIGKILLing it, then marks the unit `failed (Result: timeout)`.\n\
         \x20 `Restart=on-success` does NOT match Result=timeout, so the relaunch will not \
         fire on its own — this command will self-heal it, but re-render the unit to fix it \
         for good:\n\
         \x20   loom-daemon-update.sh --relaunch    (or: ./.loom/scripts/cli/loom-daemon-start.sh)"
    ))
}

/// The terminal verdict of [`verify_and_heal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelaunchOutcome {
    /// Verification was disabled ([`VERIFY_ENV`]) or the supervisor was not one
    /// this module knows how to probe. Never a failure.
    Skipped { reason: String },
    /// A new, live pid was observed. `healed` distinguishes "the supervisor
    /// relaunched it on its own" from "it did not, and the self-heal did".
    Relaunched { pid: u32, healed: bool },
    /// No new, live pid — the host may be daemonless. `detail` carries the
    /// operator-facing evidence.
    NotRelaunched { detail: String },
}

impl RelaunchOutcome {
    /// `true` when the daemon is confirmed back up (or verification was
    /// skipped, which asserts nothing either way).
    #[must_use]
    pub fn is_ok(&self) -> bool {
        !matches!(self, RelaunchOutcome::NotRelaunched { .. })
    }

    /// The operator-facing line to print for this outcome.
    #[must_use]
    pub fn render(&self, supervisor: &str, poll_secs: u64) -> String {
        match self {
            RelaunchOutcome::Skipped { reason } => {
                format!("relaunch verification skipped: {reason}")
            }
            RelaunchOutcome::Relaunched { pid, healed: false } => format!(
                "relaunch confirmed: {supervisor} brought the daemon back on its own \
                 (new pid {pid}, within {poll_secs}s)."
            ),
            RelaunchOutcome::Relaunched { pid, healed: true } => format!(
                "relaunch RECOVERED: {supervisor} did not relaunch within {poll_secs}s, but the \
                 self-heal did (new pid {pid}). This should not have been necessary — see the \
                 diagnostics above for why the supervisor refused."
            ),
            RelaunchOutcome::NotRelaunched { detail } => format!(
                "RELAUNCH NOT CONFIRMED: no new, live daemon pid was observed within \
                 {poll_secs}s and the self-heal did not recover it. THIS HOST MAY BE \
                 DAEMONLESS. {detail}"
            ),
        }
    }
}

/// Poll `probe` (which must return the supervisor's current pid, or `None`)
/// until it reports a confirmed relaunch relative to `pre_pid`, or `timeout`
/// elapses.
///
/// Always probes **at least once**, so a zero timeout still gets one honest
/// look rather than short-circuiting to failure. Generic over the probe so the
/// entire poll contract is unit-testable without a supervisor: real callers
/// pass a closure that shells out and folds `kill -0` liveness into its answer.
pub fn poll_until_relaunched<P>(
    pre_pid: Option<u32>,
    timeout: Duration,
    interval: Duration,
    mut probe: P,
) -> Option<u32>
where
    P: FnMut() -> (Option<u32>, bool),
{
    let deadline = Instant::now() + timeout;
    loop {
        let (cur, alive) = probe();
        if is_confirmed_relaunch(pre_pid, cur, alive) {
            return cur;
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(interval);
    }
}

// ============================================================================
// Environment resolution
// ============================================================================

/// Read a seconds knob, falling back to `default` on absent/unparsable input.
#[must_use]
pub fn resolve_secs(raw: Option<&str>, default: u64) -> u64 {
    raw.and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

/// Read the poll interval, which the shell implementation allows to be
/// fractional (e.g. `0.2`). Falls back to [`DEFAULT_POLL_INTERVAL_MS`] on
/// absent/unparsable/non-positive input.
#[must_use]
pub fn resolve_interval(raw: Option<&str>) -> Duration {
    let millis = raw
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|v| (v * 1000.0) as u64)
        .filter(|ms| *ms > 0)
        .unwrap_or(DEFAULT_POLL_INTERVAL_MS);
    Duration::from_millis(millis)
}

/// The systemd `--user` unit name, mirroring `resolve_systemd_unit()` in
/// `defaults/scripts/lib/systemd-user.sh`.
#[must_use]
pub fn resolve_systemd_unit() -> String {
    std::env::var("LOOM_SYSTEMD_UNIT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "loom-daemon.service".to_string())
}

/// [`resolve_launchd_service_detailed`]'s result: the primary `<domain>/<label>`
/// to probe, plus — only when domain resolution itself fell back to
/// `user/<uid>` because the `gui/<uid>` reachability probe did not succeed —
/// the skipped `gui/<uid>/<label>` target worth a cross-check before a
/// negative primary-domain probe is trusted as "the daemon is not running"
/// rather than merely "not found in the domain we happened to check".
///
/// This mirrors `DomainResolution` / `resolve_launchd_domain_detailed()` in
/// `daemon_install_state.rs` (#4694), which solved the identical ambiguity for
/// the watchdog-provisioning probe: a single `launchctl print gui/<uid>`
/// reachability check cannot distinguish "genuinely no GUI/Aqua session" from
/// "a transient flake, or a GUI session still coming up" (e.g. during
/// unattended-crash recovery, Issue #6101) — so a caller that needs to trust a
/// negative verdict cross-checks the skipped domain rather than believing the
/// first probe's fallback unconditionally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchdServiceResolution {
    /// `<domain>/<label>` to probe first.
    pub service: String,
    /// The skipped `gui/<uid>/<label>` target. `Some` only when `service`
    /// fell back to `user/<uid>` because the `gui/<uid>` reachability probe
    /// did not succeed — never when an explicit `LOOM_LAUNCHD_DOMAIN`
    /// override was honored (an explicit override is honored verbatim, no
    /// cross-check), and never when `gui/<uid>` was itself the resolved
    /// primary (nothing was skipped).
    pub fallback_check_service: Option<String>,
}

/// The launchd service target (`<domain>/<label>`), mirroring
/// `resolve_launchd_domain()` + `resolve_launchd_label()` in the shell libs:
/// `LOOM_LAUNCHD_DOMAIN` wins, else `gui/<uid>` when that domain resolves, else
/// `user/<uid>` (the background per-user domain sshd instantiates, #4130). See
/// [`LaunchdServiceResolution`] for the accompanying cross-check target that
/// callers making a pass/fail decision ([`supervised_pid`]) should also probe.
#[must_use]
pub fn resolve_launchd_service_detailed() -> LaunchdServiceResolution {
    let label = std::env::var("LOOM_LAUNCHD_LABEL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| crate::daemon_install_state::DEFAULT_LAUNCHD_LABEL.to_string());
    if let Some(domain) = std::env::var("LOOM_LAUNCHD_DOMAIN")
        .ok()
        .filter(|s| !s.trim().is_empty())
    {
        return LaunchdServiceResolution {
            service: format!("{domain}/{label}"),
            fallback_check_service: None,
        };
    }
    let uid = current_uid().unwrap_or_default();
    let gui = format!("gui/{uid}");
    let mut cmd = Command::new("launchctl");
    cmd.args(["print", &gui]);
    if probe(cmd).is_some_and(|o| o.status.success()) {
        LaunchdServiceResolution {
            service: format!("{gui}/{label}"),
            fallback_check_service: None,
        }
    } else {
        LaunchdServiceResolution {
            service: format!("user/{uid}/{label}"),
            fallback_check_service: Some(format!("{gui}/{label}")),
        }
    }
}

/// Convenience wrapper over [`resolve_launchd_service_detailed`] for callers
/// that only need the primary target (e.g. `heal_launchd`'s `kickstart`,
/// which is a no-op against a job that is not there, so probing the primary
/// domain alone is sufficient — see its doc comment).
#[must_use]
pub fn resolve_launchd_service() -> String {
    resolve_launchd_service_detailed().service
}

// ============================================================================
// Subprocess probes (thin — every decision above is pure)
// ============================================================================

/// Run a query-only probe, collapsing every failure mode (absent binary, spawn
/// failure, hang past [`PROBE_TIMEOUT`]) to `None`.
fn probe(mut cmd: Command) -> Option<std::process::Output> {
    cmd.stdin(std::process::Stdio::null());
    crate::sweep_registry::output_with_timeout(cmd, PROBE_TIMEOUT)
        .ok()
        .flatten()
}

/// Current uid via `id -u` (same mechanism `daemon_install_state` uses, so no
/// new dependency is introduced for one integer).
fn current_uid() -> Option<String> {
    let mut cmd = Command::new("id");
    cmd.arg("-u");
    let out = probe(cmd)?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// `kill -0 <pid>` — is the process alive (and signalable by this user)?
fn pid_alive(pid: u32) -> bool {
    let mut cmd = Command::new("kill");
    cmd.args(["-0", &pid.to_string()]);
    probe(cmd).is_some_and(|o| o.status.success())
}

/// One `systemctl --user show -p <property> --value <unit>` read.
#[must_use]
pub fn systemctl_show(unit: &str, property: &str) -> Option<String> {
    let mut cmd = Command::new("systemctl");
    cmd.args(["--user", "show", "-p", property, "--value", unit]);
    let out = probe(cmd)?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Parse a `pid = N` line out of `launchctl print <service>` output — the same
/// field `launchd_job_pid()` awks for in `loom-daemon-update.sh`.
#[must_use]
pub fn parse_launchctl_pid(output: &str) -> Option<u32> {
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("pid = ") {
            let digits: String = rest.chars().filter(char::is_ascii_digit).collect();
            if let Ok(pid) = digits.parse::<u32>() {
                return Some(pid);
            }
        }
    }
    None
}

/// One `launchctl print <service>` probe, folded straight down to a pid (or
/// `None` on any failure — absent job, absent binary, timeout).
fn launchctl_print_pid(service: &str) -> Option<u32> {
    let mut cmd = Command::new("launchctl");
    cmd.args(["print", service]);
    probe(cmd)
        .filter(|o| o.status.success())
        .and_then(|o| parse_launchctl_pid(&String::from_utf8_lossy(&o.stdout)))
}

/// The supervisor's current view of the daemon's pid, and whether it is alive.
/// `(None, false)` means "no usable answer" — never "confirmed down".
///
/// For launchd, a `None` from the primary resolved domain is cross-checked
/// against [`LaunchdServiceResolution::fallback_check_service`] (Issue #6101,
/// mirroring the #4694 cross-check in `daemon_install_state.rs`) before this
/// function reports "no pid": a single `gui/<uid>` reachability probe cannot
/// tell "genuinely no GUI session" apart from "a transient flake / a GUI
/// session still coming up", so a negative primary probe alone must not be
/// trusted as proof the daemon is down.
#[must_use]
pub fn supervised_pid(supervisor: Supervisor) -> (Option<u32>, bool) {
    let pid = match supervisor {
        Supervisor::Systemd => systemctl_show(&resolve_systemd_unit(), "MainPID")
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|p| *p != 0),
        Supervisor::Launchd => {
            let resolution = resolve_launchd_service_detailed();
            launchctl_print_pid(&resolution.service).or_else(|| {
                resolution
                    .fallback_check_service
                    .as_deref()
                    .and_then(launchctl_print_pid)
            })
        }
    };
    match pid {
        Some(p) => (Some(p), pid_alive(p)),
        None => (None, false),
    }
}

/// Dump one `<bin> <args>` probe's output as a labeled diagnostic breadcrumb.
/// When the probe failed (nonzero exit or no output at all), append a note
/// that this means "not found via THIS probe" — not, by itself, "confirmed
/// not running" (Issue #6101: a healthy relaunch was previously reported as
/// `THIS HOST MAY BE DAEMONLESS` purely because the wrong-for-that-moment
/// domain/unit was queried).
fn log_one_diagnostic(bin: &str, args: &[String], label: &str) {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    eprintln!("  {label} diagnostic snapshot:");
    match probe(cmd) {
        Some(out) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            for line in text.lines() {
                eprintln!("    {line}");
            }
            if !out.status.success() {
                eprintln!(
                    "    NOTE: this probe failed (service/unit not found here) — that means \
                     the daemon was not found via THIS specific probe, which can happen for \
                     reasons other than \"the daemon is not running\" (e.g. the wrong domain/unit \
                     was queried, or — for launchd — the GUI session was not yet resolvable at \
                     this instant). It is not by itself proof the daemon is down."
                );
            }
        }
        None => eprintln!(
            "    (no output — the probe failed or timed out; this does not by itself confirm \
             the daemon is down)"
        ),
    }
}

/// Dump the supervisor's own status output as a diagnostic breadcrumb, mirroring
/// `log_systemd_diagnostics` / `log_launchd_diagnostics`.
fn log_diagnostics(supervisor: Supervisor) {
    match supervisor {
        Supervisor::Systemd => {
            let unit = resolve_systemd_unit();
            log_one_diagnostic(
                "systemctl",
                &[
                    "--user".to_string(),
                    "status".to_string(),
                    unit.clone(),
                    "--no-pager".to_string(),
                    "--full".to_string(),
                ],
                &format!("systemctl --user status {unit}"),
            );
        }
        Supervisor::Launchd => {
            let resolution = resolve_launchd_service_detailed();
            log_one_diagnostic(
                "launchctl",
                &["print".to_string(), resolution.service.clone()],
                &format!("launchctl print {}", resolution.service),
            );
            if let Some(check) = resolution.fallback_check_service.as_deref() {
                eprintln!(
                    "  Domain resolution fell back to '{}' because the gui/<uid> reachability \
                     probe did not succeed at that moment. Cross-checking the skipped gui/<uid> \
                     domain (a real GUI/Aqua session — e.g. one still coming up during \
                     unattended-crash recovery — would show up here, not above):",
                    resolution.service
                );
                log_one_diagnostic(
                    "launchctl",
                    &["print".to_string(), check.to_string()],
                    &format!("launchctl print {check}"),
                );
            }
        }
    }
}

// ============================================================================
// Orchestration
// ============================================================================

/// Best-effort guess at which supervisor **this host** runs the daemon under,
/// for the one job that must happen *before* the ack arrives: snapshotting the
/// pre-restart pid.
///
/// It cannot use [`crate::ipc::detect_supervisor`], because that reads
/// `LOOM_DAEMON_SUPERVISOR` — a var baked into the *daemon's* plist/unit, which
/// the CLI process (an operator's shell) almost never has. So: honor the var
/// when it IS present, else fall back to the platform's only supervisor.
///
/// A wrong guess is harmless by construction: the pid probe simply yields
/// `None` (nothing to compare against), and every decision after the ack uses
/// the supervisor the **daemon itself** reported.
#[must_use]
pub fn probe_host_supervisor() -> Option<Supervisor> {
    if let Some(sup) = std::env::var("LOOM_DAEMON_SUPERVISOR")
        .ok()
        .as_deref()
        .and_then(Supervisor::parse)
    {
        return Some(sup);
    }
    if cfg!(target_os = "macos") {
        Some(Supervisor::Launchd)
    } else if cfg!(target_os = "linux") {
        Some(Supervisor::Systemd)
    } else {
        None
    }
}

/// Best-effort pre-restart pid snapshot, captured by the CLI **before** the
/// `RestartDaemon` request so the poll can tell "relaunched onto a new pid"
/// apart from "never moved".
#[must_use]
pub fn pre_restart_pid(supervisor: Supervisor) -> Option<u32> {
    supervised_pid(supervisor).0
}

/// Advisory pre-restart unit-hygiene check (systemd only): warn when the LIVE
/// unit still carries the pre-#4862 `KillMode=control-group`. See
/// [`stale_unit_warning`].
pub fn warn_if_stale_unit(supervisor: Supervisor) {
    if supervisor != Supervisor::Systemd {
        return;
    }
    let unit = resolve_systemd_unit();
    let kill_mode = systemctl_show(&unit, "KillMode");
    let timeout_stop = systemctl_show(&unit, "TimeoutStopUSec");
    if let Some(warning) = stale_unit_warning(kill_mode.as_deref(), timeout_stop.as_deref()) {
        eprintln!();
        eprintln!("{warning}");
        eprintln!();
    }
}

/// Verify the supervisor actually relaunched the daemon, and self-heal if it
/// did not (Issue #5119). Returns the terminal verdict; progress and
/// diagnostics are printed to stderr as they happen.
///
/// This is the whole point of the issue: on a systemd host whose stop
/// transition timed out, the unit is left `failed` and `Restart=on-success` can
/// never fire — so without this the daemon simply stays down.
#[must_use]
pub fn verify_and_heal(supervisor: Supervisor, pre_pid: Option<u32>) -> RelaunchOutcome {
    let poll_secs = resolve_secs(std::env::var(POLL_SECS_ENV).ok().as_deref(), DEFAULT_POLL_SECS);
    let recovery_secs = resolve_secs(
        std::env::var(RECOVERY_POLL_SECS_ENV).ok().as_deref(),
        DEFAULT_RECOVERY_POLL_SECS,
    );
    let interval = resolve_interval(std::env::var(POLL_INTERVAL_SECS_ENV).ok().as_deref());

    eprintln!(
        "Verifying {} relaunches the daemon onto a NEW, live pid within {poll_secs}s \
         (pre-restart pid: {})...",
        supervisor.as_str(),
        pre_pid.map_or_else(|| "<none>".to_string(), |p| p.to_string())
    );

    if let Some(pid) =
        poll_until_relaunched(pre_pid, Duration::from_secs(poll_secs), interval, || {
            supervised_pid(supervisor)
        })
    {
        return RelaunchOutcome::Relaunched { pid, healed: false };
    }

    eprintln!(
        "WARNING: {} did NOT relaunch within {poll_secs}s of the restart ack.",
        supervisor.as_str()
    );
    log_diagnostics(supervisor);

    match supervisor {
        Supervisor::Systemd => heal_systemd(pre_pid, recovery_secs, interval),
        Supervisor::Launchd => heal_launchd(pre_pid, recovery_secs, interval),
    }
}

/// systemd self-heal: `reset-failed` + `start`, gated on a confirmed
/// `ActiveState=failed` (#4950's rule — never guess at a recovery for a unit
/// still mid-transition).
fn heal_systemd(pre_pid: Option<u32>, recovery_secs: u64, interval: Duration) -> RelaunchOutcome {
    let unit = resolve_systemd_unit();
    let active_state =
        systemctl_show(&unit, "ActiveState").unwrap_or_else(|| "unknown".to_string());
    let result = systemctl_show(&unit, "Result").unwrap_or_else(|| "unknown".to_string());

    if systemd_recovery_for(&active_state) == SystemdRecovery::RefuseUnknownState {
        return RelaunchOutcome::NotRelaunched {
            detail: format!(
                "The unit's ActiveState is '{active_state}' (Result={result}), not 'failed' — \
                 refusing to guess at a recovery action. Investigate manually: \
                 systemctl --user status {unit}"
            ),
        };
    }

    eprintln!(
        "Unit is in a 'failed' state (Result={result}) — systemd will NOT auto-relaunch a failed \
         unit even under Restart=on-success. Self-healing via 'systemctl --user reset-failed \
         {unit} && systemctl --user start {unit}'."
    );
    let mut reset = Command::new("systemctl");
    reset.args(["--user", "reset-failed", &unit]);
    let _ = probe(reset);
    let mut start = Command::new("systemctl");
    start.args(["--user", "start", &unit]);
    let _ = probe(start);

    match poll_until_relaunched(pre_pid, Duration::from_secs(recovery_secs), interval, || {
        supervised_pid(Supervisor::Systemd)
    }) {
        Some(pid) => RelaunchOutcome::Relaunched { pid, healed: true },
        None => RelaunchOutcome::NotRelaunched {
            detail: format!(
                "No new, live MainPID even after 'systemctl --user reset-failed && start' \
                 (waited {recovery_secs}s). Investigate manually: systemctl --user status {unit}"
            ),
        },
    }
}

/// launchd self-heal: a PLAIN `launchctl kickstart` — **never** `-k`, so a job
/// that DID relaunch during the poll race window is never killed (#4232).
fn heal_launchd(pre_pid: Option<u32>, recovery_secs: u64, interval: Duration) -> RelaunchOutcome {
    let service = resolve_launchd_service();
    eprintln!(
        "Falling back to 'launchctl kickstart {service}' (plain — NEVER -k — so a daemon that \
         DID relaunch during the race window above is never killed)."
    );
    let mut cmd = Command::new("launchctl");
    cmd.args(["kickstart", &service]);
    let _ = probe(cmd);

    match poll_until_relaunched(pre_pid, Duration::from_secs(recovery_secs), interval, || {
        supervised_pid(Supervisor::Launchd)
    }) {
        Some(pid) => RelaunchOutcome::Relaunched { pid, healed: true },
        // Every tick of the poll above went through `supervised_pid`, which
        // (Issue #6101) already cross-checks the skipped gui/<uid> domain
        // whenever domain resolution itself fell back to user/<uid> — so this
        // is not simply "the wrong domain was queried"; both the resolved
        // domain and, when applicable, the skipped gui/<uid> domain were
        // checked on every poll and neither ever reported a live pid.
        None => RelaunchOutcome::NotRelaunched {
            detail: format!(
                "No new, live pid even after 'launchctl kickstart {service}' (waited \
                 {recovery_secs}s) — every poll during that window also cross-checked the \
                 skipped gui/<uid> domain whenever domain resolution fell back to user/<uid>, so \
                 this is not simply \"the wrong domain was queried\". Investigate manually: \
                 launchctl print {service}"
            ),
        },
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervisor_parses_case_insensitively_and_rejects_others() {
        assert_eq!(Supervisor::parse("launchd"), Some(Supervisor::Launchd));
        assert_eq!(Supervisor::parse("LaunchD"), Some(Supervisor::Launchd));
        assert_eq!(Supervisor::parse("systemd"), Some(Supervisor::Systemd));
        assert_eq!(Supervisor::parse("SyStEmD"), Some(Supervisor::Systemd));
        assert_eq!(Supervisor::parse("runit"), None);
        assert_eq!(Supervisor::parse(""), None);
        assert_eq!(Supervisor::Launchd.as_str(), "launchd");
        assert_eq!(Supervisor::Systemd.as_str(), "systemd");
    }

    #[test]
    fn verification_is_on_by_default_and_only_explicit_falsey_disables_it() {
        assert!(verification_enabled(None));
        assert!(verification_enabled(Some("1")));
        assert!(verification_enabled(Some("yes")));
        assert!(verification_enabled(Some("")));
        assert!(!verification_enabled(Some("0")));
        assert!(!verification_enabled(Some("false")));
        assert!(!verification_enabled(Some("FALSE")));
        assert!(!verification_enabled(Some(" no ")));
    }

    /// The three degenerate pid readings the shell implementation rejects must
    /// stay rejected — each one is a way to falsely report "the daemon is back".
    #[test]
    fn confirmed_relaunch_rejects_every_degenerate_reading() {
        // No answer at all, and systemd's MainPID=0 "not running".
        assert!(!is_confirmed_relaunch(Some(100), None, true));
        assert!(!is_confirmed_relaunch(Some(100), Some(0), true));
        // Same pid: the old process lingering mid-teardown.
        assert!(!is_confirmed_relaunch(Some(100), Some(100), true));
        // Different pid but dead: a race artifact.
        assert!(!is_confirmed_relaunch(Some(100), Some(200), false));
        // The one accepting case.
        assert!(is_confirmed_relaunch(Some(100), Some(200), true));
        // Unknown pre-pid still accepts a live pid.
        assert!(is_confirmed_relaunch(None, Some(200), true));
        assert!(!is_confirmed_relaunch(None, Some(200), false));
    }

    #[test]
    fn systemd_recovery_only_fires_on_a_confirmed_failed_state() {
        assert_eq!(systemd_recovery_for("failed"), SystemdRecovery::ResetFailedAndStart);
        assert_eq!(systemd_recovery_for(" Failed \n"), SystemdRecovery::ResetFailedAndStart);
        for state in [
            "active",
            "activating",
            "deactivating",
            "inactive",
            "unknown",
            "",
        ] {
            assert_eq!(
                systemd_recovery_for(state),
                SystemdRecovery::RefuseUnknownState,
                "state {state} must not trigger a blind recovery"
            );
        }
    }

    /// The 2026-08-03 incident's unit shape: `KillMode=control-group` with the
    /// 90s default stop timeout. The warning must name the mode, the timeout,
    /// and the re-render remediation.
    #[test]
    fn stale_unit_warning_fires_for_control_group_only() {
        let warning = stale_unit_warning(Some("control-group"), Some("1min 30s"))
            .expect("a control-group unit must warn");
        assert!(warning.starts_with("WARNING:"));
        assert!(warning.contains("KillMode=control-group"));
        assert!(warning.contains("1min 30s"));
        assert!(warning.contains("Result: timeout"));
        assert!(warning.contains("--relaunch"));

        // The canonical, healthy unit is silent.
        assert!(stale_unit_warning(Some("mixed"), Some("20s")).is_none());
        assert!(stale_unit_warning(Some("Mixed"), Some("20s")).is_none());
        // Unreadable properties are silent (advisory only, never a blocker).
        assert!(stale_unit_warning(None, Some("20s")).is_none());
        // A missing timeout still warns, with a generic phrase.
        let warning = stale_unit_warning(Some("control-group"), None).expect("still warns");
        assert!(warning.contains("the unit's TimeoutStopSec"));
    }

    #[test]
    fn poll_accepts_the_first_confirmed_reading() {
        let readings = std::cell::RefCell::new(vec![
            (Some(100), true), // unchanged — old process lingering
            (None, false),     // MainPID=0 mid-transition
            (Some(0), false),  // explicit not-running
            (Some(777), true), // relaunched
        ]);
        let got = poll_until_relaunched(
            Some(100),
            Duration::from_secs(5),
            Duration::from_millis(1),
            || readings.borrow_mut().remove(0),
        );
        assert_eq!(got, Some(777));
    }

    /// A never-relaunching supervisor must time out rather than spin, and a
    /// zero timeout must still take one honest look.
    #[test]
    fn poll_times_out_and_always_probes_at_least_once() {
        let calls = std::cell::Cell::new(0usize);
        let got = poll_until_relaunched(
            Some(100),
            Duration::from_millis(0),
            Duration::from_millis(1),
            || {
                calls.set(calls.get() + 1);
                (Some(100), true)
            },
        );
        assert_eq!(got, None);
        assert_eq!(calls.get(), 1, "a zero timeout still probes once");

        let calls = std::cell::Cell::new(0usize);
        let got = poll_until_relaunched(
            None,
            Duration::from_millis(20),
            Duration::from_millis(1),
            || {
                calls.set(calls.get() + 1);
                (None, false)
            },
        );
        assert_eq!(got, None);
        assert!(calls.get() > 1, "a non-zero timeout polls repeatedly");
    }

    #[test]
    fn knobs_fall_back_to_the_shell_implementations_defaults() {
        assert_eq!(resolve_secs(None, DEFAULT_POLL_SECS), 30);
        assert_eq!(resolve_secs(Some("not-a-number"), DEFAULT_POLL_SECS), 30);
        assert_eq!(resolve_secs(Some(" 45 "), DEFAULT_POLL_SECS), 45);
        assert_eq!(resolve_secs(Some("0"), DEFAULT_POLL_SECS), 0);
        assert_eq!(resolve_interval(None), Duration::from_millis(1000));
        assert_eq!(resolve_interval(Some("1")), Duration::from_millis(1000));
        // Fractional intervals are supported, matching `sleep 0.2` in shell.
        assert_eq!(resolve_interval(Some("0.25")), Duration::from_millis(250));
        // Non-positive / unparsable fall back rather than busy-looping.
        assert_eq!(resolve_interval(Some("0")), Duration::from_millis(1000));
        assert_eq!(resolve_interval(Some("-3")), Duration::from_millis(1000));
        assert_eq!(resolve_interval(Some("abc")), Duration::from_millis(1000));
    }

    #[test]
    fn launchctl_pid_is_parsed_from_the_print_block() {
        let output = "\
com.rjwalters.loom-daemon = {
\tactive count = 1
\tpath = /Users/x/Library/LaunchAgents/com.rjwalters.loom-daemon.plist
\tstate = running
\tpid = 99917
\tprogram = /Users/x/.local/bin/loom-daemon
}";
        assert_eq!(parse_launchctl_pid(output), Some(99917));
        assert_eq!(parse_launchctl_pid("state = not running\n"), None);
        assert_eq!(parse_launchctl_pid(""), None);
    }

    #[test]
    fn outcomes_render_their_operator_facing_verdict() {
        let ok = RelaunchOutcome::Relaunched {
            pid: 42,
            healed: false,
        };
        assert!(ok.is_ok());
        assert!(ok.render("systemd", 30).contains("on its own"));

        let healed = RelaunchOutcome::Relaunched {
            pid: 42,
            healed: true,
        };
        assert!(healed.is_ok());
        assert!(healed.render("systemd", 30).contains("RECOVERED"));

        let failed = RelaunchOutcome::NotRelaunched {
            detail: "unit is masked".to_string(),
        };
        assert!(!failed.is_ok());
        let rendered = failed.render("systemd", 30);
        assert!(rendered.contains("DAEMONLESS"));
        assert!(rendered.contains("unit is masked"));

        let skipped = RelaunchOutcome::Skipped {
            reason: "disabled".to_string(),
        };
        assert!(skipped.is_ok(), "a skipped verification asserts nothing");
        assert!(skipped.render("systemd", 30).contains("skipped"));
    }

    /// `resolve_systemd_unit` mirrors the shell resolver: override wins, blank
    /// falls back. NOTE: the sole test mutating `LOOM_SYSTEMD_UNIT`.
    #[test]
    fn systemd_unit_resolution_mirrors_the_shell_resolver() {
        std::env::remove_var("LOOM_SYSTEMD_UNIT");
        assert_eq!(resolve_systemd_unit(), "loom-daemon.service");
        std::env::set_var("LOOM_SYSTEMD_UNIT", "scratch-loom.service");
        assert_eq!(resolve_systemd_unit(), "scratch-loom.service");
        std::env::set_var("LOOM_SYSTEMD_UNIT", "   ");
        assert_eq!(resolve_systemd_unit(), "loom-daemon.service");
        std::env::remove_var("LOOM_SYSTEMD_UNIT");
    }

    // ========================================================================
    // resolve_launchd_service_detailed / supervised_pid — the launchd domain
    // cross-check (Issue #6101). Before this module, NOTHING exercised
    // `resolve_launchd_service()` at all.
    // ========================================================================

    use serial_test::serial;
    use std::path::Path;

    /// Write an executable `#!/bin/sh` stub named `name` into `dir` — the
    /// same PATH-stub pattern `disk_headroom.rs` / `daemon_install_state.rs`
    /// already use for mocking a subprocess probe.
    fn write_stub(dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
    }

    /// Run `body` with `dir` prepended to `PATH`, restoring `PATH` afterwards.
    /// Callers must be `#[serial]` (unnamed — the same default lock
    /// `disk_headroom.rs` / `daemon_install_state.rs` use) since `PATH` and
    /// `LOOM_LAUNCHD_LABEL` / `LOOM_LAUNCHD_DOMAIN` are all process-global.
    fn with_path_prefix<T>(dir: &Path, body: impl FnOnce() -> T) -> T {
        let old_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old_path}", dir.display()));
        let out = body();
        std::env::set_var("PATH", old_path);
        out
    }

    /// A `launchctl` stub whose bare `print gui/<uid>` reachability probe
    /// succeeds — the "a live GUI/Aqua session exists" case.
    const LAUNCHCTL_GUI_SUCCEEDS: &str = "#!/bin/sh\n\
        if [ \"$1\" = print ]; then\n  \
        case \"$2\" in\n    \
        gui/*) exit 0 ;;\n  \
        esac\n\
        fi\n\
        exit 1\n";

    /// A `launchctl` stub that always fails — forces the `user/<uid>`
    /// fallback path regardless of any real GUI session on the test host.
    const LAUNCHCTL_ALWAYS_FAILS: &str = "#!/bin/sh\nexit 1\n";

    #[test]
    #[serial]
    fn resolve_launchd_service_detailed_uses_gui_domain_when_the_probe_succeeds() {
        std::env::remove_var("LOOM_LAUNCHD_DOMAIN");
        std::env::set_var("LOOM_LAUNCHD_LABEL", "com.example.test-6101");
        let stub_dir = tempfile::tempdir().unwrap();
        write_stub(stub_dir.path(), "launchctl", LAUNCHCTL_GUI_SUCCEEDS);

        let resolution = with_path_prefix(stub_dir.path(), resolve_launchd_service_detailed);
        std::env::remove_var("LOOM_LAUNCHD_LABEL");

        assert!(
            resolution.service.starts_with("gui/"),
            "a succeeding gui/<uid> probe must resolve the gui domain, got {}",
            resolution.service
        );
        assert!(resolution.service.ends_with("/com.example.test-6101"));
        assert_eq!(
            resolution.fallback_check_service, None,
            "nothing was skipped when gui/<uid> itself resolved"
        );
    }

    #[test]
    #[serial]
    fn resolve_launchd_service_detailed_falls_back_to_user_uid_when_the_gui_probe_fails() {
        std::env::remove_var("LOOM_LAUNCHD_DOMAIN");
        std::env::set_var("LOOM_LAUNCHD_LABEL", "com.example.test-6101");
        let stub_dir = tempfile::tempdir().unwrap();
        write_stub(stub_dir.path(), "launchctl", LAUNCHCTL_ALWAYS_FAILS);

        let resolution = with_path_prefix(stub_dir.path(), resolve_launchd_service_detailed);
        std::env::remove_var("LOOM_LAUNCHD_LABEL");

        assert!(
            resolution.service.starts_with("user/"),
            "a failing gui/<uid> probe must fall back to user/<uid>, got {}",
            resolution.service
        );
        assert!(resolution.service.ends_with("/com.example.test-6101"));
        let check = resolution.fallback_check_service.expect(
            "the skipped gui/<uid> domain must be reported for cross-check (#6101, mirroring #4694)",
        );
        assert!(check.starts_with("gui/"));
        assert!(check.ends_with("/com.example.test-6101"));
    }

    #[test]
    #[serial]
    fn resolve_launchd_service_detailed_honors_domain_override_unconditionally() {
        // The override wins verbatim even against a stub that WOULD succeed
        // for gui/<uid> — proving the override short-circuits before any
        // probe runs at all, matching `resolve_launchd_domain_detailed`'s
        // AC6 (no cross-check for an explicit override) in
        // `daemon_install_state.rs`.
        std::env::set_var("LOOM_LAUNCHD_DOMAIN", "custom/999");
        std::env::set_var("LOOM_LAUNCHD_LABEL", "com.example.test-6101");
        let stub_dir = tempfile::tempdir().unwrap();
        write_stub(stub_dir.path(), "launchctl", LAUNCHCTL_GUI_SUCCEEDS);

        let resolution = with_path_prefix(stub_dir.path(), resolve_launchd_service_detailed);
        std::env::remove_var("LOOM_LAUNCHD_DOMAIN");
        std::env::remove_var("LOOM_LAUNCHD_LABEL");

        assert_eq!(resolution.service, "custom/999/com.example.test-6101");
        assert_eq!(
            resolution.fallback_check_service, None,
            "an explicit override is honored verbatim — no cross-check"
        );
    }

    #[test]
    #[serial]
    fn supervised_pid_cross_checks_the_skipped_gui_domain_before_reporting_no_pid() {
        // The exact incident shape (#6101): domain resolution's bare
        // `gui/<uid>` reachability probe misses (falls back to user/<uid>),
        // the primary user/<uid> probe finds nothing (matching the
        // incident's `Could not find service … in domain for uid` output),
        // but the job is genuinely there when the skipped gui/<uid> domain
        // is queried directly with the label. `supervised_pid` must return
        // that pid, not a bare "no pid".
        std::env::remove_var("LOOM_LAUNCHD_DOMAIN");
        std::env::set_var("LOOM_LAUNCHD_LABEL", "com.example.test-6101");
        let stub_dir = tempfile::tempdir().unwrap();
        let stub = "#!/bin/sh\n\
            if [ \"$1\" = print ]; then\n  \
            case \"$2\" in\n    \
            gui/*/com.example.test-6101) echo 'pid = 55555'; exit 0 ;;\n    \
            gui/*) exit 1 ;;\n    \
            user/*) echo 'Could not find service'; exit 1 ;;\n  \
            esac\n\
            fi\n\
            exit 1\n";
        write_stub(stub_dir.path(), "launchctl", stub);
        // `kill -0 55555` must read as alive for the cross-checked pid to be
        // reported live; stub it rather than depend on a real pid 55555.
        write_stub(stub_dir.path(), "kill", "#!/bin/sh\nexit 0\n");

        let (pid, alive) =
            with_path_prefix(stub_dir.path(), || supervised_pid(Supervisor::Launchd));
        std::env::remove_var("LOOM_LAUNCHD_LABEL");

        assert_eq!(
            pid,
            Some(55555),
            "the skipped gui/<uid> domain must be cross-checked and its pid reported, not a \
             bare None from the user/<uid> miss alone"
        );
        assert!(alive);
    }
}
