//! Install-state classification for `loom-daemon status`'s unreachable-daemon
//! error path (Issue #4069, AC3 of #4011).
//!
//! # Why
//!
//! Before this module, every transport failure in `handle_status_command`'s
//! `Err` arm collapsed into one generic message ("Could not reach
//! loom-daemon... Is the daemon running?"). That message is actively
//! misleading in two of three real situations: it says nothing about whether
//! autonomy was ever *expected* on this host (the #4011 silent-autonomy-loss
//! scenario), and it recommends a start even when the daemon process is alive
//! and the singleton guard will refuse.
//!
//! This module is the READ-ONLY probe that tells those situations apart, by
//! consuming the same **autonomy-desired marker** and **heartbeat file** the
//! host-side `loom-daemon-watchdog.sh` (#4011) already uses — and mirroring
//! its exact precedence so `status` and the watchdog log can never
//! contradict each other:
//!
//! 1. Marker absent → [`InstallState::NotExpected`]: no daemon is expected
//!    (deliberately stopped, or never started).
//! 2. Marker present, no live process → [`InstallState::ExpectedButDead`]:
//!    the #4011 divergence — autonomy was expected but the daemon is gone.
//! 3. Marker present, process alive (but IPC still failed, since we only
//!    reach this module from the status query's `Err` arm), and the process
//!    is *young* (age ≤ the startup-grace window, default 90s) →
//!    [`InstallState::AliveStarting`]: a normal post-`bootout`/`bootstrap`
//!    restart whose socket has not bound yet (it takes ~40–60s). This is NOT
//!    a fault — it must not print the stop/start remediation (#4213).
//! 4. Marker present, process alive, IPC failed, and the process is *older*
//!    than the grace window (or its age is undeterminable) →
//!    [`InstallState::AliveButUnresponsive`], qualified by heartbeat
//!    freshness: fresh ⇒ likely an IPC/socket-layer fault, stale ⇒ likely a
//!    wedged daemon, **prior-boot** (#4368) ⇒ the heartbeat file predates
//!    this process's own start time and is therefore NOT evidence about the
//!    current process (treated like `Unknown` for advice purposes — the
//!    caller must not print the stop/start remediation for it).
//!
//! The startup-grace discriminator is **process age alone** (via
//! `ps -o etime= -p <pid>`), never socket-file presence: the `Err` arm has
//! already established IPC failure, and a stale socket file from the prior run
//! can legitimately still exist during startup. `loom-daemon-watchdog.sh` never
//! probes IPC, so it can never emit the fault verdict and needs no matching
//! grace state — this module's grace verdict cannot contradict it (#4213).
//!
//! Like [`crate::self_update`], this module is modeled to be inherently
//! side-effect-free and never fails the command: an unreadable/malformed
//! marker, a missing `launchctl`, a stale/unowned pid, or an unreadable
//! heartbeat mtime all degrade to a less-specific (but never wrong) verdict
//! rather than propagating an error. The caller (`main.rs`) always has a
//! fallback generic message to print if [`probe`] returns `None`.
//!
//! This module changes reporting only: it starts nothing, stops nothing, and
//! writes no state (Non-goal in #4069 — do not add writes here).
//!
//! # Watchdog protection state (#4354, AC4 of #4331)
//!
//! [`InstallState`] above answers "why is the daemon unreachable?" and therefore
//! only runs on the `Err` arm. A *reachable* daemon needs a different question
//! answered: **is it actually protected?** — i.e. is the autonomy-desired marker
//! present (so a crash is detectable at all), and is the watchdog launchd job /
//! systemd timer actually provisioned (so something is scheduled to notice)?
//! [`ProtectionState`] / [`probe_protection`] are that **sibling** classification.
//! They deliberately do NOT add [`InstallState`] variants: those variants carry
//! exit-code semantics (#4069) that the reachable path must not perturb — the
//! reachable path always exits 0 regardless of protection state.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Default macOS launchd label, matching `loom-daemon-start.sh` /
/// `loom-daemon-watchdog.sh`'s fallback when the marker predates the
/// `launchd_label` field.
pub const DEFAULT_LAUNCHD_LABEL: &str = "com.rjwalters.loom-daemon";

/// Basename of the autonomy-desired intent marker under the resolved loom
/// dir, matching `loom-daemon-start.sh`'s `INTENT_MARKER` default.
pub const MARKER_FILENAME: &str = "autonomy-desired";

/// Exit code for `not-expected` (and any undiagnosable fallback) — unchanged
/// from the pre-#4069 generic behavior, so existing scripting that only
/// checks non-zero is unaffected.
pub const EXIT_NOT_EXPECTED: i32 = 1;
/// Exit code for `expected-but-dead` (the #4011 silent-autonomy-loss state).
pub const EXIT_EXPECTED_BUT_DEAD: i32 = 3;
/// Exit code for `alive-but-unresponsive` (IPC fault or wedged daemon).
/// Also reused for `alive-starting` — a distinct new exit code is a compat
/// decision this issue does not require (#4213); scripts that only check
/// nonzero are unaffected either way, and JSON consumers get the distinct
/// `state` string.
pub const EXIT_ALIVE_BUT_UNRESPONSIVE: i32 = 4;

/// Default startup-grace window (seconds): a live daemon whose process age is
/// under this and whose socket has not bound yet is treated as *starting*, not
/// faulted. Overridable via `LOOM_DAEMON_STARTUP_GRACE_SECS`. Sized above the
/// observed ~40–60s `bootout`/`bootstrap` socket-bind latency (#4213).
pub const DEFAULT_STARTUP_GRACE_SECS: u64 = 90;

/// Wall-clock bound (seconds) on every query-only subprocess probe this module
/// makes — `kill -0`, `id -u`, `ps -o etime=`, `launchctl print`, `systemctl
/// --user is-enabled` (#4548).
///
/// All of these normally answer in milliseconds, but each has a real-world hang
/// mode: a wedged `systemd --user` bus makes `systemctl` block on a D-Bus
/// connect, and `launchctl print` against an unreachable domain can stall on
/// XPC. Unbounded, any one of them wedges `loom-daemon status` — the very
/// command an operator runs to diagnose a wedge. A couple of seconds is far
/// above the real cost of these probes even on a loaded CI runner, yet keeps
/// `status` responsive.
const PROBE_TIMEOUT_SECS: u64 = 2;

/// [`PROBE_TIMEOUT_SECS`] as a [`Duration`].
const PROBE_TIMEOUT: Duration = Duration::from_secs(PROBE_TIMEOUT_SECS);

/// Run a query-only probe `cmd`, abandoning (killing) it if it exceeds
/// `timeout`, and collapse *every* failure mode to `None` (#4548).
///
/// `None` therefore means "no usable answer" for all three of: the binary is
/// absent/unspawnable, the spawn itself failed, or the probe hung past
/// `timeout`. Every caller in this module already degraded a failed/absent
/// binary to its unknown value (`None` / `false`), so folding the timeout into
/// the same arm keeps the existing verdict semantics — including the #4069
/// exit-code mapping — byte-for-byte unchanged for any probe that completes in
/// time.
///
/// `stdin` is explicitly nulled to preserve `Command::output()`'s contract:
/// `output()` nulls stdin, while [`output_with_timeout`]'s `spawn()` would
/// otherwise *inherit* the daemon's stdin. None of these probes read stdin, but
/// inheriting it is a behavior change this wrap must not smuggle in.
///
/// [`output_with_timeout`]: crate::sweep_registry::output_with_timeout
fn probe_output(mut cmd: Command, timeout: Duration) -> Option<std::process::Output> {
    cmd.stdin(std::process::Stdio::null());
    crate::sweep_registry::output_with_timeout(cmd, timeout)
        .ok()
        .flatten()
}

// ============================================================================
// Public types
// ============================================================================

/// The states `status` can distinguish once a live IPC round-trip has
/// failed. See module docs for the precedence this mirrors from
/// `loom-daemon-watchdog.sh`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallState {
    /// No autonomy-desired marker — a daemon is not currently expected
    /// (deliberately stopped via `loom-daemon-stop.sh`, or never started).
    NotExpected,
    /// Marker present, but no live process for it — the #4011 divergence.
    ExpectedButDead,
    /// Marker present, process alive, IPC failed, but the process is young
    /// (age ≤ startup-grace window): a normal restart whose socket has not
    /// bound yet — NOT a fault (#4213).
    AliveStarting,
    /// Marker present and the process is alive, but IPC still failed.
    AliveButUnresponsive,
}

impl InstallState {
    /// Machine-readable enum value for `--json` rendering.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            InstallState::NotExpected => "not-expected",
            InstallState::ExpectedButDead => "expected-but-dead",
            InstallState::AliveStarting => "alive-starting",
            InstallState::AliveButUnresponsive => "alive-but-unresponsive",
        }
    }

    /// The exit code `loom-daemon status` should use for this state.
    #[must_use]
    pub fn exit_code(self) -> i32 {
        match self {
            InstallState::NotExpected => EXIT_NOT_EXPECTED,
            InstallState::ExpectedButDead => EXIT_EXPECTED_BUT_DEAD,
            // Reuses code 4 — see [`EXIT_ALIVE_BUT_UNRESPONSIVE`].
            InstallState::AliveStarting => EXIT_ALIVE_BUT_UNRESPONSIVE,
            InstallState::AliveButUnresponsive => EXIT_ALIVE_BUT_UNRESPONSIVE,
        }
    }
}

/// Heartbeat freshness qualifier for [`InstallState::AliveButUnresponsive`] —
/// sharpens "alive but not answering" into "likely an IPC fault" (fresh) vs
/// "likely wedged" (stale). `Unknown` is a degradation (no heartbeat file,
/// unreadable mtime, or heartbeat loop disabled) — never a false report.
/// `PriorBoot` (#4368) is a distinct degradation: the heartbeat file's mtime
/// predates the live process's own start time, so it is necessarily left
/// over from a previous boot (or a previous enablement of the opt-in
/// heartbeat loop) and carries NO evidence about the current process —
/// never rendered as `Stale`/wedged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatFreshness {
    Fresh,
    Stale,
    Unknown,
    PriorBoot,
}

impl HeartbeatFreshness {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            HeartbeatFreshness::Fresh => "fresh",
            HeartbeatFreshness::Stale => "stale",
            HeartbeatFreshness::Unknown => "unknown",
            HeartbeatFreshness::PriorBoot => "prior-boot",
        }
    }
}

/// The full classification result for one probe.
#[derive(Debug, Clone)]
pub struct InstallStateReport {
    pub state: InstallState,
    /// The marker's `started_at` field, when present and the marker exists.
    pub started_at: Option<String>,
    /// The live pid, when the process is alive ([`InstallState::AliveStarting`]
    /// or [`InstallState::AliveButUnresponsive`]).
    pub pid: Option<u32>,
    /// Human-readable liveness detail (mirrors the watchdog's own
    /// `liveness_detail` strings so log/status messages read consistently).
    pub liveness_detail: Option<String>,
    /// Heartbeat freshness — only computed for `AliveButUnresponsive`.
    pub heartbeat_freshness: Option<HeartbeatFreshness>,
    /// Heartbeat age in seconds, when its mtime was readable.
    pub heartbeat_age_secs: Option<u64>,
    /// The staleness threshold used for the freshness verdict (seconds).
    pub heartbeat_stale_threshold_secs: Option<u64>,
    /// The live process's age in seconds (`ps -o etime=`), when it was alive
    /// and the age was parseable. `None` degrades to no grace claim (#4213).
    pub process_age_secs: Option<u64>,
    /// The startup-grace window used to classify a young process (seconds) —
    /// present whenever the process was alive (both `AliveStarting` and
    /// `AliveButUnresponsive`).
    pub startup_grace_threshold_secs: Option<u64>,
    /// The watchdog log path to point an operator at (advisory only — never
    /// read by this module).
    pub watchdog_log_path: PathBuf,
}

impl InstallStateReport {
    fn not_expected(watchdog_log_path: PathBuf) -> Self {
        InstallStateReport {
            state: InstallState::NotExpected,
            started_at: None,
            pid: None,
            liveness_detail: None,
            heartbeat_freshness: None,
            heartbeat_age_secs: None,
            heartbeat_stale_threshold_secs: None,
            process_age_secs: None,
            startup_grace_threshold_secs: None,
            watchdog_log_path,
        }
    }
}

/// Env/platform overrides that win over the marker's recorded fields —
/// mirrors `loom-daemon-watchdog.sh`'s "env wins over marker" rule exactly.
/// Split out from [`probe`] so unit tests can construct one directly without
/// mutating process-global env vars.
#[derive(Debug, Clone)]
pub struct EnvOverrides {
    /// From `LOOM_DAEMON_LAUNCHD`: this override is **one-directional**, exactly
    /// mirroring `loom-daemon-watchdog.sh` — a falsy value (`0`/`false`/`no`)
    /// yields `Some(false)`, forcing the pid-file path even when the marker says
    /// `use_launchd=true`. Any other value (or unset) yields `None`, deferring to
    /// the marker's own `use_launchd`; the env var can never force launchd *on*.
    pub launchd_override: Option<bool>,
    /// From `LOOM_LAUNCHD_LABEL`.
    pub launchd_label_override: Option<String>,
    /// From `LOOM_LAUNCHD_DOMAIN` — the launchd domain to probe in, mirroring
    /// `lib/launchd-domain.sh::resolve_launchd_domain`'s override (same field
    /// [`ProtectionEnv`] carries for the reachable-path protection probe).
    pub launchd_domain_override: Option<String>,
    /// From `LOOM_DAEMON_HEARTBEAT_STALE_SECS`.
    pub heartbeat_stale_secs_override: Option<u64>,
    /// From `LOOM_DAEMON_STARTUP_GRACE_SECS` — the startup-grace window in
    /// seconds. `None` defers to [`DEFAULT_STARTUP_GRACE_SECS`].
    pub startup_grace_secs_override: Option<u64>,
    /// Whether this host can have a launchd job at all — the watchdog forces
    /// `USE_LAUNCHD=false` on non-Darwin regardless of the marker.
    pub is_darwin: bool,
}

