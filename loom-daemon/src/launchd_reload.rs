//! `loom-daemon restart --reload-supervisor` (Issue #6682): boot the launchd
//! job for THIS daemon out and back in, so a hand-edited plist's
//! `EnvironmentVariables` actually takes effect.
//!
//! # Why this exists
//!
//! [`crate::launchd_env_drift`] already detects and loudly warns about the gap
//! a plain `loom-daemon restart` cannot close: launchd's `KeepAlive` relaunch
//! reuses the job spec already held in launchd's own memory since the last
//! `bootstrap` — it never re-reads the plist file from disk. The only way to
//! make an on-disk plist edit (e.g. adding `RUSTC_WRAPPER`/`SCCACHE_*` to the
//! supervisor's own environment) actually take effect is `launchctl bootout`
//! followed by `launchctl bootstrap`, and until this module the daemon had no
//! primitive for that — an operator had to run both commands by hand.
//!
//! Doing it by hand is racy: `launchctl bootout` is **asynchronous** — it
//! returns before the kernel has actually finished tearing the old job down —
//! so an immediate `bootstrap` can race that teardown and fail with
//! `Bootstrap failed: 5: Input/output error` (EIO), even though the plist is
//! perfectly valid. The worst-case outcome of that race is the worst possible
//! state: the job is now unloaded and **nothing will restart it** — no
//! `KeepAlive`, no supervisor, no watchdog remediation, because from launchd's
//! point of view the job simply is not loaded.
//!
//! This module ports the retry shape `defaults/scripts/cli/loom-daemon-start.sh`
//! already proved in production for exactly this race (#5081: settle after
//! bootout, retry bootstrap specifically on the EIO shape, bounded attempts)
//! so the same remediation is available as a first-class primitive against an
//! **already-running** daemon, not only at `loom-daemon-start.sh` invocation
//! time. It additionally treats `already bootstrapped` (launchd error 37) as
//! success — a shape the shell implementation's unconditional prior `bootout`
//! never has to consider, but a primitive invoked without necessarily knowing
//! the prior state should.
//!
//! # In-flight sweeps survive this on launchd — no drain required
//!
//! Unlike the equivalent systemd remediation (a unit drop-in edit + a plain
//! `daemon-reload`, which re-reads `Environment=` fresh without ever tearing
//! the job down), a launchd bootout **does** tear the job down before
//! bootstrapping it back. Despite that, an in-flight sweep is not killed by
//! it: every sweep gets its own process group (#3800) and reparents to
//! `launchd` (`ppid=1`) rather than dying with the job that spawned it — this
//! was confirmed directly in the incident this issue documents (a kicad-tools
//! sweep kept running across a `bootout`/`bootstrap` cycle). So a
//! `--reload-supervisor` invocation does **not** need `--drain` first on
//! launchd. **This is specifically NOT true on systemd** — there, a `restart`
//! (with or without `--drain`) runs the unit's stop job over the whole
//! cgroup, which reaps every sweep/role-run child that has not already
//! reparented out of it (#5119) — which is exactly why `--reload-supervisor`
//! refuses outright on that platform rather than trying to approximate the
//! same operation there.
//!
//! # Never leaves the job unloaded silently
//!
//! If every bootstrap retry is exhausted, this reports [`ReloadOutcome::ExhaustedRetries`]
//! loudly (non-zero exit, from the CLI caller) and names the exact
//! `launchctl bootstrap <domain> <plist>` command an operator must run by
//! hand — since, at that point, nothing else will bring the daemon back.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::restart_verify::{self, Supervisor};

// ============================================================================
// Knobs — same env var names/defaults as loom-daemon-start.sh's #5081 retry
// loop, so the two implementations cannot silently drift onto different
// tuning.
// ============================================================================

