//! Asking the supervisor to relaunch a job it already owns.
//!
//! This is the narrow remediation path (#4232 launchd / #4862 systemd), reached
//! only when the job is LOADED, has no live process, and its last exit was
//! clean. See [`super::remediation`] for why that gate is exactly that narrow.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// How long a single supervisor command may take before it is abandoned.
const SUPERVISOR_CMD_TIMEOUT: Duration = Duration::from_secs(15);

/// Build the launchd relaunch command.
///
/// **`kickstart` PLAIN, never `-k`.** `-k` means "kill the job if it is running,
/// then start it", and this path is reached from a liveness check that already
/// concluded the job has no live pid. Between that conclusion and this command
/// the supervisor may have relaunched it — the race is the ordinary case, not an
/// exotic one, because that is precisely what a supervisor does. `-k` would then
/// kill a daemon that had just come back, turning a self-healing outage into a
/// watchdog-caused one. Plain `kickstart` on an already-running job is a no-op,
/// which is exactly the behaviour wanted here (#4232).
#[must_use]
pub fn launchd_kickstart_argv(service: &str) -> Vec<String> {
    vec![
        "launchctl".to_string(),
        "kickstart".to_string(),
        service.to_string(),
    ]
}

/// Build the systemd relaunch commands, in order.
///
/// `reset-failed` first because a unit in the `failed` state refuses `start`
/// until its failure is cleared; without it the second command silently does
/// nothing and the watchdog reports a remediation that never happened.
#[must_use]
pub fn systemd_restart_argvs(unit: &str) -> Vec<Vec<String>> {
    vec![
        vec![
            "systemctl".to_string(),
            "--user".to_string(),
            "reset-failed".to_string(),
            unit.to_string(),
        ],
        vec![
            "systemctl".to_string(),
            "--user".to_string(),
            "start".to_string(),
            unit.to_string(),
        ],
    ]
}

/// Run one argv, bounded. Returns the exit code, or `None` if it could not be
/// run or exceeded the budget.
///
/// Failures are values, not errors: every caller reports what it observed and
/// carries on. A watchdog that aborts because a supervisor command hung has
/// stopped watching.
#[must_use]
pub fn run_bounded(argv: &[String]) -> Option<i32> {
    let (program, args) = argv.split_first()?;
    let mut cmd = Command::new(program);
    cmd.args(args);
    crate::sweep_registry::output_with_timeout(cmd, SUPERVISOR_CMD_TIMEOUT)
        .ok()
        .flatten()
        .and_then(|o| o.status.code())
}

/// How many times, and how far apart, to re-check for a live process after
/// asking the supervisor to relaunch.
pub struct Recheck {
    pub attempts: u64,
    pub interval: Duration,
}

impl Recheck {
    /// The shell's `${LOOM_WATCHDOG_KICKSTART_RECHECK_ATTEMPTS:-3}` and
    /// `${LOOM_WATCHDOG_KICKSTART_RECHECK_INTERVAL:-1}`.
    ///
    /// A relaunch is not instantaneous, so a single immediate re-check would
    /// report failure for a daemon that comes up a second later — and that
    /// false negative is expensive here, because it is what escalates.
    #[must_use]
    pub fn from_env() -> Self {
        let attempts = super::env::var("LOOM_WATCHDOG_KICKSTART_RECHECK_ATTEMPTS")
            .filter(|s| {
                !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && !s.starts_with('0')
            })
            .and_then(|s| s.parse().ok())
            .unwrap_or(3);
        let interval = super::env::num("LOOM_WATCHDOG_KICKSTART_RECHECK_INTERVAL", 1);
        Self {
            attempts,
            interval: Duration::from_secs(interval),
        }
    }
}

/// Poll for a live process, returning its pid when one appears.
///
/// `alive` is injected so the caller supplies whichever liveness question
/// applies (launchd's own pid, a systemd MainPID, a pid file) and this function
/// owns only the bounded polling.
pub fn recheck_alive(recheck: &Recheck, mut alive: impl FnMut() -> Option<u32>) -> Option<u32> {
    for i in 0..recheck.attempts {
        if let Some(pid) = alive() {
            return Some(pid);
        }
        // No sleep after the final attempt: it would delay the tick's report
        // without ever being observed.
        if i + 1 < recheck.attempts {
            std::thread::sleep(recheck.interval);
        }
    }
    None
}