/// Parse the `LOOM_DAEMON_LAUNCHD` env value into a launchd override.
///
/// **One-directional, mirroring `loom-daemon-watchdog.sh` exactly**: a falsy
/// value (`0`/`false`/`no`, case-insensitive) yields `Some(false)`, forcing the
/// pid-file path even when the marker recorded `use_launchd=true`. Any other
/// value — including a truthy `1`/`true`/`yes` — and the unset case yield
/// `None`, deferring to the marker's own `use_launchd`. The env var can never
/// force launchd *on*, which is the whole point of matching the shell's
/// `^(0|false|no)$`-only regex.
fn parse_launchd_override(value: Option<&str>) -> Option<bool> {
    value
        .filter(|v| matches!(v.to_ascii_lowercase().as_str(), "0" | "false" | "no"))
        .map(|_| false)
}

impl EnvOverrides {
    /// Resolve from the real process environment + platform, for the
    /// production entry point ([`probe`]).
    #[must_use]
    pub fn from_env() -> Self {
        let launchd_override =
            parse_launchd_override(std::env::var("LOOM_DAEMON_LAUNCHD").ok().as_deref());
        let launchd_label_override = std::env::var("LOOM_LAUNCHD_LABEL")
            .ok()
            .filter(|s| !s.is_empty());
        let launchd_domain_override = std::env::var("LOOM_LAUNCHD_DOMAIN")
            .ok()
            .filter(|s| !s.is_empty());
        let heartbeat_stale_secs_override = std::env::var("LOOM_DAEMON_HEARTBEAT_STALE_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok());
        let startup_grace_secs_override = std::env::var("LOOM_DAEMON_STARTUP_GRACE_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok());
        EnvOverrides {
            launchd_override,
            launchd_label_override,
            launchd_domain_override,
            heartbeat_stale_secs_override,
            startup_grace_secs_override,
            is_darwin: cfg!(target_os = "macos"),
        }
    }
}

// ============================================================================
// Path resolution — mirrors `main.rs::resolve_loom_dir` / `daemon_heartbeat`
// deliberately (both are tiny; kept in sync by inspection rather than shared,
// same rationale `daemon_heartbeat.rs` documents for its own copy).
// ============================================================================

fn resolve_loom_dir() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("LOOM_SOCKET_PATH") {
        return PathBuf::from(path).parent().map(Path::to_path_buf);
    }
    dirs::home_dir().map(|h| h.join(".loom"))
}

/// Resolve the autonomy-desired marker path: `LOOM_AUTONOMY_MARKER` env
/// override first (matching `loom-daemon-start.sh`'s `INTENT_MARKER`), else
/// `<loom_dir>/autonomy-desired`.
///
/// Delegates to [`crate::autonomy_marker::resolve_marker_path`] (#4354) so this
/// module, the startup healer (#4331), and the watchdog can never resolve the
/// marker differently — there is exactly one implementation of the rule.
fn resolve_marker_path(loom_dir: &Path) -> PathBuf {
    crate::autonomy_marker::resolve_marker_path(loom_dir)
}

// ============================================================================
// Marker parsing
// ============================================================================

/// Parse a `key=value` marker file: comments (`#`) and blank lines ignored,
/// first occurrence of a key wins (mirrors the watchdog's
/// `grep -E "^${key}=" | head -n1`).
fn parse_marker(contents: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            map.entry(key.trim().to_string())
                .or_insert_with(|| value.to_string());
        }
    }
    map
}

/// The marker fields the probe needs, with the watchdog's own per-field
/// fallbacks applied for markers that predate a field.
struct MarkerFields {
    started_at: Option<String>,
    pid_file: Option<PathBuf>,
    heartbeat_file: PathBuf,
    heartbeat_interval_secs: u64,
    use_launchd: bool,
    launchd_label: String,
}

fn resolve_marker_fields(map: &HashMap<String, String>, loom_dir: &Path) -> MarkerFields {
    let started_at = map.get("started_at").filter(|s| !s.is_empty()).cloned();

    let pid_file = map
        .get("pid_file")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);

    let heartbeat_file = map
        .get("heartbeat_file")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| loom_dir.join(crate::daemon_heartbeat::HEARTBEAT_FILENAME));

    let heartbeat_interval_secs = map
        .get("heartbeat_interval_secs")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(crate::daemon_heartbeat::DEFAULT_HEARTBEAT_INTERVAL_SECS);

    // Marker default is `true` when absent/unparsed (matches the watchdog's
    // `USE_LAUNCHD="${MARKER_USE_LAUNCHD:-true}"`).
    let use_launchd = map
        .get("use_launchd")
        .map(|v| !matches!(v.to_ascii_lowercase().as_str(), "false" | "0" | "no"))
        .unwrap_or(true);

    let launchd_label = map
        .get("launchd_label")
        .filter(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| DEFAULT_LAUNCHD_LABEL.to_string());

    MarkerFields {
        started_at,
        pid_file,
        heartbeat_file,
        heartbeat_interval_secs,
        use_launchd,
        launchd_label,
    }
}

// ============================================================================
// Liveness
// ============================================================================

/// `kill -0 <pid>` via subprocess (matches `terminal.rs`'s existing pattern
/// in this crate — no `libc`/`nix` dependency needed). Returns `false` for
/// both "no such process" and "not owned by us", exactly like the shell
/// script's `kill -0 "$pid" 2>/dev/null`. Bounded by [`PROBE_TIMEOUT`]; a hung
/// `kill` degrades to `false`, exactly like an absent one (#4548).
fn pid_alive(pid: u32) -> bool {
    let mut cmd = Command::new("kill");
    cmd.args(["-0", &pid.to_string()]);
    probe_output(cmd, PROBE_TIMEOUT).is_some_and(|o| o.status.success())
}

/// Current uid via `id -u`. Bounded by [`PROBE_TIMEOUT`]; a hung `id` degrades
/// to `None`, exactly like an absent one (#4548).
fn current_uid() -> Option<String> {
    let mut cmd = Command::new("id");
    cmd.arg("-u");
    probe_output(cmd, PROBE_TIMEOUT)
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Parse `launchctl print <domain>/<label>` output for a live pid — mirrors
/// the watchdog's `awk -F'= ' '/^[[:space:]]*pid = /{...; print $2; exit}'`.
/// `domain` is an already-resolved launchd domain (see
/// [`resolve_launchd_domain_detailed`]) — the caller resolves it once and
/// reuses it for both this probe and any human-readable detail string,
/// avoiding a duplicate `launchctl`/`id` round trip per [`check_liveness`]
/// call.
///
/// Bounded by [`PROBE_TIMEOUT`]: a `launchctl print` that stalls on XPC
/// degrades to `None` — "no live pid" — exactly like an absent `launchctl`
/// (#4548).
fn launchctl_pid(domain: &str, label: &str) -> Option<u32> {
    let service = format!("{domain}/{label}");
    let mut cmd = Command::new("launchctl");
    cmd.args(["print", &service]);
    let output = probe_output(cmd, PROBE_TIMEOUT)?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("pid = ") {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(pid) = digits.parse::<u32>() {
                return Some(pid);
            }
        }
    }
    None
}

/// One liveness check result: whether the expected daemon is alive, a
/// human-readable detail string (mirrors the watchdog's `liveness_detail`),
/// and the live pid when alive.
struct Liveness {
    alive: bool,
    detail: String,
    pid: Option<u32>,
}

/// Parse a pid file into its recorded pid — `None` when the file is missing,
/// unreadable, or does not hold a bare integer. Says nothing about whether
/// that pid is *alive*; see [`pid_file_alive_pid`].
fn read_pid_file(pid_file: &Path) -> Option<u32> {
    std::fs::read_to_string(pid_file)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

/// The pid recorded in `pid_file`, iff that pid is currently alive — an
/// independent liveness signal that does not involve launchd at all. Shared by
/// the non-launchd liveness path and, as of #4694 AC2, the launchd path's
/// pid-file cross-check.
fn pid_file_alive_pid(pid_file: &Path) -> Option<u32> {
    read_pid_file(pid_file).filter(|pid| pid_alive(*pid))
}

fn check_liveness(
    use_launchd: bool,
    label: &str,
    pid_file: Option<&Path>,
    domain_override: Option<&str>,
) -> Liveness {
    if use_launchd {
        // Same domain-resolution rule the reachable-path protection probe
        // uses (#4354/#4533): explicit `LOOM_LAUNCHD_DOMAIN` override, else
        // `gui/<uid>` when that domain resolves, else `user/<uid>`. Resolved
        // once and reused for both the probe and the detail string so they
        // never disagree (#4536).
        let resolution = resolve_launchd_domain_detailed(domain_override);
        let domain = resolution.domain.clone();
        let service = domain
            .as_deref()
            .map(|d| format!("{d}/{label}"))
            .unwrap_or_else(|| label.to_string());
        if let Some(pid) = domain.as_deref().and_then(|d| launchctl_pid(d, label)) {
            if pid_alive(pid) {
                return Liveness {
                    alive: true,
                    detail: format!("launchd job {service} alive (pid {pid})"),
                    pid: Some(pid),
                };
            }
        }

        // #4694 AC1: the primary domain came back negative. Before trusting
        // that, cross-check the domain `resolve_launchd_domain_detailed`
        // skipped because its `gui/<uid>` reachability probe failed — that
        // single probe cannot distinguish a genuine absence from a transient
        // flake, and folding a flake into a permanent `user/<uid>` fallback
        // for the rest of this call previously produced a false "not
        // loaded" verdict for a job that was actually alive under
        // `gui/<uid>`. No cross-check when an explicit `LOOM_LAUNCHD_DOMAIN`
        // override was honored (AC6) — `fallback_check_domain` is always
        // `None` in that case.
        if let Some(check_domain) = resolution.fallback_check_domain.as_deref() {
            if let Some(pid) = launchctl_pid(check_domain, label) {
                if pid_alive(pid) {
                    return Liveness {
                        alive: true,
                        detail: format!("launchd job {check_domain}/{label} alive (pid {pid})"),
                        pid: Some(pid),
                    };
                }
            }
        }

        // #4694 AC2: both launchd domains agree the job is not loaded/alive.
        // Cross-check the pid file — an independent liveness signal that does
        // not depend on `launchctl`/launchd at all — before declaring the
        // daemon dead. A live pid-file pid overrides the launchd-domain
        // negative rather than silently disagreeing with it: this is the
        // specific check that would have prevented the near-miss where the
        // daemon was alive (with 6 sweeps running) but `status` reported it
        // dead solely because the launchd domain probe looked in the wrong
        // place.
        if let Some(pf) = pid_file {
            if let Some(pid) = pid_file_alive_pid(pf) {
                return Liveness {
                    alive: true,
                    detail: format!(
                        "launchd job {service} not loaded, but pid file {} shows pid {pid} alive",
                        pf.display()
                    ),
                    pid: Some(pid),
                };
            }
        }

        return Liveness {
            alive: false,
            detail: format!("launchd job {service} is not loaded/alive"),
            pid: None,
        };
    }

    // Non-launchd (nohup / Linux) path: the pid file is the only signal.
    match pid_file {
        Some(pf) => match read_pid_file(pf) {
            Some(pid) if pid_alive(pid) => Liveness {
                alive: true,
                detail: format!("pid {pid} (from {}) alive", pf.display()),
                pid: Some(pid),
            },
            Some(_) => Liveness {
                alive: false,
                detail: format!("pid file {} present but pid not alive", pf.display()),
                pid: None,
            },
            None => Liveness {
                alive: false,
                detail: format!("no live pid file at {}", pf.display()),
                pid: None,
            },
        },
        None => Liveness {
            alive: false,
            detail: "no live pid file at <none>".to_string(),
            pid: None,
        },
    }
}

// ============================================================================
// Process age (startup-grace discriminator)
// ============================================================================

/// Parse a macOS `ps -o etime=` duration into whole seconds. The format is
/// `[[dd-]hh:]mm:ss` (there is no `etimes` seconds-only keyword on macOS), so
/// this accepts `ss`, `mm:ss`, `hh:mm:ss`, and `dd-hh:mm:ss`. Any unexpected
/// shape or non-numeric field yields `None` — the caller treats an unparseable
/// age as *unknown* and makes no grace claim, never a false "starting" verdict.
fn parse_etime(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    // Split optional leading `dd-` day component.
    let (days, rest) = match raw.split_once('-') {
        Some((d, r)) => (d.trim().parse::<u64>().ok()?, r),
        None => (0u64, raw),
    };
    let parts: Vec<&str> = rest.split(':').collect();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [s] => (0u64, 0u64, s.parse::<u64>().ok()?),
        [m, s] => (0u64, m.parse::<u64>().ok()?, s.parse::<u64>().ok()?),
        [h, m, s] => (h.parse::<u64>().ok()?, m.parse::<u64>().ok()?, s.parse::<u64>().ok()?),
        _ => return None,
    };
    Some(days * 86_400 + hours * 3_600 + minutes * 60 + seconds)
}

/// Probe a live pid's age via `ps -o etime= -p <pid>` (no `libc`/`nix`
/// dependency — matches this module's `kill -0` / `launchctl` subprocess
/// pattern). Degrades to `None` on a failed/absent `ps` or unparseable output,
/// so the caller falls through to today's verdicts rather than falsely
/// reporting "starting" (#4213). Bounded by [`PROBE_TIMEOUT`]: a hung `ps`
/// takes the same `None` path (#4548).
fn process_age_secs(pid: u32) -> Option<u64> {
    let mut cmd = Command::new("ps");
    cmd.args(["-o", "etime=", "-p", &pid.to_string()]);
    let output = probe_output(cmd, PROBE_TIMEOUT)?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    parse_etime(&stdout)
}

/// Resolve the startup-grace window: `LOOM_DAEMON_STARTUP_GRACE_SECS` overrides
/// [`DEFAULT_STARTUP_GRACE_SECS`] (env > default, matching the heartbeat
/// staleness-threshold precedence).
fn resolve_startup_grace(env_override: Option<u64>) -> u64 {
    env_override.unwrap_or(DEFAULT_STARTUP_GRACE_SECS)
}

// ============================================================================
// Heartbeat freshness
// ============================================================================

/// Compute heartbeat age (seconds) from a file's mtime, degrading to `None`
/// on any I/O error or unrepresentable timestamp rather than failing.
fn heartbeat_age_secs(path: &Path) -> Option<u64> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let elapsed = modified.elapsed().ok()?;
    Some(elapsed.as_secs())
}

