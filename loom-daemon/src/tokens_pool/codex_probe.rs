//! Live Codex rate limits via `codex app-server` (issue #9233).
//!
//! [`super::codex_check`] reads each profile's recorded rollout snapshots,
//! which are only as current as the account's last Codex turn: an account
//! nobody has used for a few days reports a stale window or none at all. This
//! module asks Codex itself — `account/read` + `account/rateLimits/read` over
//! the app-server's JSON-RPC stdio — and produces the same [`UsageSnapshot`]
//! the rest of `accounts check` already consumes, so ranking, health feedback,
//! and output are unchanged; only the source is live (`accounts check --live`).
//!
//! # Ownership (ADR-0017 Decision 1)
//!
//! A **session-managed** profile is probed **only inside its session
//! container** (`docker exec`), the same seam `accounts status` uses for its
//! in-container login probe. If no session container is running the result is
//! [`ProbeOutcome::SessionUnavailable`] — never a host-direct fallback, because
//! the container is that account's single `auth.json` owner and Codex has no
//! cross-process lock of its own (a proactive refresh from a second process can
//! race the owner's and kill the refresh chain). A host-managed profile is
//! probed host-directly with `CODEX_HOME=<profile>`.
//!
//! # Wire (codex-cli 0.156, verified in a session container 2026-09-27)
//!
//! - Newline-delimited JSON; replies **omit** `jsonrpc`; notifications carry no
//!   `id` and are skipped.
//! - The requests are written up front and stdin is held open for
//!   [`STDIN_HOLD_SECS`] (the app-server answers pipelined requests in order,
//!   then exits on EOF), so one non-interactive `sh -c` works identically on the
//!   host and through `docker exec`, whose stdin the existing seam closes.
//! - `rateLimits.primary` / `.secondary` each carry `usedPercent`,
//!   `windowDurationMins`, `resetsAt` (epoch seconds). On 0.156 the **weekly**
//!   window arrives as `primary` with `secondary: null`, so windows are filed by
//!   duration ([`super::codex_check::slot_by_duration`]), never by slot.
//! - An unused window reports `resetsAt ≈ now + window` — a window that has not
//!   started and would move on every probe. It is kept as measured headroom but
//!   with **no** reset instant ([`window_not_started`]).
//! - `-32600` / `-32601` on `account/rateLimits/read` mean the capability is
//!   absent (an older CLI) — unless `account/read` answered `account: null`,
//!   which an unauthenticated home also produces, so that is checked first.
//!
//! Nothing secret is read or emitted: no token, no email, no raw reply body.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};

use super::codex_check::{epoch_to_datetime, slot_by_duration, RateLimitWindow, UsageSnapshot};
use super::session_lifecycle::{self, ContainerRunner, ExecOutput, ProcessContainerRunner};

/// Seconds stdin stays open after the requests are written. The app-server
/// answers in about a second; this is the ceiling before EOF ends it.
pub const STDIN_HOLD_SECS: u64 = 4;

/// Wall-clock bound for one account's probe, container or host.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// Output ceiling: the replies are a few KB; anything past this is not a probe.
const MAX_OUTPUT_BYTES: u64 = 256 * 1024;

/// A window reporting `resetsAt` within this many seconds of
/// `now + windowDurationMins` with nothing used has not started.
const NOT_STARTED_TOLERANCE_SECS: i64 = 180;

/// The whole probe as one POSIX `sh -c` program. Runs with `CODEX_HOME`
/// already set by the caller (host) or by the session container's
/// environment. stderr is discarded: it carries only diagnostics.
#[must_use]
pub fn probe_script() -> String {
    format!(
        "(printf '%s\\n' \
         '{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"clientInfo\":{{\"name\":\"loom-daemon-probe\",\"version\":\"{version}\"}}}}}}' \
         '{{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{{}}}}' \
         '{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"account/read\",\"params\":{{}}}}' \
         '{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"account/rateLimits/read\",\"params\":{{}}}}'; \
         sleep {hold}) | codex -s read-only -a never app-server 2>/dev/null",
        version = env!("CARGO_PKG_VERSION"),
        hold = STDIN_HOLD_SECS,
    )
}