/// Seconds to wait, after `bootout`, for the job to actually disappear from
/// `launchctl print` before attempting `bootstrap`.
pub const BOOTOUT_SETTLE_SECS_ENV: &str = "LOOM_DAEMON_BOOTOUT_SETTLE_SECS";
/// Default for [`BOOTOUT_SETTLE_SECS_ENV`].
pub const DEFAULT_BOOTOUT_SETTLE_SECS: u64 = 5;

/// Bounded `launchctl bootstrap` retry attempts.
pub const BOOTSTRAP_RETRY_ATTEMPTS_ENV: &str = "LOOM_DAEMON_BOOTSTRAP_RETRY_ATTEMPTS";
/// Default for [`BOOTSTRAP_RETRY_ATTEMPTS_ENV`].
pub const DEFAULT_BOOTSTRAP_RETRY_ATTEMPTS: u32 = 4;

/// Seconds to sleep between bootstrap retries.
pub const BOOTSTRAP_RETRY_SECS_ENV: &str = "LOOM_DAEMON_BOOTSTRAP_RETRY_SECS";
/// Default for [`BOOTSTRAP_RETRY_SECS_ENV`].
pub const DEFAULT_BOOTSTRAP_RETRY_SECS: u64 = 2;

/// Bound on each individual `launchctl` probe/mutation — all are local,
/// in-memory launchd operations and should return near-instantly; this is
/// only a safety net against a wedged binary hanging the whole reload.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Interval between settle polls.
const SETTLE_POLL_INTERVAL: Duration = Duration::from_millis(200);

// ============================================================================
// Pure classification
// ============================================================================

/// How a failed `launchctl bootstrap` attempt should be handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BootstrapFailureClass {
    /// launchd error 37 — the job is already bootstrapped. Treated as
    /// success, not a failure (Issue #6682's 4th acceptance criterion).
    AlreadyBootstrapped,
    /// launchd error 5 (`Input/output error`) — the well-known async-bootout
    /// teardown race (#5081). Worth retrying.
    TransientEio,
    /// Anything else — a genuine plist/permission/argument problem a retry
    /// cannot fix. Fails immediately, even on the first attempt.
    Fatal,
}

/// Does `stderr` carry a leading launchd error `code` (`"<code>:"`, not
/// preceded by another digit — so e.g. matching `"5"` does not also match a
/// `"135:"`)? Mirrors the shell implementation's
/// `grep -qE '(^|[^0-9])<code>: '` boundary check.
fn stderr_has_launchd_code(stderr: &str, code: &str) -> bool {
    let needle = format!("{code}:");
    let bytes = stderr.as_bytes();
    let mut search_from = 0usize;
    while let Some(rel) = stderr[search_from..].find(&needle) {
        let abs = search_from + rel;
        let preceded_by_digit = abs > 0 && bytes[abs - 1].is_ascii_digit();
        if !preceded_by_digit {
            return true;
        }
        search_from = abs + 1;
    }
    false
}

/// Classify a failed bootstrap attempt's stderr. Pure so it's directly
/// unit-testable against captured/synthetic launchctl output.
fn classify_bootstrap_failure(stderr: &str) -> BootstrapFailureClass {
    if stderr_has_launchd_code(stderr, "37") {
        BootstrapFailureClass::AlreadyBootstrapped
    } else if stderr_has_launchd_code(stderr, "5") {
        BootstrapFailureClass::TransientEio
    } else {
        BootstrapFailureClass::Fatal
    }
}

// ============================================================================
// Outcome
// ============================================================================

/// The terminal verdict of a `--reload-supervisor` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadOutcome {
    /// This host is not launchd-supervised (or has no installed plist to
    /// reload) — nothing was touched.
    Refused { detail: String },
    /// `launchctl bootstrap` succeeded — either on this attempt, or it
    /// reported "already bootstrapped" (launchd error 37), which is treated
    /// identically to a fresh success.
    Success {
        attempts: u32,
        already_bootstrapped: bool,
    },
    /// Every bootstrap retry was exhausted. **The launchd job may now be
    /// UNLOADED with nothing to restart it** — `remediation_command` is the
    /// exact `launchctl bootstrap <domain> <plist>` an operator must run by
    /// hand.
    ExhaustedRetries {
        attempts: u32,
        last_stderr: String,
        remediation_command: String,
    },
}

