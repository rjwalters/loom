//! The bounded auto-remediation gate (#4232 launchd / #4862 systemd).
//!
//! A supervisor job that is LOADED but has no live process is a specific,
//! recoverable shape: the supervisor accepted the job and then failed to
//! relaunch it. `launchctl kickstart` (or `systemctl start`) fixes exactly that.
//!
//! # The gate, and why it is this narrow
//!
//! Remediation fires **only when the last exit was clean**. That is the whole
//! safety property, and it is not conservatism for its own sake:
//!
//! - A **clean** exit (status 0) is the restart-primitive's own contract
//!   (#4054/#4077) — the daemon meant to go down and come back, and the
//!   supervisor dropped the second half. Restarting completes an interrupted
//!   operation.
//! - A **non-zero** exit means the daemon died for a reason. Restarting it
//!   re-runs whatever killed it. Unattended, on a timer, that is a crash loop
//!   that burns the host and buries the cause under identical log lines. The
//!   #4232 gate exists precisely so this path is never taken automatically.
//! - **143** (SIGTERM) is a deliberate stop. Reviving it fights the operator
//!   (#6388).
//!
//! An unreadable or absent exit status is treated as non-clean. "We could not
//! tell" must never license an automatic restart — the whole point of the gate
//! is that acting requires positive evidence, not the absence of evidence
//! against.

/// Extract the supervisor's last exit status from `launchctl print` output.
///
/// Mirrors the shell's
/// `grep -oE 'last exit (code|status)[[:space:]]*=[[:space:]]*[-0-9]+' | head -n1 | grep -oE '[-0-9]+$'`
/// and each property of that pipeline is load-bearing:
///
/// - **Line-scoped, all lines.** `grep` scans every line and `head -n1` takes
///   the first match. Real `launchctl print` output is dozens of lines with
///   this field buried in the middle.
/// - **Unanchored within the line.** `grep -oE` matches anywhere, not only at
///   the start.
/// - **Trailing text after the number is ignored**, because the `-o` match ends
///   at the last digit.
///
/// A first version of this function got all three wrong, and differential
/// testing against the shell caught it (see the module tests). The worst was
/// using `?` inside the line loop, which returned `None` from the whole
/// function on the first non-matching line instead of continuing — so the
/// status was only ever found when it happened to be on line 1. Against real
/// output that means [`gate`] would always see `None`, treat it as unclean, and
/// **auto-remediation would never fire at all** — a safety feature silently
/// degraded into a no-op, with every unit test green because each fixture put
/// the field on the first line.
#[must_use]
pub fn parse_launchd_last_exit(output: &str) -> Option<i64> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"last exit (?:code|status)[[:space:]]*=[[:space:]]*([-0-9]+)")
            .expect("static launchctl last-exit pattern")
    });
    output
        .lines()
        .find_map(|line| re.captures(line))
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse().ok())
}

/// Whether this tick may auto-remediate.
#[derive(Debug, PartialEq, Eq)]
pub enum Gate {
    /// The job is loaded and the last exit was clean — kickstart it.
    Remediate,
    /// The job is not loaded, so there is nothing to kickstart; a full start is
    /// needed instead, which is the bounded-recovery path, not this one.
    NotLoaded,
    /// The job is loaded but the last exit was not clean. Deliberately NOT
    /// remediated. Carries the status so the report can say which.
    UncleanExit { status: Option<i64> },
}

/// Decide, from the supervisor's own report.
///
/// `last_exit` is `None` when it could not be read — treated exactly like a
/// dirty exit, never like a clean one.
#[must_use]
pub fn gate(job_loaded: bool, last_exit: Option<i64>) -> Gate {
    if !job_loaded {
        return Gate::NotLoaded;
    }
    match last_exit {
        Some(0) => Gate::Remediate,
        other => Gate::UncleanExit { status: other },
    }
}

/// Signal-shaped exit codes: a job killed or interrupted rather than failing.
///
/// `143` = 128+15 (SIGTERM), `130` = 128+2 (SIGINT). The negative forms are
/// what launchd reports on some macOS versions for the same thing.
const SIGNAL_SHAPED_EXITS: &[i64] = &[143, 130, -15, -2];