/// What one live probe established. Secret-free by construction.
#[derive(Debug, Clone, PartialEq)]
pub enum ProbeOutcome {
    /// A live reading.
    Measured(UsageSnapshot),
    /// `account/read` answered `account: null`: the home is not logged in.
    NotLoggedIn,
    /// The CLI does not implement `account/rateLimits/read`.
    CapabilityAbsent,
    /// Session-managed, but its session container is not running (or the
    /// container runtime is unavailable). Not evidence about the account.
    SessionUnavailable,
    /// The probe ran but produced no usable answer.
    Failed(&'static str),
}

impl ProbeOutcome {
    /// The row detail `accounts check` reports for a probe that measured
    /// nothing; `None` for a measurement.
    #[must_use]
    pub fn detail(&self) -> Option<&'static str> {
        match self {
            Self::Measured(_) => None,
            Self::NotLoggedIn => Some("not_logged_in"),
            Self::CapabilityAbsent => Some("rate_limits_unsupported_by_codex_cli"),
            Self::SessionUnavailable => Some("session_unavailable"),
            Self::Failed(why) => Some(why),
        }
    }
}

/// How a probe program is run. Split out so tests can prove the ownership
/// rule (a session-managed profile never reaches [`ProbeTransport::host`]).
pub trait ProbeTransport {
    /// Run the probe host-directly with `CODEX_HOME=profile`.
    fn host(&self, profile: &Path) -> Result<ExecOutput>;
    /// Run the probe inside `container`. `Ok(None)` = no probe was possible
    /// (not running, or no container runtime).
    fn container(&self, container: &str) -> Result<Option<ExecOutput>>;
}

/// The real transport: `sh -c` on the host, `docker exec` via the session
/// lifecycle's own [`ProcessContainerRunner`] in a container.
pub struct ProcessProbeTransport;

impl ProbeTransport for ProcessProbeTransport {
    fn host(&self, profile: &Path) -> Result<ExecOutput> {
        run_bounded(
            Command::new("sh")
                .args(["-c", &probe_script()])
                .env("CODEX_HOME", profile),
        )
    }

    fn container(&self, container: &str) -> Result<Option<ExecOutput>> {
        let runner = ProcessContainerRunner;
        match runner.inspect(container)? {
            Some(state) if state.running => {}
            _ => return Ok(None),
        }
        let script = probe_script();
        let out = runner.exec_capture(container, &["sh", "-c", &script], PROBE_TIMEOUT)?;
        Ok((!out.unavailable).then_some(out))
    }
}

/// Probe one account, honoring ADR-0017's ownership rule.
pub fn probe_account<T: ProbeTransport + ?Sized>(
    transport: &T,
    name: &str,
    profile: &Path,
    now: DateTime<Utc>,
) -> ProbeOutcome {
    let run = if session_lifecycle::is_session_managed(profile) {
        match transport.container(&session_lifecycle::container_name(name)) {
            Ok(Some(out)) => out,
            Ok(None) => return ProbeOutcome::SessionUnavailable,
            Err(_) => return ProbeOutcome::Failed("session_probe_error"),
        }
    } else {
        match transport.host(profile) {
            Ok(out) => out,
            Err(_) => return ProbeOutcome::Failed("host_probe_error"),
        }
    };
    if run.unavailable {
        return ProbeOutcome::Failed("codex_cli_unavailable");
    }
    if run.timed_out {
        return ProbeOutcome::Failed("probe_timed_out");
    }
    parse_probe_output(&run.output, now)
}