impl ReloadOutcome {
    /// `true` only for [`ReloadOutcome::Success`] — both `Refused` and
    /// `ExhaustedRetries` are non-zero-exit outcomes for the CLI caller.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self, ReloadOutcome::Success { .. })
    }

    /// Operator-facing rendering of the outcome.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            ReloadOutcome::Refused { detail } => {
                format!("loom-daemon restart --reload-supervisor refused: {detail}")
            }
            ReloadOutcome::Success {
                attempts,
                already_bootstrapped: true,
            } => format!(
                "launchd job already bootstrapped (attempt {attempts}) — nothing further to do; \
                 treated as success."
            ),
            ReloadOutcome::Success {
                attempts,
                already_bootstrapped: false,
            } => format!("launchd job reloaded: bootstrap succeeded on attempt {attempts}."),
            ReloadOutcome::ExhaustedRetries {
                attempts,
                last_stderr,
                remediation_command,
            } => format!(
                "FAILED to reload the launchd job after {attempts} attempt(s). Last error:\n\
                 {last_stderr}\n\
                 \n\
                 The job may now be UNLOADED with NOTHING to restart it. Run this by hand:\n  \
                 {remediation_command}"
            ),
        }
    }
}

// ============================================================================
// Orchestration core — parameterized over its I/O boundaries so the retry
// shape is unit-testable without a real launchd host or `launchctl` binary.
// ============================================================================

/// [`reload_launchd_supervisor`]'s implementation, injected over every I/O
/// boundary (bootout, the settle poll, each bootstrap attempt, and sleeping)
/// so the whole state machine is exercised hermetically.
#[allow(clippy::too_many_arguments)] // pure orchestration seam: each arg is a distinct injected I/O boundary or knob
fn reload_launchd_core(
    settle_secs: u64,
    max_attempts: u32,
    retry_sleep_secs: u64,
    domain: &str,
    plist_path: &str,
    mut bootout: impl FnMut(),
    mut job_still_loaded: impl FnMut() -> bool,
    mut bootstrap_attempt: impl FnMut() -> (bool, String),
    mut sleep: impl FnMut(Duration),
) -> ReloadOutcome {
    // `launchctl bootout` is async — best-effort, and a failure here just
    // means the job was not currently loaded, which is fine (mirrors the
    // shell implementation's `|| true`).
    bootout();

    // Settle: poll until the job is actually gone, bounded by settle_secs, so
    // the first bootstrap attempt does not race the kernel's own teardown
    // (#5081) any more than it has to. Never blocks forever.
    let settle_deadline = Instant::now() + Duration::from_secs(settle_secs);
    while job_still_loaded() {
        if Instant::now() >= settle_deadline {
            break;
        }
        sleep(SETTLE_POLL_INTERVAL);
    }

    let max_attempts = max_attempts.max(1);
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let (success, stderr) = bootstrap_attempt();
        if success {
            return ReloadOutcome::Success {
                attempts: attempt,
                already_bootstrapped: false,
            };
        }
        match classify_bootstrap_failure(&stderr) {
            BootstrapFailureClass::AlreadyBootstrapped => {
                return ReloadOutcome::Success {
                    attempts: attempt,
                    already_bootstrapped: true,
                };
            }
            BootstrapFailureClass::TransientEio if attempt < max_attempts => {
                sleep(Duration::from_secs(retry_sleep_secs));
            }
            _ => {
                return ReloadOutcome::ExhaustedRetries {
                    attempts: attempt,
                    last_stderr: stderr,
                    remediation_command: format!("launchctl bootstrap {domain} {plist_path}"),
                };
            }
        }
    }
}