/// Whether the supervisor's recorded exit looks like a signal, and the
/// operator-facing detail naming it.
///
/// # Why this is NOT a reason to refuse recovery (#6388)
///
/// Before #6388 a signal-shaped exit was read as "the operator stopped it" and
/// recovery was skipped. That was wrong, and the resulting report was
/// self-contradictory: it said a daemon was expected (marker present) and in
/// the same breath refused to revive it because of an exit code.
///
/// **Only marker ABSENCE means a deliberate stop.** The marker is operator
/// intent; an exit code is not. A stray SIGTERM — an OOM killer, a careless
/// `kill`, a host going down — leaves the marker in place, and a daemon that is
/// still expected should come back. So this detail exists to be NAMED in the
/// report, never to block the attempt.
#[must_use]
pub fn launchd_exit_signal_detail(last_exit: Option<i64>) -> Option<String> {
    let status = last_exit?;
    SIGNAL_SHAPED_EXITS
        .contains(&status)
        .then(|| format!("launchd records the job's last exit status as {status} (SIGTERM/SIGINT)"))
}

/// The systemd equivalent, from `ExecMainCode` and `ExecMainStatus`.
#[must_use]
pub fn systemd_exit_signal_detail(exec_code: &str, exec_status: &str) -> Option<String> {
    match exec_code {
        "killed" if matches!(exec_status, "TERM" | "INT" | "15" | "2") => {
            Some(format!("systemd records the unit's main process as killed by SIG{exec_status}"))
        }
        "exited" if matches!(exec_status, "143" | "130") => Some(format!(
            "systemd records the unit's main process as exiting {exec_status} (SIGTERM/SIGINT)"
        )),
        _ => None,
    }
}