/// Interpret the app-server's replies. Pure, for the tests.
#[must_use]
pub fn parse_probe_output(text: &str, now: DateTime<Utc>) -> ProbeOutcome {
    let mut account_null = false;
    let mut limits: Option<serde_json::Value> = None;
    let mut capability_absent = false;
    for line in text.lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        match message.get("id").and_then(serde_json::Value::as_i64) {
            Some(2) => {
                if message
                    .get("result")
                    .is_some_and(|r| r.get("account").is_some_and(serde_json::Value::is_null))
                {
                    account_null = true;
                }
            }
            Some(3) => {
                if let Some(code) = message
                    .get("error")
                    .and_then(|e| e.get("code"))
                    .and_then(serde_json::Value::as_i64)
                {
                    capability_absent = code == -32600 || code == -32601;
                } else if let Some(rl) = message.get("result").and_then(|r| r.get("rateLimits")) {
                    limits = Some(rl.clone());
                }
            }
            _ => {}
        }
    }
    // An unauthenticated home answers `account: null` *and* -32600 on the
    // rate-limit call; the null account is the real finding.
    if account_null {
        return ProbeOutcome::NotLoggedIn;
    }
    if capability_absent {
        return ProbeOutcome::CapabilityAbsent;
    }
    let Some(limits) = limits else {
        return ProbeOutcome::Failed("no_rate_limit_reply");
    };
    let window = |key: &str| limits.get(key).and_then(|w| parse_live_window(w, now));
    let (primary, secondary) = slot_by_duration(window("primary"), window("secondary"));
    if primary.is_none() && secondary.is_none() {
        return ProbeOutcome::Failed("no_rate_limit_windows");
    }
    ProbeOutcome::Measured(UsageSnapshot {
        observed_at: now,
        primary,
        secondary,
    })
}

fn parse_live_window(value: &serde_json::Value, now: DateTime<Utc>) -> Option<RateLimitWindow> {
    let used_percent = value
        .get("usedPercent")
        .and_then(serde_json::Value::as_f64)?;
    if !used_percent.is_finite() || used_percent < 0.0 {
        return None;
    }
    let window_minutes = value
        .get("windowDurationMins")
        .and_then(serde_json::Value::as_u64);
    let mut resets_at = value
        .get("resetsAt")
        .and_then(serde_json::Value::as_i64)
        .and_then(epoch_to_datetime);
    if window_not_started(used_percent, window_minutes, resets_at, now) {
        resets_at = None;
    }
    Some(RateLimitWindow {
        used_fraction: (used_percent / 100.0).min(1.0),
        window_minutes,
        resets_at,
    })
}

/// An unused window whose reset is `now + its own length`: it starts at first
/// use, so its "reset" is not an instant anything will happen at.
#[must_use]
pub fn window_not_started(
    used_percent: f64,
    window_minutes: Option<u64>,
    resets_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> bool {
    let (Some(minutes), Some(reset)) = (window_minutes, resets_at) else {
        return false;
    };
    let Ok(minutes) = i64::try_from(minutes) else {
        return false;
    };
    let expected = now + chrono::Duration::minutes(minutes);
    used_percent == 0.0 && (reset - expected).num_seconds().abs() <= NOT_STARTED_TOLERANCE_SECS
}

/// Run a host command to completion under [`PROBE_TIMEOUT`], reading stdout on
/// a thread so a chatty child can never deadlock on a full pipe.
fn run_bounded(command: &mut Command) -> Result<ExecOutput> {
    let mut child = match command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ExecOutput {
                success: false,
                unavailable: true,
                timed_out: false,
                exit_code: None,
                output: String::new(),
            });
        }
        Err(error) => return Err(error.into()),
    };
    let stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(stdout) = stdout {
            let _ = stdout.take(MAX_OUTPUT_BYTES).read_to_end(&mut bytes);
        }
        bytes
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if started.elapsed() >= PROBE_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let bytes = reader.join().unwrap_or_default();
    Ok(ExecOutput {
        success: status.is_some_and(|s| s.success()),
        unavailable: false,
        timed_out: status.is_none(),
        exit_code: status.and_then(|s| s.code()),
        output: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

#[cfg(test)]
mod tests;