// ============================================================================
// Real I/O entry point
// ============================================================================

/// Split a `<domain>/<label>` service string (e.g. `resolve_launchd_service_detailed`'s
/// `service` field) into its `(domain, label)` parts. `domain` itself may
/// contain a `/` (`gui/501`, `user/501`), so this splits at the LAST `/`.
fn split_service(service: &str) -> (String, String) {
    match service.rsplit_once('/') {
        Some((domain, label)) => (domain.to_string(), label.to_string()),
        None => (String::new(), service.to_string()),
    }
}

/// The on-disk plist path `loom-daemon-start.sh` renders to for a given
/// label: `~/Library/LaunchAgents/<label>.plist`.
fn default_plist_path(label: &str) -> Option<PathBuf> {
    dirs::home_dir().map(|home| {
        home.join("Library")
            .join("LaunchAgents")
            .join(format!("{label}.plist"))
    })
}

/// Explanatory refusal text for a non-launchd-supervised host (Issue #6682's
/// 5th acceptance criterion) — names the systemd-equivalent remediation,
/// which already works without this primitive.
fn non_launchd_refusal(why: &str) -> String {
    format!(
        "{why}. `--reload-supervisor` is launchd-specific — on systemd, a unit drop-in edit \
         plus `systemctl --user daemon-reload` already picks up new `Environment=` lines on the \
         NEXT restart (it re-reads the unit file fresh every time, unlike launchd's KeepAlive \
         relaunch); follow that with `loom-daemon restart --drain` so the stop job does not reap \
         any in-flight sweep's cgroup (#5119) — unlike launchd, a systemd restart does NOT let \
         in-flight sweeps survive on its own."
    )
}

/// Run a query/mutation-only `launchctl` invocation, collapsing every failure
/// mode (absent binary, spawn failure, a hang past [`PROBE_TIMEOUT`]) to
/// `None`.
fn run_launchctl(args: &[&str]) -> Option<std::process::Output> {
    let mut cmd = Command::new("launchctl");
    cmd.args(args);
    cmd.stdin(std::process::Stdio::null());
    crate::sweep_registry::output_with_timeout(cmd, PROBE_TIMEOUT)
        .ok()
        .flatten()
}

/// Real `launchctl bootout <domain>/<label>` — best-effort, failure ignored
/// (mirrors `loom-daemon-start.sh`'s `|| true`: a "not loaded" bootout
/// failure is the common, harmless case).
fn real_bootout(target: &str) {
    let _ = run_launchctl(&["bootout", target]);
}

/// Real `launchctl print <domain>/<label>` reachability probe, folded to "is
/// it still loaded" for the settle poll.
fn real_job_still_loaded(target: &str) -> bool {
    run_launchctl(&["print", target]).is_some_and(|o| o.status.success())
}

/// Real `launchctl bootstrap <domain> <plist>` attempt, folded to
/// `(succeeded, stderr)`.
fn real_bootstrap_attempt(domain: &str, plist_path: &str) -> (bool, String) {
    match run_launchctl(&["bootstrap", domain, plist_path]) {
        Some(out) => (out.status.success(), String::from_utf8_lossy(&out.stderr).to_string()),
        None => (
            false,
            "launchctl bootstrap produced no output (spawn failure or timeout)".to_string(),
        ),
    }
}