/// Whether `systemctl` is usable on this host.
#[must_use]
pub fn systemctl_available() -> bool {
    super::supervisor::systemctl_available()
}

/// Whether `launchctl` is usable on this host.
#[must_use]
pub fn launchctl_available() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join("launchctl").is_file()))
}

/// Advisory: the path a caller should report when neither supervisor is present.
#[must_use]
pub fn no_supervisor_detail(cli_dir: &Path) -> String {
    format!(
        "no supervisor command available on this host — relaunch by hand with {}",
        cli_dir.join("loom-daemon-start.sh").display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kickstart_is_never_passed_dash_k() {
        // -k kills a running job before starting it. This path is reached from
        // a liveness check that already said there is no live pid, and the
        // supervisor may have relaunched it in between -- which is the ordinary
        // case, because that is what a supervisor does. -k would kill a daemon
        // that had just come back, turning a self-healing outage into one the
        // watchdog caused.
        let argv = launchd_kickstart_argv("gui/501/com.rjwalters.loom-daemon");
        assert_eq!(
            argv,
            [
                "launchctl",
                "kickstart",
                "gui/501/com.rjwalters.loom-daemon"
            ]
        );
        assert!(!argv.iter().any(|a| a == "-k"), "{argv:?}");
        assert!(!argv.iter().any(|a| a.starts_with("-k")), "{argv:?}");
    }

    #[test]
    fn systemd_clears_the_failed_state_before_starting() {
        // A unit in `failed` refuses `start`. Without reset-failed the start
        // silently does nothing and the watchdog reports a remediation that
        // never happened.
        let cmds = systemd_restart_argvs("loom-daemon.service");
        assert_eq!(cmds.len(), 2);
        assert!(cmds[0].contains(&"reset-failed".to_string()), "{cmds:?}");
        assert!(cmds[1].contains(&"start".to_string()), "{cmds:?}");
        assert_eq!(cmds[0].last().unwrap(), "loom-daemon.service");
    }

    #[test]
    fn systemd_commands_are_user_scoped() {
        // The daemon runs as a --user unit. Omitting it would target the system
        // manager, which either fails or acts on something else entirely.
        for c in systemd_restart_argvs("loom-daemon.service") {
            assert!(c.contains(&"--user".to_string()), "{c:?}");
        }
    }

    #[test]
    fn the_service_name_is_passed_as_one_argument() {
        // Never interpolated into a shell string: a service name is data.
        let argv = launchd_kickstart_argv("gui/501/weird name; rm -rf x");
        assert_eq!(argv.len(), 3);
        assert_eq!(argv[2], "gui/501/weird name; rm -rf x");
    }

    #[test]
    fn recheck_returns_as_soon_as_a_pid_appears() {
        let mut calls = 0;
        let r = Recheck {
            attempts: 5,
            interval: Duration::from_millis(0),
        };
        let got = recheck_alive(&r, || {
            calls += 1;
            (calls >= 2).then_some(4242)
        });
        assert_eq!(got, Some(4242));
        assert_eq!(calls, 2, "must stop polling once it is up");
    }

    #[test]
    fn recheck_gives_up_after_the_budget() {
        let mut calls = 0;
        let r = Recheck {
            attempts: 3,
            interval: Duration::from_millis(0),
        };
        let got = recheck_alive(&r, || {
            calls += 1;
            None
        });
        assert_eq!(got, None);
        assert_eq!(calls, 3, "exactly the budget, no more");
    }

    #[test]
    fn a_single_attempt_recheck_still_checks_once() {
        let mut calls = 0;
        let r = Recheck {
            attempts: 1,
            interval: Duration::from_millis(0),
        };
        let _ = recheck_alive(&r, || {
            calls += 1;
            None
        });
        assert_eq!(calls, 1);
    }

    #[test]
    fn a_zero_attempt_recheck_checks_nothing_and_says_so() {
        // Reachable only from a hand-set knob. It must not loop forever, and it
        // must not claim the daemon is up.
        let mut calls = 0;
        let r = Recheck {
            attempts: 0,
            interval: Duration::from_millis(0),
        };
        assert_eq!(
            recheck_alive(&r, || {
                calls += 1;
                Some(1)
            }),
            None
        );
        assert_eq!(calls, 0);
    }
}