/// The note the outage report carries when a signal-shaped exit is on record.
#[must_use]
pub fn signal_rule_note(detail: &str) -> String {
    format!(
        " RULE: marker present, {detail} -> stray signal, recovering (NOT a deliberate operator \
         stop — only marker ABSENCE means that, #6388)."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_shaped_exits_are_recognised_in_both_spellings() {
        for status in [143, 130, -15, -2] {
            assert!(
                launchd_exit_signal_detail(Some(status)).is_some(),
                "{status} is signal-shaped"
            );
        }
        for status in [0, 1, 2, 70, 15] {
            assert!(
                launchd_exit_signal_detail(Some(status)).is_none(),
                "{status} is not signal-shaped"
            );
        }
        assert!(launchd_exit_signal_detail(None).is_none());
    }

    #[test]
    fn a_signal_shaped_exit_still_falls_through_to_bounded_recovery() {
        // #6388: only marker ABSENCE means a deliberate stop. A stray SIGTERM
        // -- an OOM kill, a careless `kill`, a host going down -- leaves the
        // marker in place, and a daemon that is still expected should come
        // back. The gate refuses SUPERVISOR remediation (that path needs a
        // clean exit) but must not block the attempt entirely.
        assert_eq!(gate(true, Some(143)), Gate::UncleanExit { status: Some(143) });
        assert!(launchd_exit_signal_detail(Some(143)).is_some(), "and it is nameable");
    }

    #[test]
    fn the_rule_note_says_recovering_not_refusing() {
        // The pre-#6388 report was self-contradictory: a daemon was expected,
        // and in the same breath it refused to revive it because of an exit
        // code. The note exists so that never reads that way again.
        let n =
            signal_rule_note("launchd records the job's last exit status as 143 (SIGTERM/SIGINT)");
        assert!(n.contains("stray signal, recovering"), "{n}");
        assert!(n.contains("only marker ABSENCE means that"), "{n}");
        assert!(n.contains("#6388"), "{n}");
    }

    #[test]
    fn systemd_reports_signals_two_different_ways() {
        // `killed` carries a signal NAME or number; `exited` carries 128+n.
        assert!(systemd_exit_signal_detail("killed", "TERM").is_some());
        assert!(systemd_exit_signal_detail("killed", "15").is_some());
        assert!(systemd_exit_signal_detail("exited", "143").is_some());
        assert!(systemd_exit_signal_detail("exited", "130").is_some());
        // A clean exit is not a signal, and neither is a plain failure.
        assert!(systemd_exit_signal_detail("exited", "0").is_none());
        assert!(systemd_exit_signal_detail("exited", "1").is_none());
        assert!(systemd_exit_signal_detail("killed", "KILL").is_none(), "SIGKILL is not a stop");
    }

    // --- regressions from differential testing against the retired shell ---

    #[test]
    fn the_field_is_found_anywhere_in_the_output_not_only_on_the_first_line() {
        // The bug this pins: `?` inside the line loop returned None from the
        // whole function on the first non-matching line. Real `launchctl print`
        // output buries this field in dozens of lines, so the parser would have
        // found it essentially never -- and `gate` treats None as unclean, so
        // auto-remediation would have silently become a no-op.
        let out = "\tstate = not running\n\tpid = 42\n\tlast exit code = 0\n\tprogram = x\n";
        assert_eq!(parse_launchd_last_exit(out), Some(0));
    }

    #[test]
    fn the_match_is_unanchored_within_the_line() {
        // `grep -oE` matches anywhere in the line, not only at its start.
        assert_eq!(parse_launchd_last_exit("xlast exit code = 0"), Some(0));
    }

    #[test]
    fn trailing_text_after_the_number_is_ignored() {
        // The `-o` match ends at the last digit, so anything after is not part
        // of the value and does not invalidate it.
        assert_eq!(parse_launchd_last_exit("last exit code = 0 extra"), Some(0));
    }

    #[test]
    fn a_realistic_multi_line_launchctl_print_parses() {
        let out = "com.rjwalters.loom-daemon = {\n\tactive count = 0\n\tpath = /x/y.plist\n\t\
                   state = not running\n\tprogram = /usr/local/bin/loom-daemon\n\t\
                   last exit code = 143\n\truns = 4\n}\n";
        assert_eq!(parse_launchd_last_exit(out), Some(143));
        assert_eq!(
            gate(true, parse_launchd_last_exit(out)),
            Gate::UncleanExit { status: Some(143) }
        );
    }

    #[test]
    fn a_clean_exit_on_a_loaded_job_is_the_only_case_that_remediates() {
        assert_eq!(gate(true, Some(0)), Gate::Remediate);
    }

    #[test]
    fn a_crash_is_never_auto_restarted() {
        // Restarting re-runs whatever killed it. Unattended, on a timer, that
        // is a crash loop that buries the cause.
        for status in [1, 2, 70, 127, -1] {
            assert_eq!(
                gate(true, Some(status)),
                Gate::UncleanExit {
                    status: Some(status)
                },
                "status {status} must not remediate"
            );
        }
    }

    #[test]
    fn sigterm_is_a_deliberate_stop_and_is_never_revived() {
        // #6388: 143 is the operator stopping it. Reviving fights them.
        assert_eq!(gate(true, Some(143)), Gate::UncleanExit { status: Some(143) });
    }

    #[test]
    fn an_unreadable_exit_status_is_treated_as_unclean() {
        // "We could not tell" must never license an automatic restart. Acting
        // requires positive evidence, not the absence of evidence against.
        assert_eq!(gate(true, None), Gate::UncleanExit { status: None });
    }

    #[test]
    fn an_unloaded_job_is_a_different_problem() {
        assert_eq!(gate(false, Some(0)), Gate::NotLoaded);
        assert_eq!(gate(false, None), Gate::NotLoaded);
    }

    #[test]
    fn both_launchctl_spellings_parse() {
        // launchctl has used each across macOS versions.
        assert_eq!(parse_launchd_last_exit("\tlast exit code = 0\n"), Some(0));
        assert_eq!(parse_launchd_last_exit("\tlast exit status = 0\n"), Some(0));
    }

    #[test]
    fn the_first_match_wins() {
        let out = "\tlast exit code = 0\n\tlast exit code = 1\n";
        assert_eq!(parse_launchd_last_exit(out), Some(0));
    }

    #[test]
    fn a_negative_status_parses() {
        assert_eq!(parse_launchd_last_exit("\tlast exit status = -1\n"), Some(-1));
    }

    #[test]
    fn spacing_around_the_equals_is_tolerated() {
        for s in [
            "last exit code = 143",
            "last exit code=143",
            "last exit code   =   143",
            "        last exit code = 143",
        ] {
            assert_eq!(parse_launchd_last_exit(s), Some(143), "{s:?}");
        }
    }

    #[test]
    fn output_without_the_field_yields_none_not_zero() {
        // Returning 0 here would remediate on a job that never reported an
        // exit at all -- inventing the one value that licenses a restart.
        assert_eq!(parse_launchd_last_exit(""), None);
        assert_eq!(parse_launchd_last_exit("state = running\npid = 42\n"), None);
    }

    #[test]
    fn a_non_numeric_value_does_not_parse_as_clean() {
        assert_eq!(parse_launchd_last_exit("last exit code = (ipc/send) invalid"), None);
    }
}