/// Classify heartbeat freshness against the staleness threshold — mirrors
/// the watchdog's degrade-don't-false-report rules: an absent file or an
/// unreadable mtime both yield `Unknown`, never a false `Stale`.
///
/// `process_age_secs` (#4368) is the live process's own age, when known. A
/// heartbeat file strictly older than the process's own start time predates
/// this boot and is classified `PriorBoot` — checked *before* the staleness
/// threshold, since even a heartbeat that would otherwise look "fresh" by
/// age alone is not current-boot evidence if it is older than the process
/// itself (e.g. a very recent restart with a leftover heartbeat file from
/// seconds before it). Equal ages are deliberately NOT `PriorBoot` — a
/// heartbeat written in the same instant as process start is still
/// current-boot evidence, so it falls through to the ordinary threshold
/// check. `None` (unparseable `ps` age) makes no prior-boot claim and
/// degrades to the pre-#4368 Stale/Fresh verdicts, per the module's
/// degrade-don't-false-report rule.
fn check_heartbeat(
    heartbeat_file: &Path,
    stale_threshold_secs: u64,
    process_age_secs: Option<u64>,
) -> (HeartbeatFreshness, Option<u64>) {
    match heartbeat_age_secs(heartbeat_file) {
        Some(age) => {
            if let Some(proc_age) = process_age_secs {
                if age > proc_age {
                    return (HeartbeatFreshness::PriorBoot, Some(age));
                }
            }
            if age > stale_threshold_secs {
                (HeartbeatFreshness::Stale, Some(age))
            } else {
                (HeartbeatFreshness::Fresh, Some(age))
            }
        }
        None => (HeartbeatFreshness::Unknown, None),
    }
}

/// The watchdog's staleness threshold formula: `max(interval * 5, 300)`,
/// unless `LOOM_DAEMON_HEARTBEAT_STALE_SECS` overrides it.
fn resolve_stale_threshold(interval_secs: u64, env_override: Option<u64>) -> u64 {
    env_override.unwrap_or_else(|| (interval_secs * 5).max(300))
}

// ============================================================================
// Classification
// ============================================================================

/// Pure(ish) classification given an already-resolved loom dir, marker path,
/// and env overrides — split out from [`probe`] so tests can drive it against
/// tempdir fixtures without touching real env vars or `~/.loom`. Always uses
/// the real [`process_age_secs`] (`ps`-backed) lookup; see
/// [`classify_with_process_age_fn`] for the injectable variant tests use to
/// pin a deterministic process age.
#[must_use]
pub fn classify(loom_dir: &Path, marker_path: &Path, env: &EnvOverrides) -> InstallStateReport {
    classify_with_process_age_fn(loom_dir, marker_path, env, process_age_secs)
}

/// [`classify`]'s implementation, parameterized over the process-age lookup.
/// Production code always goes through [`classify`] (which passes the real
/// `ps`-backed [`process_age_secs`]); tests use this directly with a fixed
/// closure so heartbeat-vs-process-age comparisons (#4368) and startup-grace
/// comparisons (#4213) are deterministic — the test binary's own real uptime,
/// shared across every unit test running in this one process, is not a value
/// any single test can control.
///
/// `pub(crate)` so sibling modules' tests (e.g. `autonomy_marker`) can pin an
/// age too; there is no production caller outside [`classify`] (#4406).
pub(crate) fn classify_with_process_age_fn(
    loom_dir: &Path,
    marker_path: &Path,
    env: &EnvOverrides,
    process_age_fn: impl Fn(u32) -> Option<u64>,
) -> InstallStateReport {
    let watchdog_log_path = loom_dir.join("logs").join("daemon-watchdog.log");

    let contents = match std::fs::read_to_string(marker_path) {
        Ok(c) => c,
        Err(_) => return InstallStateReport::not_expected(watchdog_log_path),
    };
    let map = parse_marker(&contents);
    let fields = resolve_marker_fields(&map, loom_dir);

    // Env-wins-over-marker (mirrors the watchdog exactly), then the
    // non-Darwin blanket override.
    let use_launchd = env.launchd_override.unwrap_or(fields.use_launchd) && env.is_darwin;
    let label = env
        .launchd_label_override
        .clone()
        .unwrap_or(fields.launchd_label);

    let liveness = check_liveness(
        use_launchd,
        &label,
        fields.pid_file.as_deref(),
        env.launchd_domain_override.as_deref(),
    );

    if !liveness.alive {
        return InstallStateReport {
            state: InstallState::ExpectedButDead,
            started_at: fields.started_at,
            pid: None,
            liveness_detail: Some(liveness.detail),
            heartbeat_freshness: None,
            heartbeat_age_secs: None,
            heartbeat_stale_threshold_secs: None,
            process_age_secs: None,
            startup_grace_threshold_secs: None,
            watchdog_log_path,
        };
    }

    // Startup-grace (#4213): a young live process whose socket has not bound
    // yet is a normal `bootout`/`bootstrap` restart, not a fault. Process age
    // is the sole discriminator — never socket-file presence (a stale socket
    // from the prior run may still exist during startup). An undeterminable
    // age makes no grace claim and falls through to the fault/wedged verdict.
    let grace_threshold = resolve_startup_grace(env.startup_grace_secs_override);
    let process_age = liveness.pid.and_then(process_age_fn);
    if let Some(age) = process_age {
        if age <= grace_threshold {
            return InstallStateReport {
                state: InstallState::AliveStarting,
                started_at: fields.started_at,
                pid: liveness.pid,
                liveness_detail: Some(liveness.detail),
                heartbeat_freshness: None,
                heartbeat_age_secs: None,
                heartbeat_stale_threshold_secs: None,
                process_age_secs: Some(age),
                startup_grace_threshold_secs: Some(grace_threshold),
                watchdog_log_path,
            };
        }
    }

    let stale_threshold =
        resolve_stale_threshold(fields.heartbeat_interval_secs, env.heartbeat_stale_secs_override);
    let (freshness, age) = check_heartbeat(&fields.heartbeat_file, stale_threshold, process_age);

    InstallStateReport {
        state: InstallState::AliveButUnresponsive,
        started_at: fields.started_at,
        pid: liveness.pid,
        liveness_detail: Some(liveness.detail),
        heartbeat_freshness: Some(freshness),
        heartbeat_age_secs: age,
        heartbeat_stale_threshold_secs: Some(stale_threshold),
        process_age_secs: process_age,
        startup_grace_threshold_secs: Some(grace_threshold),
        watchdog_log_path,
    }
}

/// Production entry point: resolves the loom dir + marker path from env
/// (mirroring `main.rs::resolve_loom_dir` / `loom-daemon-watchdog.sh`) and
/// classifies. Returns `None` only when no loom dir can be resolved at all
/// (no `LOOM_SOCKET_PATH` and no home directory) — the caller should fall
/// back to the pre-#4069 generic message in that case. This function never
/// panics and never touches the filesystem beyond a single marker/heartbeat
/// read plus at most two read-only subprocess calls (`launchctl`, `kill -0`).
#[must_use]
pub fn probe() -> Option<InstallStateReport> {
    let loom_dir = resolve_loom_dir()?;
    let marker_path = resolve_marker_path(&loom_dir);
    Some(classify(&loom_dir, &marker_path, &EnvOverrides::from_env()))
}

// ============================================================================
// Watchdog protection state (#4354) — the REACHABLE-path sibling classification
// ============================================================================

/// Whether a *reachable* daemon is actually protected against a future death.
///
/// Two independent host-local facts feed this, both visible to the CLI process
/// with no IPC involvement:
///
/// 1. the **autonomy-desired marker** — absent ⇒ the watchdog logs
///    `[OK] … nothing to check` and crash protection is disarmed (the #4331
///    state), and
/// 2. the **watchdog job/timer** — not provisioned ⇒ nothing is scheduled to
///    ever notice a death, however armed the marker is.
///
/// Precedence when both are bad: [`ProtectionState::NoMarker`] wins for the
/// single-word verdict (it is the stronger statement — even a provisioned
/// watchdog is inert without a marker), but
/// [`ProtectionReport::watchdog_provisioned`] still carries the second fact
/// verbatim so `--json` consumers and the human detail line report **both**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectionState {
    /// Marker present AND the watchdog job/timer is provisioned.
    Protected,
    /// No autonomy-desired marker — crash protection is disarmed (#4331).
    NoMarker,
    /// Marker present, but no watchdog launchd job / systemd timer is
    /// provisioned: nothing is scheduled to detect a future death.
    WatchdogNotProvisioned,
    /// Marker present but the provisioning probe could not answer (no
    /// `launchctl`/`systemctl`, or a `systemctl --user` bus that could not be
    /// reached). A degradation, never a false verdict.
    Unknown,
}

impl ProtectionState {
    /// Machine-readable enum value for `--json` rendering.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ProtectionState::Protected => "protected",
            ProtectionState::NoMarker => "no-marker",
            ProtectionState::WatchdogNotProvisioned => "watchdog-not-provisioned",
            ProtectionState::Unknown => "unknown",
        }
    }

    /// The operator-facing phrase for the human `Protection:` line — the exact
    /// wording #4354's AC1 specifies.
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            ProtectionState::Protected => "protected",
            ProtectionState::NoMarker => "unprotected — no autonomy-desired marker",
            ProtectionState::WatchdogNotProvisioned => "watchdog job not provisioned",
            ProtectionState::Unknown => "unknown",
        }
    }
}

/// The scheduled watchdog job this host would use, resolved (name and all) the
/// same way `loom-daemon-start.sh` provisions it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchdogJob {
    /// macOS launchd job — `${LOOM_WATCHDOG_LABEL:-<daemon-label>-watchdog}`
    /// (`loom-daemon-start.sh::resolve_watchdog_label`), probed with
    /// `launchctl print <domain>/<label>`.
    Launchd { label: String },
    /// `systemd --user` timer unit —
    /// `${LOOM_WATCHDOG_LABEL:-<daemon-unit%.service>-watchdog}.timer`
    /// (`loom-daemon-start.sh::resolve_systemd_watchdog_unit`), probed with
    /// `systemctl --user is-enabled <unit>`.
    SystemdTimer { timer_unit: String },
}

impl WatchdogJob {
    /// The job's identifier as an operator would type it.
    #[must_use]
    pub fn identifier(&self) -> &str {
        match self {
            WatchdogJob::Launchd { label } => label,
            WatchdogJob::SystemdTimer { timer_unit } => timer_unit,
        }
    }

    /// Which scheduling mechanism this is (`"launchd"` / `"systemd-timer"`).
    #[must_use]
    pub fn kind_str(&self) -> &'static str {
        match self {
            WatchdogJob::Launchd { .. } => "launchd",
            WatchdogJob::SystemdTimer { .. } => "systemd-timer",
        }
    }
}

/// The full protection classification for one probe.
#[derive(Debug, Clone)]
pub struct ProtectionReport {
    pub state: ProtectionState,
    /// Whether the autonomy-desired marker file was readable.
    pub marker_present: bool,
    /// The marker path that was checked (after `LOOM_AUTONOMY_MARKER`).
    pub marker_path: PathBuf,
    /// The watchdog job whose provisioning was probed.
    pub job: WatchdogJob,
    /// `Some(true/false)` when the provisioning probe answered; `None` when it
    /// could not be run at all (degradation ⇒ [`ProtectionState::Unknown`]).
    pub watchdog_provisioned: Option<bool>,
    /// One-sentence operator-facing explanation carrying BOTH facts.
    pub detail: String,
}

/// Env overrides the protection probe honors — a **separate** struct from
/// [`EnvOverrides`] on purpose: that one encodes the unreachable path's
/// marker-vs-heartbeat precedence (#4069) and is constructed literally by
/// sibling tests, so widening it here would couple two unrelated
/// classifications. Split out so unit tests drive every branch without mutating
/// process-global env vars.
#[derive(Debug, Clone)]
pub struct ProtectionEnv {
    /// From `LOOM_WATCHDOG_LABEL` — overrides the derived watchdog job name on
    /// BOTH platforms, exactly as `loom-daemon-start.sh` does.
    pub watchdog_label_override: Option<String>,
    /// From `LOOM_LAUNCHD_LABEL` — the *daemon* label the watchdog label is
    /// derived from (`<daemon-label>-watchdog`).
    pub launchd_label_override: Option<String>,
    /// From `LOOM_SYSTEMD_UNIT` — the *daemon* unit the watchdog unit is
    /// derived from (`<unit%.service>-watchdog`).
    pub systemd_unit_override: Option<String>,
    /// From `LOOM_LAUNCHD_DOMAIN` — the launchd domain to probe in, mirroring
    /// `lib/launchd-domain.sh::resolve_launchd_domain`'s override.
    pub launchd_domain_override: Option<String>,
    /// From `LOOM_DAEMON_LAUNCHD` — see [`parse_launchd_override`]; can only
    /// ever force launchd *off*.
    pub launchd_override: Option<bool>,
    /// Whether this host can have a launchd job at all.
    pub is_darwin: bool,
}

impl ProtectionEnv {
    /// Resolve from the real process environment + platform.
    #[must_use]
    pub fn from_env() -> Self {
        let non_empty = |key: &str| std::env::var(key).ok().filter(|s| !s.is_empty());
        ProtectionEnv {
            watchdog_label_override: non_empty("LOOM_WATCHDOG_LABEL"),
            launchd_label_override: non_empty("LOOM_LAUNCHD_LABEL"),
            systemd_unit_override: non_empty("LOOM_SYSTEMD_UNIT"),
            launchd_domain_override: non_empty("LOOM_LAUNCHD_DOMAIN"),
            launchd_override: parse_launchd_override(
                std::env::var("LOOM_DAEMON_LAUNCHD").ok().as_deref(),
            ),
            is_darwin: cfg!(target_os = "macos"),
        }
    }
}

/// Default `systemd --user` daemon unit, matching
/// `lib/systemd-user.sh::resolve_systemd_unit`'s fallback.
pub const DEFAULT_SYSTEMD_UNIT: &str = "loom-daemon.service";