/// Reload the launchd job for this daemon: `launchctl bootout` then a
/// settle-and-bounded-retry `launchctl bootstrap` (Issue #6682), so a
/// hand-edited plist's `EnvironmentVariables` actually takes effect.
///
/// Refuses outright — no `launchctl` invocation at all — on a host that is
/// not launchd-supervised, or one with no installed plist to reload against.
#[must_use]
pub fn reload_launchd_supervisor() -> ReloadOutcome {
    if !cfg!(target_os = "macos") {
        return ReloadOutcome::Refused {
            detail: non_launchd_refusal("this host is not macOS (launchd only exists there)"),
        };
    }
    match restart_verify::probe_host_supervisor() {
        Some(Supervisor::Launchd) => {}
        other => {
            let sup_desc = other.map_or("unknown", Supervisor::as_str);
            return ReloadOutcome::Refused {
                detail: non_launchd_refusal(&format!(
                    "this host's supervisor is '{sup_desc}', not launchd"
                )),
            };
        }
    }

    let resolution = restart_verify::resolve_launchd_service_detailed();
    let (domain, label) = split_service(&resolution.service);
    let Some(plist_path) = default_plist_path(&label).filter(|p| p.exists()) else {
        return ReloadOutcome::Refused {
            detail: format!(
                "no installed launchd plist found for label '{label}' — nothing to reload. Run \
                 ./.loom/scripts/cli/loom-daemon-start.sh first to install one."
            ),
        };
    };
    let plist_str = plist_path.display().to_string();
    let target = resolution.service;

    reload_launchd_core(
        restart_verify::resolve_secs(
            std::env::var(BOOTOUT_SETTLE_SECS_ENV).ok().as_deref(),
            DEFAULT_BOOTOUT_SETTLE_SECS,
        ),
        std::env::var(BOOTSTRAP_RETRY_ATTEMPTS_ENV)
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(DEFAULT_BOOTSTRAP_RETRY_ATTEMPTS),
        restart_verify::resolve_secs(
            std::env::var(BOOTSTRAP_RETRY_SECS_ENV).ok().as_deref(),
            DEFAULT_BOOTSTRAP_RETRY_SECS,
        ),
        &domain,
        &plist_str,
        || real_bootout(&target),
        || real_job_still_loaded(&target),
        || real_bootstrap_attempt(&domain, &plist_str),
        std::thread::sleep,
    )
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // Pure classification
    // ------------------------------------------------------------------

    #[test]
    fn classifies_the_exact_incident_eio_shape() {
        assert_eq!(
            classify_bootstrap_failure("Bootstrap failed: 5: Input/output error"),
            BootstrapFailureClass::TransientEio
        );
    }

    #[test]
    fn classifies_already_bootstrapped_launchd_37() {
        assert_eq!(
            classify_bootstrap_failure("Bootstrap failed: 37: Already bootstrapped"),
            BootstrapFailureClass::AlreadyBootstrapped
        );
    }

    #[test]
    fn classifies_an_unrelated_failure_as_fatal() {
        assert_eq!(
            classify_bootstrap_failure("Bootstrap failed: 22: Invalid argument"),
            BootstrapFailureClass::Fatal
        );
        assert_eq!(classify_bootstrap_failure(""), BootstrapFailureClass::Fatal);
    }

    #[test]
    fn code_boundary_does_not_false_match_a_longer_number() {
        // "135:" must not be read as code "5:".
        assert_eq!(
            classify_bootstrap_failure("Bootstrap failed: 135: Some other error"),
            BootstrapFailureClass::Fatal
        );
        // "137:" must not be read as code "37:".
        assert_eq!(
            classify_bootstrap_failure("Bootstrap failed: 137: Some other error"),
            BootstrapFailureClass::Fatal
        );
    }

    #[test]
    fn split_service_separates_domain_and_label() {
        assert_eq!(
            split_service("gui/501/com.rjwalters.loom-daemon"),
            ("gui/501".to_string(), "com.rjwalters.loom-daemon".to_string())
        );
        assert_eq!(
            split_service("user/501/com.rjwalters.loom-daemon"),
            ("user/501".to_string(), "com.rjwalters.loom-daemon".to_string())
        );
    }

    // ------------------------------------------------------------------
    // Orchestration core (Issue #6682's hermetic test matrix)
    // ------------------------------------------------------------------

    /// No sleeping in tests — just record how many times/how long the core
    /// asked to sleep, so retry pacing is assertable without a slow suite.
    fn no_op_sleep(_d: Duration) {}

    #[test]
    fn bootstrap_succeeding_on_the_first_try_is_reported_as_success() {
        let mut bootstrap_calls = 0u32;
        let outcome = reload_launchd_core(
            0,
            4,
            0,
            "gui/501",
            "/tmp/x.plist",
            || {},
            || false,
            || {
                bootstrap_calls += 1;
                (true, String::new())
            },
            no_op_sleep,
        );
        assert_eq!(
            outcome,
            ReloadOutcome::Success {
                attempts: 1,
                already_bootstrapped: false
            }
        );
        assert_eq!(bootstrap_calls, 1);
    }

    #[test]
    fn bootstrap_failing_once_with_eio_then_succeeding_retries_and_reports_success() {
        // The exact incident shape: `Bootstrap failed: 5: Input/output error`
        // on the first attempt (the async-bootout race), succeeding on retry.
        let mut bootstrap_calls = 0u32;
        let outcome = reload_launchd_core(
            0,
            4,
            0,
            "gui/501",
            "/tmp/x.plist",
            || {},
            || false,
            || {
                bootstrap_calls += 1;
                if bootstrap_calls == 1 {
                    (false, "Bootstrap failed: 5: Input/output error".to_string())
                } else {
                    (true, String::new())
                }
            },
            no_op_sleep,
        );
        assert_eq!(
            outcome,
            ReloadOutcome::Success {
                attempts: 2,
                already_bootstrapped: false
            }
        );
        assert_eq!(bootstrap_calls, 2);
    }

    #[test]
    fn exhausting_every_retry_fails_loudly_and_names_the_manual_command() {
        let mut bootstrap_calls = 0u32;
        let outcome = reload_launchd_core(
            0,
            3,
            0,
            "gui/501",
            "/Users/example/Library/LaunchAgents/com.rjwalters.loom-daemon.plist",
            || {},
            || false,
            || {
                bootstrap_calls += 1;
                (false, "Bootstrap failed: 5: Input/output error".to_string())
            },
            no_op_sleep,
        );
        assert_eq!(bootstrap_calls, 3, "must retry exactly max_attempts times, no more");
        match outcome {
            ReloadOutcome::ExhaustedRetries {
                attempts,
                remediation_command,
                ..
            } => {
                assert_eq!(attempts, 3);
                assert_eq!(
                    remediation_command,
                    "launchctl bootstrap gui/501 /Users/example/Library/LaunchAgents/com.rjwalters.loom-daemon.plist"
                );
            }
            other => panic!("expected ExhaustedRetries, got {other:?}"),
        }
    }

    #[test]
    fn already_bootstrapped_is_success_not_a_retry_trigger() {
        let mut bootstrap_calls = 0u32;
        let outcome = reload_launchd_core(
            0,
            4,
            0,
            "gui/501",
            "/tmp/x.plist",
            || {},
            || false,
            || {
                bootstrap_calls += 1;
                (false, "Bootstrap failed: 37: Already bootstrapped".to_string())
            },
            no_op_sleep,
        );
        assert_eq!(bootstrap_calls, 1, "already-bootstrapped must not trigger a retry loop");
        assert_eq!(
            outcome,
            ReloadOutcome::Success {
                attempts: 1,
                already_bootstrapped: true
            }
        );
    }

    #[test]
    fn a_non_eio_non_37_failure_fails_immediately_without_retrying() {
        // A genuine plist/permission problem: retrying cannot fix it, so the
        // core must not burn through every attempt before giving up.
        let mut bootstrap_calls = 0u32;
        let outcome = reload_launchd_core(
            0,
            4,
            0,
            "gui/501",
            "/tmp/x.plist",
            || {},
            || false,
            || {
                bootstrap_calls += 1;
                (false, "Bootstrap failed: 22: Invalid argument".to_string())
            },
            no_op_sleep,
        );
        assert_eq!(bootstrap_calls, 1, "a fatal failure must not be retried");
        assert!(matches!(outcome, ReloadOutcome::ExhaustedRetries { attempts: 1, .. }));
    }

    #[test]
    fn settle_poll_waits_for_the_job_to_actually_disappear_before_bootstrapping() {
        // job_still_loaded reports true for the first two polls, then false —
        // bootstrap must not be attempted until it does.
        let mut poll_calls = 0u32;
        let mut sleep_calls = 0u32;
        let outcome = reload_launchd_core(
            5,
            4,
            0,
            "gui/501",
            "/tmp/x.plist",
            || {},
            || {
                poll_calls += 1;
                poll_calls < 3
            },
            || (true, String::new()),
            |_d| sleep_calls += 1,
        );
        assert_eq!(poll_calls, 3, "must poll until job_still_loaded reports false");
        assert_eq!(
            sleep_calls, 2,
            "must sleep between settle polls, not between bootstrap and success"
        );
        assert_eq!(
            outcome,
            ReloadOutcome::Success {
                attempts: 1,
                already_bootstrapped: false
            }
        );
    }

    #[test]
    fn settle_poll_is_bounded_and_proceeds_to_bootstrap_anyway_on_timeout() {
        // job_still_loaded NEVER reports false — the settle poll must still
        // give up once its deadline (elapsed via a fast-forwarding sleep
        // stand-in) passes, rather than hanging the whole reload forever.
        let mut poll_calls = 0u32;
        let outcome = reload_launchd_core(
            0, // settle_secs = 0 -> the very first deadline check already fires
            4,
            0,
            "gui/501",
            "/tmp/x.plist",
            || {},
            || {
                poll_calls += 1;
                true
            },
            || (true, String::new()),
            no_op_sleep,
        );
        assert_eq!(
            outcome,
            ReloadOutcome::Success {
                attempts: 1,
                already_bootstrapped: false
            }
        );
        assert!(poll_calls >= 1, "must poll at least once");
    }

    #[test]
    fn max_attempts_of_zero_is_treated_as_at_least_one() {
        let mut bootstrap_calls = 0u32;
        let outcome = reload_launchd_core(
            0,
            0,
            0,
            "gui/501",
            "/tmp/x.plist",
            || {},
            || false,
            || {
                bootstrap_calls += 1;
                (true, String::new())
            },
            no_op_sleep,
        );
        assert_eq!(bootstrap_calls, 1);
        assert!(matches!(outcome, ReloadOutcome::Success { .. }));
    }

    #[test]
    fn render_names_the_manual_command_on_exhaustion() {
        let outcome = ReloadOutcome::ExhaustedRetries {
            attempts: 4,
            last_stderr: "Bootstrap failed: 5: Input/output error".to_string(),
            remediation_command: "launchctl bootstrap gui/501 /tmp/x.plist".to_string(),
        };
        let rendered = outcome.render();
        assert!(rendered.contains("UNLOADED"));
        assert!(rendered.contains("launchctl bootstrap gui/501 /tmp/x.plist"));
        assert!(!outcome.is_ok());
    }

    #[test]
    fn render_reports_already_bootstrapped_as_success() {
        let outcome = ReloadOutcome::Success {
            attempts: 1,
            already_bootstrapped: true,
        };
        assert!(outcome.is_ok());
        assert!(outcome.render().contains("already bootstrapped"));
    }

    #[test]
    fn refusal_names_the_systemd_equivalent_remediation() {
        let detail = non_launchd_refusal("this host's supervisor is 'systemd', not launchd");
        assert!(detail.contains("daemon-reload"));
        assert!(detail.contains("restart --drain"));
        let outcome = ReloadOutcome::Refused { detail };
        assert!(!outcome.is_ok());
        assert!(outcome
            .render()
            .starts_with("loom-daemon restart --reload-supervisor refused:"));
    }
}