/// Resolve which watchdog job this host schedules, mirroring
/// `loom-daemon-start.sh` exactly:
///
/// - launchd (`use_launchd`): `${LOOM_WATCHDOG_LABEL:-<daemon-label>-watchdog}`
///   where the daemon label is `${LOOM_LAUNCHD_LABEL:-<marker label>}`
///   (`resolve_watchdog_label`, `loom-daemon-start.sh:511`).
/// - systemd otherwise: `${LOOM_WATCHDOG_LABEL:-<daemon-unit%.service>-watchdog}`
///   plus the `.timer` suffix the timer unit carries
///   (`resolve_systemd_watchdog_unit`, `loom-daemon-start.sh:596`).
///
/// `use_launchd` is already the env-and-platform-resolved value (a non-Darwin
/// host is always `false`), so a Linux host never resolves — or probes — a
/// launchd job, and `launchctl`'s absence there is never even consulted.
#[must_use]
pub fn resolve_watchdog_job(
    use_launchd: bool,
    daemon_launchd_label: &str,
    env: &ProtectionEnv,
) -> WatchdogJob {
    if use_launchd {
        let label = env
            .watchdog_label_override
            .clone()
            .unwrap_or_else(|| format!("{daemon_launchd_label}-watchdog"));
        return WatchdogJob::Launchd { label };
    }
    let daemon_unit = env
        .systemd_unit_override
        .clone()
        .unwrap_or_else(|| DEFAULT_SYSTEMD_UNIT.to_string());
    let base = env
        .watchdog_label_override
        .clone()
        .unwrap_or_else(|| format!("{}-watchdog", daemon_unit.trim_end_matches(".service")));
    WatchdogJob::SystemdTimer {
        timer_unit: format!("{base}.timer"),
    }
}

/// [`resolve_launchd_domain_detailed`]'s result, carrying enough detail for
/// callers to cross-check a negative verdict against the domain that domain
/// resolution *skipped* (#4694).
///
/// The `gui/<uid>` → `user/<uid>` fallback itself is intentional (#4130,
/// headless-SSH support) — this struct does not change which domain is
/// *primary*, it only lets a caller know when a second, skipped domain
/// exists and is worth a cross-check before declaring a negative (dead /
/// not-loaded / not-provisioned) verdict: the single reachability probe used
/// to decide whether `gui/<uid>` is usable cannot distinguish "genuinely
/// unreachable" from "a transient hang/flake within `PROBE_TIMEOUT`" —
/// folding a flaky probe into a permanent domain choice for the rest of the
/// call previously produced false negatives (#4694).
struct DomainResolution {
    /// The primary domain to probe. `None` only when the uid itself is
    /// undeterminable.
    domain: Option<String>,
    /// The `gui/<uid>` domain that was skipped because its reachability probe
    /// came back non-success, when that is why `domain` is `user/<uid>`.
    /// `Some` only in that exact case — never when an explicit
    /// `LOOM_LAUNCHD_DOMAIN` override was honored (AC6: no cross-check
    /// fallback for an explicit override) and never when `gui/<uid>` was
    /// itself the resolved domain (nothing was skipped).
    fallback_check_domain: Option<String>,
}

/// Resolve the launchd domain to probe in, mirroring
/// `lib/launchd-domain.sh::resolve_launchd_domain`: an explicit
/// `LOOM_LAUNCHD_DOMAIN` wins, else `gui/<uid>` when that domain resolves, else
/// the SSH-reachable background `user/<uid>` domain. `None` only when the uid
/// itself is undeterminable (⇒ the caller degrades to `Unknown`). The result
/// also carries the skipped-domain detail described in [`DomainResolution`],
/// for callers that need to cross-check a negative verdict (#4694) — every
/// caller in this module uses that detail, so there is no separate
/// domain-only accessor.
fn resolve_launchd_domain_detailed(override_value: Option<&str>) -> DomainResolution {
    if let Some(explicit) = override_value.filter(|s| !s.is_empty()) {
        return DomainResolution {
            domain: Some(explicit.to_string()),
            fallback_check_domain: None,
        };
    }
    let Some(uid) = current_uid() else {
        return DomainResolution {
            domain: None,
            fallback_check_domain: None,
        };
    };
    let gui = format!("gui/{uid}");
    let mut cmd = Command::new("launchctl");
    cmd.args(["print", &gui]);
    // A hung reachability probe reads as "gui/<uid> not reachable" — the same
    // verdict an absent/nonzero `launchctl` gives — so the caller falls back to
    // the SSH-reachable `user/<uid>` domain rather than blocking (#4548). That
    // failure is exactly the ambiguous case #4694 cares about: it may be a
    // genuine absence, or it may be a transient flake — either way `gui` is
    // reported back as the domain worth cross-checking before a caller trusts
    // a negative verdict from `user/<uid>` alone.
    let gui_ok = probe_output(cmd, PROBE_TIMEOUT).is_some_and(|o| o.status.success());
    if gui_ok {
        return DomainResolution {
            domain: Some(gui),
            fallback_check_domain: None,
        };
    }
    DomainResolution {
        domain: Some(format!("user/{uid}")),
        fallback_check_domain: Some(gui),
    }
}

/// Probe whether the launchd job `<domain>/<label>` is loaded —
/// `launchctl print <domain>/<label>` exits 0 only for a bootstrapped job, so
/// a nonzero exit is a real (for *this domain*) "not loaded". A
/// missing/unspawnable `launchctl` yields `None` — unknown, never a false
/// negative. A probe that hangs past [`PROBE_TIMEOUT`] takes that same `None`
/// path (#4548). Shared by [`launchctl_job_provisioned`]'s primary and
/// cross-check probes (#4694) — both are this exact same call against
/// different domains.
fn probe_domain_provisioned(domain: &str, label: &str) -> Option<bool> {
    let mut cmd = Command::new("launchctl");
    cmd.args(["print", &format!("{domain}/{label}")]);
    let output = probe_output(cmd, PROBE_TIMEOUT)?;
    Some(output.status.success())
}

/// Is the watchdog launchd job loaded? A missing/unspawnable `launchctl` (or
/// an undeterminable domain) yields `None` — unknown, never a false negative.
///
/// #4694: a negative (or unknown) primary-domain verdict is not trusted on
/// its own when [`resolve_launchd_domain_detailed`] reports a
/// `fallback_check_domain` — i.e. when domain resolution fell back to
/// `user/<uid>` because its `gui/<uid>` reachability probe failed, which
/// cannot distinguish a genuine absence from a transient flake. In that case
/// this also probes the skipped `gui/<uid>` domain and only reports
/// not-provisioned (`Some(false)`) when BOTH domains agree; any timeout/error
/// on either probe, or a disagreement other than "either domain says loaded",
/// degrades to `None` rather than a confident negative (never folds "unknown"
/// into `Some(false)`). No cross-check occurs when an explicit
/// `LOOM_LAUNCHD_DOMAIN` override was honored (AC6) — that always resolves
/// with `fallback_check_domain: None`.
fn launchctl_job_provisioned(label: &str, domain_override: Option<&str>) -> Option<bool> {
    let resolution = resolve_launchd_domain_detailed(domain_override);
    let domain = resolution.domain?;
    let primary = probe_domain_provisioned(&domain, label);
    if primary == Some(true) {
        return primary;
    }
    let Some(check_domain) = resolution.fallback_check_domain.as_deref() else {
        return primary;
    };
    let secondary = probe_domain_provisioned(check_domain, label);
    match (primary, secondary) {
        (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

/// Is the watchdog systemd timer provisioned? `systemctl --user is-enabled
/// <unit>` prints its verdict on stdout: `enabled` / `enabled-runtime` ⇒
/// provisioned; any other verdict (`disabled`, `static`, `masked`, `not-found`,
/// …) ⇒ NOT provisioned, which deliberately collapses "present but disabled"
/// and "absent" into the same operator-relevant answer — neither will fire.
///
/// No stdout verdict at all is ambiguous, so it is disambiguated from stderr: a
/// "no such file" complaint is a genuine absence (`Some(false)`), while anything
/// else — most importantly `Failed to connect to bus` on a host with no user
/// manager — is `None` (unknown), never a false "not provisioned".
///
/// Bounded by [`PROBE_TIMEOUT`] (#4548): a wedged `systemd --user` bus can make
/// `systemctl` block on its D-Bus connect instead of failing fast, and that
/// hang degrades to the same `None` (unknown) an unspawnable `systemctl` gives.
fn systemd_timer_provisioned(timer_unit: &str) -> Option<bool> {
    let mut cmd = Command::new("systemctl");
    cmd.args(["--user", "is-enabled", timer_unit]);
    let output = probe_output(cmd, PROBE_TIMEOUT)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    match stdout.lines().map(str::trim).find(|l| !l.is_empty()) {
        Some("enabled" | "enabled-runtime") => Some(true),
        Some(_) => Some(false),
        None => {
            let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
            if stderr.contains("no such file") || stderr.contains("not found") {
                Some(false)
            } else {
                None
            }
        }
    }
}

/// The real provisioning probe: dispatch to the mechanism the resolved job
/// names. Read-only (`launchctl print` / `systemctl is-enabled` are both
/// queries) and never fails — every failure mode degrades to `None`.
fn probe_watchdog_provisioned(job: &WatchdogJob, domain_override: Option<&str>) -> Option<bool> {
    match job {
        WatchdogJob::Launchd { label } => launchctl_job_provisioned(label, domain_override),
        WatchdogJob::SystemdTimer { timer_unit } => systemd_timer_provisioned(timer_unit),
    }
}

/// Classify protection from an already-resolved loom dir + marker path, with the
/// provisioning check **injected** — the same testability seam [`classify`] uses
/// for process age, so unit tests pin every (marker × provisioning) combination
/// without a live launchd/systemd on the host.
#[must_use]
pub fn classify_protection(
    loom_dir: &Path,
    marker_path: &Path,
    env: &ProtectionEnv,
    // `FnMut` (not `Fn`) so a test's injected probe can record which job it was
    // asked about; production passes a plain closure and it is called exactly
    // once either way.
    mut provisioned_fn: impl FnMut(&WatchdogJob) -> Option<bool>,
) -> ProtectionReport {
    // Marker read is the ONLY filesystem touch, and its failure (absent or
    // unreadable) is a first-class verdict, never an error.
    let contents = std::fs::read_to_string(marker_path).ok();
    let marker_present = contents.is_some();
    let fields = contents
        .as_deref()
        .map(|c| resolve_marker_fields(&parse_marker(c), loom_dir));

    // Which supervisor owns the watchdog job — same env-wins-over-marker rule
    // plus the non-Darwin blanket override the unreachable path applies. With no
    // marker to consult, fall back to the platform default (launchd on Darwin).
    let marker_use_launchd = fields.as_ref().map_or(env.is_darwin, |f| f.use_launchd);
    let use_launchd = env.launchd_override.unwrap_or(marker_use_launchd) && env.is_darwin;
    let daemon_label = env
        .launchd_label_override
        .clone()
        .or_else(|| fields.as_ref().map(|f| f.launchd_label.clone()))
        .unwrap_or_else(|| DEFAULT_LAUNCHD_LABEL.to_string());

    let job = resolve_watchdog_job(use_launchd, &daemon_label, env);
    let watchdog_provisioned = provisioned_fn(&job);

    let kind = job.kind_str();
    let id = job.identifier().to_string();
    let marker_display = marker_path.display();

    let (state, detail) = match (marker_present, watchdog_provisioned) {
        // No marker: the strongest verdict, but still report the watchdog fact.
        (false, provisioned) => {
            let watchdog_note = match provisioned {
                Some(true) => {
                    format!("the watchdog {kind} job {id} IS provisioned but has nothing to check")
                }
                Some(false) => {
                    format!("the watchdog {kind} job {id} is not provisioned either")
                }
                None => format!("watchdog {kind} job {id} provisioning is undeterminable"),
            };
            (
                ProtectionState::NoMarker,
                format!(
                    "no autonomy-desired marker at {marker_display} — crash protection is \
                     DISARMED; {watchdog_note}"
                ),
            )
        }
        (true, Some(true)) => (
            ProtectionState::Protected,
            format!(
                "autonomy-desired marker present at {marker_display}; watchdog {kind} job {id} \
                 is provisioned"
            ),
        ),
        (true, Some(false)) => (
            ProtectionState::WatchdogNotProvisioned,
            format!(
                "autonomy-desired marker present at {marker_display}, but the watchdog {kind} \
                 job {id} is not provisioned — nothing is scheduled to detect a future daemon \
                 death"
            ),
        ),
        (true, None) => (
            ProtectionState::Unknown,
            format!(
                "autonomy-desired marker present at {marker_display}, but the watchdog {kind} \
                 job {id} could not be probed on this host"
            ),
        ),
    };

    ProtectionReport {
        state,
        marker_present,
        marker_path: marker_path.to_path_buf(),
        job,
        watchdog_provisioned,
        detail,
    }
}

/// Production entry point for the reachable path (#4354): resolve the loom dir +
/// marker path from env (via [`crate::autonomy_marker::resolve_marker_path`], so
/// `LOOM_AUTONOMY_MARKER` is honored identically to the watchdog and the startup
/// healer) and classify with the real launchd/systemd provisioning probe.
///
/// Returns `None` only when no loom dir can be resolved at all — the caller then
/// prints nothing rather than guessing. Read-only and infallible by
/// construction: at most three query-only subprocess calls, no writes, and no
/// path that can fail `loom-daemon status` (the reachable path still exits 0
/// whatever this reports).
#[must_use]
pub fn probe_protection() -> Option<ProtectionReport> {
    let loom_dir = resolve_loom_dir()?;
    let marker_path = resolve_marker_path(&loom_dir);
    let env = ProtectionEnv::from_env();
    let domain_override = env.launchd_domain_override.clone();
    Some(classify_protection(&loom_dir, &marker_path, &env, |job| {
        probe_watchdog_provisioned(job, domain_override.as_deref())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;
    use std::time::{Duration, Instant, SystemTime};

    fn no_env_overrides() -> EnvOverrides {
        // `is_darwin: false` forces the pid-file path deterministically in
        // tests regardless of the host OS or a real `launchctl` — the marker
        // fixtures below always set `use_launchd=false` too, so this is
        // belt-and-suspenders, not load-bearing.
        EnvOverrides {
            launchd_override: None,
            launchd_label_override: None,
            launchd_domain_override: None,
            heartbeat_stale_secs_override: None,
            // A zero grace window is the *default* here so most tests reach the
            // post-grace verdicts. It is NOT sufficient on its own: a real
            // process under one second old reports `ps -o etime=` as `00:00`,
            // which parses to 0 and satisfies `age <= grace` — so a test that
            // asserts a post-grace verdict against a real `ps`-backed age
            // flakes to `AliveStarting` whenever the test binary is still in
            // its first wall-clock second (#4406). Any test that wants a
            // post-grace verdict must ALSO pin a synthetic process age via
            // `classify_with_process_age_fn`. Tests that want the
            // startup-grace path set a huge window explicitly.
            startup_grace_secs_override: Some(0),
            is_darwin: false,
        }
    }

    fn write_pid_file(dir: &Path, pid: u32) -> PathBuf {
        let path = dir.join("daemon.pid");
        fs::write(&path, pid.to_string()).unwrap();
        path
    }

    fn write_marker(dir: &Path, extra: &str) -> PathBuf {
        let path = dir.join(MARKER_FILENAME);
        fs::write(&path, extra).unwrap();
        path
    }

    fn write_heartbeat(dir: &Path, age: Duration) -> PathBuf {
        let path = dir.join("daemon.heartbeat");
        fs::write(&path, "1 pid=1 ts=x\n").unwrap();
        let mtime = SystemTime::now() - age;
        let file = fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(mtime).unwrap();
        path
    }

    #[test]
    fn marker_absent_is_not_expected() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(MARKER_FILENAME);
        let report = classify(dir.path(), &marker, &no_env_overrides());
        assert_eq!(report.state, InstallState::NotExpected);
        assert_eq!(report.state.exit_code(), EXIT_NOT_EXPECTED);
    }

    #[test]
    fn marker_present_live_pid_is_alive_but_unresponsive() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = write_pid_file(dir.path(), std::process::id());
        let marker = write_marker(
            dir.path(),
            &format!(
                "started_at=2026-07-27T00:00:00Z\npid_file={}\nuse_launchd=false\n",
                pid_file.display()
            ),
        );
        // Pin a synthetic process age well beyond the zero grace window
        // (#4406) — a real `ps` age of `00:00` for a sub-second-old test
        // binary would otherwise satisfy `age <= grace` and flake to
        // AliveStarting.
        let report = classify_with_process_age_fn(dir.path(), &marker, &no_env_overrides(), |_| {
            Some(1_000_000)
        });
        assert_eq!(report.state, InstallState::AliveButUnresponsive);
        assert_eq!(report.state.exit_code(), EXIT_ALIVE_BUT_UNRESPONSIVE);
        assert_eq!(report.pid, Some(std::process::id()));
        assert_eq!(report.started_at.as_deref(), Some("2026-07-27T00:00:00Z"));
    }

    #[test]
    fn marker_present_dead_pid_is_expected_but_dead() {
        let dir = tempfile::tempdir().unwrap();
        // Pid 0 is never `kill -0`-able as a normal user process here in the
        // sense the shell script cares about; use a pid that is extremely
        // unlikely to be alive instead of relying on 0's special meaning.
        let dead_pid: u32 = 999_999;
        let pid_file = write_pid_file(dir.path(), dead_pid);
        let marker = write_marker(
            dir.path(),
            &format!("pid_file={}\nuse_launchd=false\n", pid_file.display()),
        );
        let report = classify(dir.path(), &marker, &no_env_overrides());
        assert_eq!(report.state, InstallState::ExpectedButDead);
        assert_eq!(report.state.exit_code(), EXIT_EXPECTED_BUT_DEAD);
        assert!(report.pid.is_none());
    }

    #[test]
    fn marker_present_no_pid_file_is_expected_but_dead() {
        let dir = tempfile::tempdir().unwrap();
        let marker = write_marker(dir.path(), "use_launchd=false\n");
        let report = classify(dir.path(), &marker, &no_env_overrides());
        assert_eq!(report.state, InstallState::ExpectedButDead);
    }

    #[test]
    fn alive_with_fresh_heartbeat_is_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = write_pid_file(dir.path(), std::process::id());
        let heartbeat = write_heartbeat(dir.path(), Duration::from_secs(5));
        let marker = write_marker(
            dir.path(),
            &format!(
                "pid_file={}\nheartbeat_file={}\nheartbeat_interval_secs=60\nuse_launchd=false\n",
                pid_file.display(),
                heartbeat.display()
            ),
        );
        // Pin a large synthetic process age (#4368) so this test's intent —
        // "a fresh heartbeat classifies as Fresh" — cannot be perturbed by
        // the test binary's own real (and much smaller, at least early in a
        // run) uptime being compared against the 5s heartbeat age.
        let report = classify_with_process_age_fn(dir.path(), &marker, &no_env_overrides(), |_| {
            Some(1_000_000)
        });
        assert_eq!(report.state, InstallState::AliveButUnresponsive);
        assert_eq!(report.heartbeat_freshness, Some(HeartbeatFreshness::Fresh));
        assert_eq!(report.heartbeat_stale_threshold_secs, Some(300));
    }

    #[test]
    fn alive_with_stale_heartbeat_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = write_pid_file(dir.path(), std::process::id());
        let heartbeat = write_heartbeat(dir.path(), Duration::from_secs(1000));
        let marker = write_marker(
            dir.path(),
            &format!(
                "pid_file={}\nheartbeat_file={}\nheartbeat_interval_secs=60\nuse_launchd=false\n",
                pid_file.display(),
                heartbeat.display()
            ),
        );
        // Pin a synthetic process age comfortably larger than the 1000s
        // heartbeat age (#4368) — without this, the real (tiny) test-binary
        // uptime would make this heartbeat look older than the process and
        // misclassify as PriorBoot instead of exercising the Stale verdict
        // this test targets.
        let report = classify_with_process_age_fn(dir.path(), &marker, &no_env_overrides(), |_| {
            Some(1_000_000)
        });
        assert_eq!(report.state, InstallState::AliveButUnresponsive);
        assert_eq!(report.heartbeat_freshness, Some(HeartbeatFreshness::Stale));
    }

    #[test]
    fn stale_threshold_honors_env_override() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = write_pid_file(dir.path(), std::process::id());
        let heartbeat = write_heartbeat(dir.path(), Duration::from_secs(20));
        let marker = write_marker(
            dir.path(),
            &format!(
                "pid_file={}\nheartbeat_file={}\nheartbeat_interval_secs=60\nuse_launchd=false\n",
                pid_file.display(),
                heartbeat.display()
            ),
        );
        let mut env = no_env_overrides();
        env.heartbeat_stale_secs_override = Some(10);
        // See the #4368 note above — pin process age so the 20s heartbeat
        // age is never mistaken for a prior-boot file.
        let report = classify_with_process_age_fn(dir.path(), &marker, &env, |_| Some(1_000_000));
        assert_eq!(report.heartbeat_freshness, Some(HeartbeatFreshness::Stale));
        assert_eq!(report.heartbeat_stale_threshold_secs, Some(10));
    }

    #[test]
    fn malformed_marker_degrades_to_not_expected_semantics_gracefully() {
        // A marker file that exists but has no recognizable keys at all still
        // parses (empty map); fallbacks apply, no field lookups panic. Since
        // there's no pid_file, this resolves to ExpectedButDead — the "marker
        // present" branch never panics on garbage content.
        let dir = tempfile::tempdir().unwrap();
        let marker = write_marker(dir.path(), "not a key value file at all\n\n# comment\n");
        let report = classify(dir.path(), &marker, &no_env_overrides());
        assert_eq!(report.state, InstallState::ExpectedButDead);
    }

    #[test]
    fn marker_missing_fields_uses_fallback_paths() {
        let dir = tempfile::tempdir().unwrap();
        // No heartbeat_file / heartbeat_interval_secs / launchd_label fields —
        // exercise the per-field fallback path (marker predates those fields).
        let pid_file = write_pid_file(dir.path(), std::process::id());
        let marker = write_marker(
            dir.path(),
            &format!("pid_file={}\nuse_launchd=false\n", pid_file.display()),
        );
        // Pinned synthetic age (#4406, see `marker_present_live_pid_...`) so
        // the zero grace window is genuinely cleared.
        let report = classify_with_process_age_fn(dir.path(), &marker, &no_env_overrides(), |_| {
            Some(1_000_000)
        });
        assert_eq!(report.state, InstallState::AliveButUnresponsive);
        // No heartbeat file was ever written at the fallback location, so the
        // qualifier degrades to Unknown rather than falsely reporting Stale.
        assert_eq!(report.heartbeat_freshness, Some(HeartbeatFreshness::Unknown));
    }

    #[test]
    fn pid_file_with_unowned_or_stale_pid_is_expected_but_dead() {
        let dir = tempfile::tempdir().unwrap();
        // pid 1 is (almost) never owned by the test process and typically
        // cannot be `kill -0`-ed without privilege — exercises the "present
        // pid, not killable by us" branch the same way a stale/unowned pid
        // does in the shell script.
        let pid_file = write_pid_file(dir.path(), 1);
        let marker = write_marker(
            dir.path(),
            &format!("pid_file={}\nuse_launchd=false\n", pid_file.display()),
        );
        let report = classify(dir.path(), &marker, &no_env_overrides());
        assert_eq!(report.state, InstallState::ExpectedButDead);
    }

    #[test]
    fn heartbeat_absent_degrades_to_unknown_not_stale() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = write_pid_file(dir.path(), std::process::id());
        let marker = write_marker(
            dir.path(),
            &format!(
                "pid_file={}\nheartbeat_file={}\nuse_launchd=false\n",
                pid_file.display(),
                dir.path().join("no-such-heartbeat").display()
            ),
        );
        // Pinned synthetic age (#4406, see `marker_present_live_pid_...`) so
        // the zero grace window is genuinely cleared.
        let report = classify_with_process_age_fn(dir.path(), &marker, &no_env_overrides(), |_| {
            Some(1_000_000)
        });
        assert_eq!(report.state, InstallState::AliveButUnresponsive);
        assert_eq!(report.heartbeat_freshness, Some(HeartbeatFreshness::Unknown));
        assert!(report.heartbeat_age_secs.is_none());
    }

    #[test]
    fn env_launchd_override_forces_pid_file_path_even_when_marker_says_launchd() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = write_pid_file(dir.path(), std::process::id());
        // Marker claims launchd, but env override forces it off — exercises
        // "env wins over marker".
        let marker = write_marker(
            dir.path(),
            &format!("pid_file={}\nuse_launchd=true\n", pid_file.display()),
        );
        let mut env = no_env_overrides();
        env.launchd_override = Some(false);
        // Pinned synthetic age (#4406, see `marker_present_live_pid_...`) so
        // the zero grace window is genuinely cleared.
        let report = classify_with_process_age_fn(dir.path(), &marker, &env, |_| Some(1_000_000));
        assert_eq!(report.state, InstallState::AliveButUnresponsive);
    }

    // ===================================================================
    // launchctl_pid domain resolution (#4536): the unreachable-path launchd
    // probe must use the same `resolve_launchd_domain_detailed` fallback rule
    // (explicit `LOOM_LAUNCHD_DOMAIN` override -> `gui/<uid>` -> `user/<uid>`)
    // the reachable-path protection probe uses (#4354/#4533), instead of a
    // hardcoded `gui/<uid>`.
    //
    // `launchctl_pid_domain_override_is_honored_verbatim` passes an explicit
    // override, so `resolve_launchd_domain_detailed` returns before ever
    // invoking `launchctl` — it is genuinely independent of whether the host
    // has a real `launchctl`, so it needs no stubbing.
    //
    // `launchctl_pid_falls_back_to_gui_uid_then_user_uid_when_no_override`
    // asserts on the no-override fallback path, which *does* call
    // `launchctl`. It used to rely on CI's `ubuntu-latest` lacking a real
    // `launchctl` to force that fallback; on a macOS host with a reachable
    // `gui/<uid>` login session, the real probe would succeed and resolve to
    // `gui/<uid>` instead, breaking the assertion. It now stubs `launchctl`
    // to always fail (#4724), making the `user/<uid>` fallback branch (and,
    // since #4694, its `gui/<uid>` cross-check, which fails identically)
    // deterministic regardless of the host.
    // ===================================================================

    #[test]
    fn launchctl_pid_domain_override_is_honored_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let marker = write_marker(dir.path(), "use_launchd=true\n");
        let mut env = no_env_overrides();
        // is_darwin: true so `use_launchd` actually selects the launchd probe
        // path in `check_liveness`, exercising `launchctl_pid`'s domain
        // resolution rather than the pid-file fallback.
        env.is_darwin = true;
        env.launchd_domain_override = Some("custom/999".to_string());
        let report = classify_with_process_age_fn(dir.path(), &marker, &env, |_| Some(1_000_000));
        // An explicit override short-circuits before `launchctl` is ever
        // invoked, so this assertion holds regardless of whether the host has
        // a real `launchctl` — but the detail string must reflect the
        // *override* domain verbatim, not a hardcoded `gui/<uid>`.
        let detail = report.liveness_detail.expect("liveness_detail set");
        assert!(
            detail.contains(&format!("custom/999/{DEFAULT_LAUNCHD_LABEL}")),
            "expected override domain in detail, got: {detail}"
        );
        assert!(!detail.contains("gui/"), "detail should not fall back to gui/<uid>: {detail}");
    }

    #[test]
    #[serial]
    fn launchctl_pid_falls_back_to_gui_uid_then_user_uid_when_no_override() {
        // #4724: stub `launchctl` to always fail rather than relying on it
        // being absent from the host's PATH. On CI's `ubuntu-latest`
        // `launchctl` genuinely doesn't exist, but on a macOS dev host with a
        // reachable `gui/<uid>` login session, the real `gui/<uid>`
        // reachability probe inside `resolve_launchd_domain_detailed` would
        // succeed and this test's `user/<uid>` fallback assertion would fail.
        let stub_dir = tempfile::tempdir().unwrap();
        write_stub(stub_dir.path(), "launchctl", FAIL_STUB);

        let dir = tempfile::tempdir().unwrap();
        let marker = write_marker(dir.path(), "use_launchd=true\n");
        let mut env = no_env_overrides();
        env.is_darwin = true;
        env.launchd_domain_override = None;
        let report = with_path_prefix(stub_dir.path(), || {
            classify_with_process_age_fn(dir.path(), &marker, &env, |_| Some(1_000_000))
        });
        let detail = report.liveness_detail.expect("liveness_detail set");
        // No `LOOM_LAUNCHD_DOMAIN` override: `resolve_launchd_domain_detailed`
        // tries `gui/<uid>` first, but the stubbed `launchctl` always fails,
        // so that probe fails and it falls through to `user/<uid>` — the same
        // fallback order `resolve_launchd_domain_detailed` documents. The
        // #4694 `gui/<uid>` cross-check fails on the same stubbed `launchctl`,
        // so the reported detail stays the `user/<uid>` domain.
        let uid = current_uid().expect("current_uid resolves in test env");
        assert!(
            detail.contains(&format!("user/{uid}/{DEFAULT_LAUNCHD_LABEL}")),
            "expected user/<uid> fallback domain in detail, got: {detail}"
        );
    }

    #[test]
    fn non_darwin_forces_pid_file_path_regardless_of_marker() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = write_pid_file(dir.path(), std::process::id());
        let marker = write_marker(
            dir.path(),
            &format!("pid_file={}\nuse_launchd=true\n", pid_file.display()),
        );
        // is_darwin: false with no explicit launchd_override — the blanket
        // non-Darwin rule must still force the pid-file path. Pinned synthetic
        // age (#4406, see `marker_present_live_pid_...`) so the zero grace
        // window is genuinely cleared.
        let report = classify_with_process_age_fn(dir.path(), &marker, &no_env_overrides(), |_| {
            Some(1_000_000)
        });
        assert_eq!(report.state, InstallState::AliveButUnresponsive);
    }

    #[test]
    fn parse_marker_first_occurrence_wins() {
        let map = parse_marker("key=first\nkey=second\n# comment\n\nother=val\n");
        assert_eq!(map.get("key").map(String::as_str), Some("first"));
        assert_eq!(map.get("other").map(String::as_str), Some("val"));
    }

    #[test]
    fn resolve_stale_threshold_matches_watchdog_formula() {
        assert_eq!(resolve_stale_threshold(60, None), 300);
        assert_eq!(resolve_stale_threshold(120, None), 600);
        assert_eq!(resolve_stale_threshold(60, Some(45)), 45);
    }

    #[test]
    fn install_state_as_str_matches_taxonomy() {
        assert_eq!(InstallState::NotExpected.as_str(), "not-expected");
        assert_eq!(InstallState::ExpectedButDead.as_str(), "expected-but-dead");
        assert_eq!(InstallState::AliveStarting.as_str(), "alive-starting");
        assert_eq!(InstallState::AliveButUnresponsive.as_str(), "alive-but-unresponsive");
    }

    #[test]
    fn heartbeat_freshness_as_str_matches_taxonomy() {
        assert_eq!(HeartbeatFreshness::Fresh.as_str(), "fresh");
        assert_eq!(HeartbeatFreshness::Stale.as_str(), "stale");
        assert_eq!(HeartbeatFreshness::Unknown.as_str(), "unknown");
        assert_eq!(HeartbeatFreshness::PriorBoot.as_str(), "prior-boot");
    }

    // ===================================================================
    // Prior-boot heartbeat detection (#4368)
    // ===================================================================

    #[test]
    fn check_heartbeat_prior_boot_when_older_than_process() {
        let dir = tempfile::tempdir().unwrap();
        let heartbeat = write_heartbeat(dir.path(), Duration::from_secs(1000));
        let (freshness, age) = check_heartbeat(&heartbeat, 300, Some(10));
        assert_eq!(freshness, HeartbeatFreshness::PriorBoot);
        assert_eq!(age, Some(1000));
    }

    #[test]
    fn check_heartbeat_current_boot_stale_when_younger_than_process() {
        // Heartbeat is well past the staleness threshold, but it is younger
        // than the process itself — this IS current-boot evidence, so the
        // real-wedge verdict (Stale) must be preserved, never PriorBoot.
        let dir = tempfile::tempdir().unwrap();
        let heartbeat = write_heartbeat(dir.path(), Duration::from_secs(1000));
        let (freshness, age) = check_heartbeat(&heartbeat, 300, Some(5000));
        assert_eq!(freshness, HeartbeatFreshness::Stale);
        assert_eq!(age, Some(1000));
    }

    #[test]
    fn check_heartbeat_no_prior_boot_claim_when_process_age_unknown() {
        // Unparseable `ps` age (`None`) must never manufacture a prior-boot
        // claim — degrade to the pre-#4368 Stale/Fresh verdicts instead.
        let dir = tempfile::tempdir().unwrap();
        let heartbeat = write_heartbeat(dir.path(), Duration::from_secs(1000));
        let (freshness, age) = check_heartbeat(&heartbeat, 300, None);
        assert_eq!(freshness, HeartbeatFreshness::Stale);
        assert_eq!(age, Some(1000));
    }

    #[test]
    fn check_heartbeat_boundary_equal_ages_is_not_prior_boot() {
        // A heartbeat exactly as old as the process itself is deliberately
        // NOT prior-boot (a strictly-older file is) — it is current-boot
        // evidence and falls through to the ordinary threshold check.
        let dir = tempfile::tempdir().unwrap();
        let heartbeat = write_heartbeat(dir.path(), Duration::from_secs(500));
        let (freshness, age) = check_heartbeat(&heartbeat, 300, Some(500));
        assert_eq!(freshness, HeartbeatFreshness::Stale);
        assert_eq!(age, Some(500));
    }

    #[test]
    fn heartbeat_older_than_process_start_is_prior_boot_end_to_end() {
        // End-to-end wiring proof (via classify_with_process_age_fn, so the
        // injected process age is deterministic): a heartbeat file from
        // 83814s ago — the exact age observed in the #4368 incident — with a
        // young (9s) process must classify PriorBoot, never Stale, and must
        // NOT be mistaken for the startup-grace path either (grace defaults
        // to 0 in `no_env_overrides`, so the 9s process falls through to the
        // AliveButUnresponsive branch as intended).
        let dir = tempfile::tempdir().unwrap();
        let pid_file = write_pid_file(dir.path(), std::process::id());
        let heartbeat = write_heartbeat(dir.path(), Duration::from_secs(83_814));
        let marker = write_marker(
            dir.path(),
            &format!(
                "pid_file={}\nheartbeat_file={}\nheartbeat_interval_secs=60\nuse_launchd=false\n",
                pid_file.display(),
                heartbeat.display()
            ),
        );
        let report =
            classify_with_process_age_fn(dir.path(), &marker, &no_env_overrides(), |_| Some(9));
        assert_eq!(report.state, InstallState::AliveButUnresponsive);
        assert_eq!(report.heartbeat_freshness, Some(HeartbeatFreshness::PriorBoot));
        assert_eq!(report.heartbeat_age_secs, Some(83_814));
        assert_eq!(report.process_age_secs, Some(9));
    }

    #[test]
    fn stale_heartbeat_with_unknown_process_age_makes_no_prior_boot_claim_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = write_pid_file(dir.path(), std::process::id());
        let heartbeat = write_heartbeat(dir.path(), Duration::from_secs(1000));
        let marker = write_marker(
            dir.path(),
            &format!(
                "pid_file={}\nheartbeat_file={}\nheartbeat_interval_secs=60\nuse_launchd=false\n",
                pid_file.display(),
                heartbeat.display()
            ),
        );
        let report =
            classify_with_process_age_fn(dir.path(), &marker, &no_env_overrides(), |_| None);
        assert_eq!(report.state, InstallState::AliveButUnresponsive);
        assert_eq!(report.heartbeat_freshness, Some(HeartbeatFreshness::Stale));
        assert!(report.process_age_secs.is_none());
    }

    #[test]
    fn alive_starting_reuses_alive_but_unresponsive_exit_code() {
        assert_eq!(InstallState::AliveStarting.exit_code(), EXIT_ALIVE_BUT_UNRESPONSIVE);
        assert_eq!(InstallState::AliveStarting.exit_code(), 4);
    }

    #[test]
    fn parse_etime_covers_all_ps_shapes_and_garbage() {
        // `ss` — macOS has no seconds-only keyword, but accept it defensively.
        assert_eq!(parse_etime("05"), Some(5));
        // `mm:ss`
        assert_eq!(parse_etime("01:30"), Some(90));
        assert_eq!(parse_etime("00:45"), Some(45));
        // `hh:mm:ss`
        assert_eq!(parse_etime("01:00:00"), Some(3_600));
        assert_eq!(parse_etime("02:03:04"), Some(2 * 3_600 + 3 * 60 + 4));
        // `dd-hh:mm:ss`
        assert_eq!(parse_etime("1-00:00:00"), Some(86_400));
        assert_eq!(parse_etime("2-01:02:03"), Some(2 * 86_400 + 3_600 + 2 * 60 + 3));
        // Leading/trailing whitespace (ps pads the field) is tolerated.
        assert_eq!(parse_etime("   00:10  "), Some(10));
        // Garbage / undeterminable ⇒ None (age-unknown degradation).
        assert_eq!(parse_etime(""), None);
        assert_eq!(parse_etime("   "), None);
        assert_eq!(parse_etime("abc"), None);
        assert_eq!(parse_etime("1:2:3:4"), None);
        assert_eq!(parse_etime("aa:bb"), None);
        assert_eq!(parse_etime("x-01:00"), None);
    }

    #[test]
    fn resolve_startup_grace_env_wins_over_default() {
        assert_eq!(resolve_startup_grace(None), DEFAULT_STARTUP_GRACE_SECS);
        assert_eq!(resolve_startup_grace(None), 90);
        assert_eq!(resolve_startup_grace(Some(30)), 30);
        assert_eq!(resolve_startup_grace(Some(0)), 0);
    }

    #[test]
    fn young_process_within_grace_is_alive_starting() {
        // The test runner's own pid is necessarily younger than a huge grace
        // window, so classify must report the startup-grace verdict instead of
        // the fault/wedged verdict — no age injection needed (#4213 test plan).
        let dir = tempfile::tempdir().unwrap();
        let pid_file = write_pid_file(dir.path(), std::process::id());
        let marker = write_marker(
            dir.path(),
            &format!("pid_file={}\nuse_launchd=false\n", pid_file.display()),
        );
        let mut env = no_env_overrides();
        env.startup_grace_secs_override = Some(u64::MAX);
        let report = classify(dir.path(), &marker, &env);
        assert_eq!(report.state, InstallState::AliveStarting);
        assert_eq!(report.state.exit_code(), EXIT_ALIVE_BUT_UNRESPONSIVE);
        assert_eq!(report.pid, Some(std::process::id()));
        assert!(report.process_age_secs.is_some());
        assert_eq!(report.startup_grace_threshold_secs, Some(u64::MAX));
        // Grace verdict does not compute a heartbeat qualifier.
        assert!(report.heartbeat_freshness.is_none());
    }

    #[test]
    fn process_beyond_grace_falls_through_to_fault_verdict() {
        // A zero-second grace window plus a pinned process age puts the
        // process beyond grace, so the existing AliveButUnresponsive verdict
        // stands. The pinned age is load-bearing, not decorative: a real
        // sub-second-old process reports `etime` as `00:00`, which would land
        // inside the zero window (#4406).
        let dir = tempfile::tempdir().unwrap();
        let pid_file = write_pid_file(dir.path(), std::process::id());
        let heartbeat = write_heartbeat(dir.path(), Duration::from_secs(5));
        let marker = write_marker(
            dir.path(),
            &format!(
                "pid_file={}\nheartbeat_file={}\nheartbeat_interval_secs=60\nuse_launchd=false\n",
                pid_file.display(),
                heartbeat.display()
            ),
        );
        let mut env = no_env_overrides();
        env.startup_grace_secs_override = Some(0);
        // Pin a synthetic process age (#4368, see the note on
        // `alive_with_stale_heartbeat_is_stale`) comfortably larger than the
        // 5s heartbeat age so the fresh-heartbeat verdict this test targets
        // is not perturbed by prior-boot detection.
        let report = classify_with_process_age_fn(dir.path(), &marker, &env, |_| Some(1_000_000));
        assert_eq!(report.state, InstallState::AliveButUnresponsive);
        assert_eq!(report.heartbeat_freshness, Some(HeartbeatFreshness::Fresh));
        // The grace threshold used is still surfaced for JSON consumers.
        assert_eq!(report.startup_grace_threshold_secs, Some(0));
    }

    #[test]
    fn launchd_override_is_one_directional_only_forces_false() {
        // Falsy values force the pid-file path — exactly the shell's
        // `^(0|false|no)$` regex, case-insensitive.
        for falsy in ["0", "false", "no", "FALSE", "No", "NO"] {
            assert_eq!(
                parse_launchd_override(Some(falsy)),
                Some(false),
                "{falsy:?} should force use_launchd=false"
            );
        }

        // Truthy / arbitrary / empty values NEVER force launchd on — they defer
        // to the marker (`None`). This is the #4150 fix: the watchdog only ever
        // forces `false`, never `true`.
        for other in ["1", "true", "yes", "TRUE", "Yes", "", "maybe", "on"] {
            assert_eq!(
                parse_launchd_override(Some(other)),
                None,
                "{other:?} must not force use_launchd on"
            );
        }

        // Unset defers to the marker.
        assert_eq!(parse_launchd_override(None), None);
    }

    // ===================================================================
    // Watchdog protection state (#4354)
    // ===================================================================

    /// Deterministic protection env: no overrides, non-Darwin (so the systemd
    /// timer branch is selected regardless of the host the tests run on).
    fn protection_env() -> ProtectionEnv {
        ProtectionEnv {
            watchdog_label_override: None,
            launchd_label_override: None,
            systemd_unit_override: None,
            launchd_domain_override: None,
            launchd_override: None,
            is_darwin: false,
        }
    }

    fn darwin_protection_env() -> ProtectionEnv {
        ProtectionEnv {
            is_darwin: true,
            ..protection_env()
        }
    }

    #[test]
    fn protection_marker_present_and_watchdog_provisioned_is_protected() {
        let dir = tempfile::tempdir().unwrap();
        let marker = write_marker(dir.path(), "use_launchd=false\n");
        let report = classify_protection(dir.path(), &marker, &protection_env(), |_| Some(true));
        assert_eq!(report.state, ProtectionState::Protected);
        assert!(report.marker_present);
        assert_eq!(report.watchdog_provisioned, Some(true));
        assert!(report.detail.contains("is provisioned"), "{}", report.detail);
    }

    #[test]
    fn protection_marker_absent_is_no_marker_even_when_watchdog_provisioned() {
        // The #4331 state: the watchdog job exists and fires on cadence, but with
        // no marker it logs "nothing to check" — protection is disarmed. The
        // verdict must say so, while still reporting the watchdog fact (AC1).
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(MARKER_FILENAME);
        let report = classify_protection(dir.path(), &marker, &protection_env(), |_| Some(true));
        assert_eq!(report.state, ProtectionState::NoMarker);
        assert!(!report.marker_present);
        assert_eq!(report.watchdog_provisioned, Some(true));
        assert!(report.detail.contains("DISARMED"), "{}", report.detail);
        assert!(report.detail.contains("IS provisioned"), "{}", report.detail);
        assert_eq!(
            ProtectionState::NoMarker.description(),
            "unprotected — no autonomy-desired marker"
        );
    }

    #[test]
    fn protection_marker_absent_and_watchdog_absent_reports_both_facts() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join(MARKER_FILENAME);
        let report = classify_protection(dir.path(), &marker, &protection_env(), |_| Some(false));
        assert_eq!(report.state, ProtectionState::NoMarker);
        assert_eq!(report.watchdog_provisioned, Some(false));
        assert!(report.detail.contains("not provisioned either"), "{}", report.detail);
    }

    #[test]
    fn protection_marker_present_watchdog_absent_is_watchdog_not_provisioned() {
        let dir = tempfile::tempdir().unwrap();
        let marker = write_marker(dir.path(), "use_launchd=false\n");
        let report = classify_protection(dir.path(), &marker, &protection_env(), |_| Some(false));
        assert_eq!(report.state, ProtectionState::WatchdogNotProvisioned);
        assert_eq!(report.watchdog_provisioned, Some(false));
        assert_eq!(
            ProtectionState::WatchdogNotProvisioned.description(),
            "watchdog job not provisioned"
        );
    }

    #[test]
    fn protection_undeterminable_probe_degrades_to_unknown() {
        // AC4: never fail, never guess — an unanswerable provisioning probe is
        // `unknown`, not a false "not provisioned".
        let dir = tempfile::tempdir().unwrap();
        let marker = write_marker(dir.path(), "use_launchd=false\n");
        let report = classify_protection(dir.path(), &marker, &protection_env(), |_| None);
        assert_eq!(report.state, ProtectionState::Unknown);
        assert!(report.watchdog_provisioned.is_none());
        assert!(report.detail.contains("could not be probed"), "{}", report.detail);
    }

    #[test]
    fn protection_state_taxonomy_strings_are_stable() {
        assert_eq!(ProtectionState::Protected.as_str(), "protected");
        assert_eq!(ProtectionState::NoMarker.as_str(), "no-marker");
        assert_eq!(ProtectionState::WatchdogNotProvisioned.as_str(), "watchdog-not-provisioned");
        assert_eq!(ProtectionState::Unknown.as_str(), "unknown");
        assert_eq!(ProtectionState::Protected.description(), "protected");
        assert_eq!(ProtectionState::Unknown.description(), "unknown");
    }

    #[test]
    fn watchdog_job_defaults_mirror_the_start_script() {
        // Darwin: `<daemon label>-watchdog` (loom-daemon-start.sh:511).
        let job = resolve_watchdog_job(true, DEFAULT_LAUNCHD_LABEL, &darwin_protection_env());
        assert_eq!(
            job,
            WatchdogJob::Launchd {
                label: "com.rjwalters.loom-daemon-watchdog".to_string()
            }
        );
        assert_eq!(job.kind_str(), "launchd");

        // systemd: `<daemon unit%.service>-watchdog.timer` (…:596).
        let job = resolve_watchdog_job(false, DEFAULT_LAUNCHD_LABEL, &protection_env());
        assert_eq!(
            job,
            WatchdogJob::SystemdTimer {
                timer_unit: "loom-daemon-watchdog.timer".to_string()
            }
        );
        assert_eq!(job.kind_str(), "systemd-timer");
        assert_eq!(job.identifier(), "loom-daemon-watchdog.timer");
    }

    #[test]
    fn watchdog_label_env_override_is_honored_on_both_platforms() {
        // AC3: LOOM_WATCHDOG_LABEL wins over the derived name, exactly as
        // `resolve_watchdog_label` / `resolve_systemd_watchdog_unit` do.
        let mut env = darwin_protection_env();
        env.watchdog_label_override = Some("com.example.custom-wd".to_string());
        assert_eq!(
            resolve_watchdog_job(true, DEFAULT_LAUNCHD_LABEL, &env),
            WatchdogJob::Launchd {
                label: "com.example.custom-wd".to_string()
            }
        );

        let mut env = protection_env();
        env.watchdog_label_override = Some("my-wd".to_string());
        assert_eq!(
            resolve_watchdog_job(false, DEFAULT_LAUNCHD_LABEL, &env),
            WatchdogJob::SystemdTimer {
                timer_unit: "my-wd.timer".to_string()
            }
        );
    }

    #[test]
    fn watchdog_job_derives_from_the_daemon_label_and_unit_overrides() {
        // launchd: the derivation base is the *already-resolved* daemon label
        // `resolve_watchdog_job` is handed — `LOOM_LAUNCHD_LABEL`'s precedence
        // over the marker is applied one level up, in `classify_protection` (see
        // `protection_launchd_label_override_beats_the_marker_label` below).
        assert_eq!(
            resolve_watchdog_job(true, "com.example.loom-daemon", &darwin_protection_env()),
            WatchdogJob::Launchd {
                label: "com.example.loom-daemon-watchdog".to_string()
            }
        );

        // systemd: `LOOM_SYSTEMD_UNIT` is the derivation base, with `.service`
        // stripped before the `-watchdog` suffix (loom-daemon-start.sh:596).
        let mut env = protection_env();
        env.systemd_unit_override = Some("loom-scratch.service".to_string());
        assert_eq!(
            resolve_watchdog_job(false, DEFAULT_LAUNCHD_LABEL, &env),
            WatchdogJob::SystemdTimer {
                timer_unit: "loom-scratch-watchdog.timer".to_string()
            }
        );
        // A unit name with no `.service` suffix is left verbatim, matching
        // `resolve_systemd_unit`'s documented no-normalization rule.
        let mut env = protection_env();
        env.systemd_unit_override = Some("loom-scratch".to_string());
        assert_eq!(
            resolve_watchdog_job(false, DEFAULT_LAUNCHD_LABEL, &env),
            WatchdogJob::SystemdTimer {
                timer_unit: "loom-scratch-watchdog.timer".to_string()
            }
        );
    }

    #[test]
    fn protection_launchd_label_override_beats_the_marker_label() {
        // AC3: `LOOM_LAUNCHD_LABEL` wins over the marker's recorded
        // `launchd_label`, mirroring the watchdog's env-wins-over-marker rule —
        // so the derived `<label>-watchdog` job probed is the env one.
        let dir = tempfile::tempdir().unwrap();
        let marker =
            write_marker(dir.path(), "use_launchd=true\nlaunchd_label=com.example.from-marker\n");
        let mut env = darwin_protection_env();
        env.launchd_label_override = Some("com.example.from-env".to_string());
        let mut probed = None;
        let _ = classify_protection(dir.path(), &marker, &env, |job| {
            probed = Some(job.clone());
            Some(true)
        });
        assert_eq!(
            probed,
            Some(WatchdogJob::Launchd {
                label: "com.example.from-env-watchdog".to_string()
            })
        );
    }

    #[test]
    fn protection_marker_launchd_label_feeds_the_watchdog_label_on_darwin() {
        // With no LOOM_LAUNCHD_LABEL override, the *marker's* recorded label is
        // the derivation base — so a host started under a custom label probes the
        // matching watchdog job, not the default one.
        let dir = tempfile::tempdir().unwrap();
        let marker =
            write_marker(dir.path(), "use_launchd=true\nlaunchd_label=com.example.alt-daemon\n");
        let mut probed = None;
        let report = classify_protection(dir.path(), &marker, &darwin_protection_env(), |job| {
            probed = Some(job.clone());
            Some(true)
        });
        assert_eq!(report.state, ProtectionState::Protected);
        assert_eq!(
            probed,
            Some(WatchdogJob::Launchd {
                label: "com.example.alt-daemon-watchdog".to_string()
            })
        );
    }

    #[test]
    fn protection_non_darwin_never_probes_a_launchd_job() {
        // Even a marker claiming launchd resolves to the systemd timer on a
        // non-Darwin host — the "launchctl absent on Linux falls through to the
        // systemd probe" edge case, resolved before any probe is attempted.
        let dir = tempfile::tempdir().unwrap();
        let marker =
            write_marker(dir.path(), "use_launchd=true\nlaunchd_label=com.example.alt-daemon\n");
        let mut probed = None;
        let report = classify_protection(dir.path(), &marker, &protection_env(), |job| {
            probed = Some(job.clone());
            Some(false)
        });
        assert_eq!(report.state, ProtectionState::WatchdogNotProvisioned);
        assert_eq!(
            probed,
            Some(WatchdogJob::SystemdTimer {
                timer_unit: "loom-daemon-watchdog.timer".to_string()
            })
        );
    }

    #[test]
    fn protection_env_launchd_override_forces_the_systemd_probe_on_darwin() {
        let dir = tempfile::tempdir().unwrap();
        let marker = write_marker(dir.path(), "use_launchd=true\n");
        let mut env = darwin_protection_env();
        env.launchd_override = Some(false);
        let mut probed = None;
        let _ = classify_protection(dir.path(), &marker, &env, |job| {
            probed = Some(job.clone());
            Some(true)
        });
        assert!(
            matches!(probed, Some(WatchdogJob::SystemdTimer { .. })),
            "LOOM_DAEMON_LAUNCHD=0 must force the pid-file/systemd side: {probed:?}"
        );
    }

    #[test]
    fn protection_report_records_the_marker_path_it_checked() {
        // AC3: the reported path is whatever was resolved (a
        // LOOM_AUTONOMY_MARKER override lands here verbatim via
        // `probe_protection`), so an operator can confirm which file was read.
        let dir = tempfile::tempdir().unwrap();
        let custom = dir.path().join("custom-autonomy-marker");
        fs::write(&custom, "use_launchd=false\n").unwrap();
        let report = classify_protection(dir.path(), &custom, &protection_env(), |_| Some(true));
        assert_eq!(report.marker_path, custom);
        assert!(report.detail.contains("custom-autonomy-marker"), "{}", report.detail);
    }

    #[test]
    fn protection_marker_resolution_honors_the_autonomy_marker_override() {
        // The module's resolver now delegates to `autonomy_marker` (#4354) — this
        // pins that the delegation preserves both the default and the override
        // behavior. Env is read process-globally, so assert the default here and
        // rely on `autonomy_marker`'s own override coverage for the env case.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_marker_path(dir.path()),
            crate::autonomy_marker::resolve_marker_path(dir.path())
        );
    }

    #[test]
    fn protection_probe_never_panics_against_the_real_host() {
        // AC4 (read-only, never fails): the production entry point must return a
        // report (or `None` when no loom dir resolves) on ANY host — with or
        // without launchctl/systemctl, with or without a marker.
        if let Some(report) = probe_protection() {
            // Whatever the host looks like, the verdict is one of the four and
            // the detail is non-empty.
            assert!(matches!(
                report.state,
                ProtectionState::Protected
                    | ProtectionState::NoMarker
                    | ProtectionState::WatchdogNotProvisioned
                    | ProtectionState::Unknown
            ));
            assert!(!report.detail.is_empty());
        }
    }

    // ===================================================================
    // Probe timeouts (#4548): every query-only subprocess this module
    // spawns is bounded, and a hang degrades to the same value an
    // absent/failing binary already produced.
    // ===================================================================

    /// Write an executable `#!/bin/sh` stub named `name` into `dir` (the
    /// `disk_headroom.rs` PATH-stub pattern).
    fn write_stub(dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).unwrap();
        }
    }

    /// A stub that never exits on its own — the wedged-bus / stalled-XPC shape.
    /// It sleeps far longer than [`PROBE_TIMEOUT`], so an unbounded probe would
    /// visibly hang the test rather than merely slow it.
    const HANG_STUB: &str = "#!/bin/sh\nsleep 60\n";

    /// A stub that always fails fast — used to force a probe's absent/failed
    /// fallback path deterministically, regardless of whether the host
    /// actually has the real binary (and, for `launchctl`, regardless of
    /// whether the host has a reachable `gui/<uid>` login session; #4724).
    const FAIL_STUB: &str = "#!/bin/sh\nexit 1\n";

    /// Generous upper bound on a bounded probe: well above `PROBE_TIMEOUT` +
    /// spawn/kill overhead on a loaded runner, and far below `HANG_STUB`'s 60s.
    fn hang_budget() -> Duration {
        PROBE_TIMEOUT + Duration::from_secs(8)
    }

    /// Run `body` with `dir` prepended to `PATH`, restoring `PATH` afterwards.
    /// Callers must be `#[serial]` — `PATH` is process-global (#4525).
    fn with_path_prefix<T>(dir: &Path, body: impl FnOnce() -> T) -> T {
        let old_path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{old_path}", dir.display()));
        let out = body();
        std::env::set_var("PATH", old_path);
        out
    }

    #[test]
    fn probe_output_abandons_a_command_that_outlives_its_timeout() {
        // The mechanism itself: a child still running at the deadline is killed
        // and reported as `None` (no usable answer), not awaited.
        let mut cmd = Command::new("sleep");
        cmd.arg("60");
        let started = Instant::now();
        let out = probe_output(cmd, Duration::from_millis(200));
        let elapsed = started.elapsed();
        assert!(out.is_none(), "a timed-out probe must degrade to None");
        assert!(
            elapsed < Duration::from_secs(5),
            "probe_output must return at its deadline, took {elapsed:?}"
        );
    }

    #[test]
    fn probe_output_preserves_fast_success_and_fast_failure_verbatim() {
        // AC: fast paths are unchanged by the wrap. A zero exit stays
        // `Some(success)`, a NONZERO exit stays `Some(!success)` — collapsing
        // the latter to `None` would flip `systemd_timer_provisioned`'s
        // `Some(false)` ("present but disabled") into `None` ("unknown").
        let mut ok = Command::new("sh");
        ok.args(["-c", "printf hello; exit 0"]);
        let ok_out = probe_output(ok, PROBE_TIMEOUT).expect("a fast command must be captured");
        assert!(ok_out.status.success());
        assert_eq!(String::from_utf8_lossy(&ok_out.stdout), "hello");

        let mut fail = Command::new("sh");
        fail.args(["-c", "printf boom >&2; exit 3"]);
        let fail_out = probe_output(fail, PROBE_TIMEOUT).expect("a fast failure must be captured");
        assert!(!fail_out.status.success());
        assert_eq!(String::from_utf8_lossy(&fail_out.stderr), "boom");
    }

    #[test]
    fn probe_output_degrades_to_none_when_the_binary_is_absent() {
        // An unspawnable binary keeps its pre-#4548 `None`, so callers cannot
        // tell "absent" from "hung" — both are simply "no answer".
        let cmd = Command::new("loom-nonexistent-probe-binary-4548");
        assert!(probe_output(cmd, PROBE_TIMEOUT).is_none());
    }

    #[test]
    #[serial]
    fn systemd_timer_provisioned_degrades_to_unknown_when_systemctl_hangs() {
        // The headline hang mode: `systemctl --user` blocking on a wedged
        // `systemd --user` D-Bus connect. Bounded, it degrades to `None`
        // (unknown) — the same verdict an unspawnable `systemctl` gives, never a
        // false "not provisioned" and never an indefinite block.
        //
        // `systemctl` is a deliberately sibling-safe stub target: every other
        // test in this module either injects its own provisioning probe closure
        // or (like `protection_probe_never_panics_against_the_real_host`)
        // already accepts `Unknown`, so shadowing it for this window cannot
        // change another test's verdict.
        let stub_dir = tempfile::tempdir().unwrap();
        write_stub(stub_dir.path(), "systemctl", HANG_STUB);

        let started = Instant::now();
        let verdict =
            with_path_prefix(stub_dir.path(), || systemd_timer_provisioned("loom-hang.timer"));
        let elapsed = started.elapsed();

        assert_eq!(verdict, None, "a hung systemctl must read as unknown");
        assert!(
            elapsed < hang_budget(),
            "systemd_timer_provisioned must be bounded by PROBE_TIMEOUT, took {elapsed:?}"
        );
    }

    #[test]
    #[serial]
    fn launchctl_probes_degrade_when_launchctl_hangs() {
        // `launchctl print` can stall on XPC against an unreachable domain. All
        // three launchctl probes must fall back to their absent-binary values:
        // `launchctl_pid` -> None (no live pid), `launchctl_job_provisioned` ->
        // None (unknown), and `resolve_launchd_domain_detailed` -> the
        // SSH-reachable `user/<uid>` domain (its `gui/<uid>` reachability probe
        // reads as "not reachable"). An explicit domain override is passed
        // where possible so the assertions do not depend on a real `id -u` —
        // which also means the #4694 cross-check is deliberately not engaged
        // here, keeping this a single-probe timing assertion.
        let stub_dir = tempfile::tempdir().unwrap();
        write_stub(stub_dir.path(), "launchctl", HANG_STUB);

        let started = Instant::now();
        let (pid, provisioned) = with_path_prefix(stub_dir.path(), || {
            (
                launchctl_pid("gui/501", DEFAULT_LAUNCHD_LABEL),
                launchctl_job_provisioned(DEFAULT_LAUNCHD_LABEL, Some("gui/501")),
            )
        });
        let elapsed = started.elapsed();

        assert_eq!(pid, None, "a hung launchctl print must yield no pid");
        assert_eq!(provisioned, None, "a hung launchctl print must read as unknown");
        assert!(
            elapsed < hang_budget() * 2,
            "both launchctl probes must be bounded by PROBE_TIMEOUT, took {elapsed:?}"
        );
    }

    #[test]
    #[serial]
    fn resolve_launchd_domain_falls_back_to_user_uid_when_launchctl_hangs() {
        // The reachability probe inside `resolve_launchd_domain_detailed` is
        // the one launchctl call whose *timeout* is a verdict, not an error:
        // it must read as "gui/<uid> unreachable" and hand back
        // `user/<uid>`, exactly like the absent-launchctl path this repo's
        // CI already exercises. It must also report `gui/<uid>` as the
        // skipped domain to cross-check (#4694) — a hang is exactly the kind
        // of ambiguous "unreachable" this module cannot tell apart from a
        // genuine absence.
        let uid = current_uid().expect("current_uid resolves in the test env");
        let stub_dir = tempfile::tempdir().unwrap();
        write_stub(stub_dir.path(), "launchctl", HANG_STUB);

        let started = Instant::now();
        let resolution =
            with_path_prefix(stub_dir.path(), || resolve_launchd_domain_detailed(None));
        let elapsed = started.elapsed();

        assert_eq!(resolution.domain, Some(format!("user/{uid}")));
        assert_eq!(resolution.fallback_check_domain, Some(format!("gui/{uid}")));
        assert!(
            elapsed < hang_budget(),
            "resolve_launchd_domain_detailed must be bounded by PROBE_TIMEOUT, took {elapsed:?}"
        );
    }

    // ===================================================================
    // #4694: a flaky (as opposed to genuinely absent) `gui/<uid>`
    // reachability probe inside domain resolution must not permanently
    // misroute `check_liveness`/`launchctl_job_provisioned` to the wrong
    // domain for the rest of the call. Each stub below dispatches on the
    // `launchctl print <target>` argument so the *bare*-domain reachability
    // probe (`gui/<uid>`) and the *job* probes (`<domain>/<uid>/<label>`)
    // can be scripted independently.
    // ===================================================================

    #[test]
    #[serial]
    fn check_liveness_survives_a_transient_gui_domain_probe_failure() {
        // The job IS loaded under `gui/<uid>`, but the bare `gui/<uid>`
        // reachability probe fails (simulating a transient flake) — domain
        // resolution falls back to `user/<uid>`, where the job is not
        // loaded. Before this fix that produced a permanent false "not
        // loaded/alive" verdict; the cross-check must recover the true
        // positive from `gui/<uid>`.
        let uid = current_uid().expect("current_uid resolves in the test env");
        let pid = std::process::id();
        let stub_dir = tempfile::tempdir().unwrap();
        write_stub(
            stub_dir.path(),
            "launchctl",
            &format!(
                "#!/bin/sh\n\
                 target=\"$2\"\n\
                 case \"$target\" in\n\
                   gui/{uid}/*) echo \"    pid = {pid}\"; exit 0 ;;\n\
                   gui/{uid}) exit 1 ;;\n\
                   user/{uid}/*) exit 1 ;;\n\
                   *) exit 1 ;;\n\
                 esac\n"
            ),
        );

        let liveness = with_path_prefix(stub_dir.path(), || {
            check_liveness(true, DEFAULT_LAUNCHD_LABEL, None, None)
        });

        assert!(
            liveness.alive,
            "a job loaded under gui/<uid> must read as alive despite a flaky gui \
             reachability probe: {}",
            liveness.detail
        );
        assert_eq!(liveness.pid, Some(pid));
        assert!(
            liveness.detail.contains(&format!("gui/{uid}/")),
            "expected the cross-checked gui/<uid> domain in the detail, got: {}",
            liveness.detail
        );
    }

    #[test]
    #[serial]
    fn launchctl_job_provisioned_survives_a_transient_gui_domain_probe_failure() {
        // Same false-negative shape as the liveness test above, but for the
        // `Protection: watchdog job not provisioned` call site.
        let uid = current_uid().expect("current_uid resolves in the test env");
        let stub_dir = tempfile::tempdir().unwrap();
        write_stub(
            stub_dir.path(),
            "launchctl",
            &format!(
                "#!/bin/sh\n\
                 target=\"$2\"\n\
                 case \"$target\" in\n\
                   gui/{uid}/*) exit 0 ;;\n\
                   gui/{uid}) exit 1 ;;\n\
                   user/{uid}/*) exit 1 ;;\n\
                   *) exit 1 ;;\n\
                 esac\n"
            ),
        );

        let provisioned = with_path_prefix(stub_dir.path(), || {
            launchctl_job_provisioned(DEFAULT_LAUNCHD_LABEL, None)
        });

        assert_eq!(
            provisioned,
            Some(true),
            "a watchdog job provisioned under gui/<uid> must be reported provisioned \
             despite a flaky gui reachability probe"
        );
    }

    #[test]
    #[serial]
    fn negative_verdicts_still_reported_when_job_absent_from_both_domains() {
        // No regression on the true-negative case: when the job is
        // genuinely absent from BOTH domains, both call sites must still
        // report the negative verdict, not degrade it to unknown.
        let uid = current_uid().expect("current_uid resolves in the test env");
        let stub_dir = tempfile::tempdir().unwrap();
        write_stub(
            stub_dir.path(),
            "launchctl",
            &format!(
                "#!/bin/sh\n\
                 target=\"$2\"\n\
                 case \"$target\" in\n\
                   gui/{uid}) exit 1 ;;\n\
                   gui/{uid}/*) exit 1 ;;\n\
                   user/{uid}/*) exit 1 ;;\n\
                   *) exit 1 ;;\n\
                 esac\n"
            ),
        );

        let (liveness, provisioned) = with_path_prefix(stub_dir.path(), || {
            (
                check_liveness(true, DEFAULT_LAUNCHD_LABEL, None, None),
                launchctl_job_provisioned(DEFAULT_LAUNCHD_LABEL, None),
            )
        });

        assert!(
            !liveness.alive,
            "a job absent from both domains must still read as not loaded: {}",
            liveness.detail
        );
        assert_eq!(
            provisioned,
            Some(false),
            "a job absent from both domains must still read as not provisioned, not unknown"
        );
    }

    #[test]
    #[serial]
    fn pid_file_alive_overrides_a_launchd_domain_negative() {
        // #4694 AC2 — the specific check that would have prevented the
        // near-miss: the daemon was alive at a real pid with 6 sweeps
        // running, but `status` reported it dead because the launchd domain
        // probe was looking in the wrong place. A live pid-file pid must
        // override a launchd-domain negative.
        let stub_dir = tempfile::tempdir().unwrap();
        // Both domains genuinely report the job absent — no launchd signal
        // at all.
        write_stub(stub_dir.path(), "launchctl", FAIL_STUB);

        let pid_dir = tempfile::tempdir().unwrap();
        let pid_file = write_pid_file(pid_dir.path(), std::process::id());

        let liveness = with_path_prefix(stub_dir.path(), || {
            check_liveness(true, DEFAULT_LAUNCHD_LABEL, Some(&pid_file), None)
        });

        assert!(
            liveness.alive,
            "a live pid-file pid must override a launchd-domain negative: {}",
            liveness.detail
        );
        assert_eq!(liveness.pid, Some(std::process::id()));
        assert!(liveness.detail.contains("pid file"), "{}", liveness.detail);
    }

    #[test]
    #[serial]
    fn launchctl_job_provisioned_cross_check_timeout_degrades_to_unknown() {
        // AC3/AC4d: a cross-check probe that itself hangs (rather than
        // returning a fast negative) must degrade the verdict to unknown —
        // never fold a timeout into a confident `Some(false)`.
        let uid = current_uid().expect("current_uid resolves in the test env");
        let stub_dir = tempfile::tempdir().unwrap();
        write_stub(
            stub_dir.path(),
            "launchctl",
            &format!(
                "#!/bin/sh\n\
                 target=\"$2\"\n\
                 case \"$target\" in\n\
                   gui/{uid}) exit 1 ;;\n\
                   gui/{uid}/*) sleep 60 ;;\n\
                   user/{uid}/*) exit 1 ;;\n\
                   *) exit 1 ;;\n\
                 esac\n"
            ),
        );

        let started = Instant::now();
        let provisioned = with_path_prefix(stub_dir.path(), || {
            launchctl_job_provisioned(DEFAULT_LAUNCHD_LABEL, None)
        });
        let elapsed = started.elapsed();

        assert_eq!(
            provisioned, None,
            "a hung cross-check probe must degrade to unknown, not a confident negative"
        );
        assert!(
            elapsed < hang_budget() * 2,
            "the cross-check probe must be bounded by PROBE_TIMEOUT, took {elapsed:?}"
        );
    }

    #[test]
    #[serial]
    fn launchctl_job_provisioned_honors_explicit_domain_override_without_cross_check() {
        // AC6: an explicit `LOOM_LAUNCHD_DOMAIN` override must be honored
        // verbatim with no cross-check fallback. Even though the job IS
        // provisioned under `gui/<uid>`, an explicit override domain that
        // reports "absent" must stay a confident negative — never upgraded
        // by a secondary probe the override opted out of.
        let uid = current_uid().expect("current_uid resolves in the test env");
        let stub_dir = tempfile::tempdir().unwrap();
        write_stub(
            stub_dir.path(),
            "launchctl",
            &format!(
                "#!/bin/sh\n\
                 target=\"$2\"\n\
                 case \"$target\" in\n\
                   custom/1/*) exit 1 ;;\n\
                   gui/{uid}/*) exit 0 ;;\n\
                   *) exit 1 ;;\n\
                 esac\n"
            ),
        );

        let provisioned = with_path_prefix(stub_dir.path(), || {
            launchctl_job_provisioned(DEFAULT_LAUNCHD_LABEL, Some("custom/1"))
        });

        assert_eq!(
            provisioned,
            Some(false),
            "an explicit domain override must not be upgraded by a gui/<uid> cross-check"
        );
    }

    #[test]
    fn kill_id_and_ps_backed_probes_keep_their_fast_path_behavior() {
        // AC: the `kill`/`id`/`ps` probes — the three that exist on Linux CI —
        // behave identically after the wrap. Deliberately NOT PATH-stubbed:
        // many sibling tests in this module classify against a real
        // `pid_alive(std::process::id())` / `ps -o etime=`, and shadowing those
        // binaries process-globally would flake them (#4525). `probe_output`'s
        // own timeout coverage above proves the bound they share.
        assert!(pid_alive(std::process::id()), "our own pid must read as alive");
        // `i32::MAX` is above every platform's `pid_max`, so it never names a
        // live process, and (unlike `u32::MAX`, which some `kill` builds
        // wrap to the "every process" `-1`) it stays a plain positive pid.
        const NO_SUCH_PID: u32 = i32::MAX as u32;
        assert!(!pid_alive(NO_SUCH_PID), "an out-of-range pid must read as dead");
        assert!(current_uid().is_some_and(|u| u.chars().all(|c| c.is_ascii_digit())));
        assert!(
            process_age_secs(std::process::id()).is_some(),
            "a real ps against our own pid must still parse"
        );
        assert_eq!(process_age_secs(NO_SUCH_PID), None, "an unknown pid stays None");
    }
}
