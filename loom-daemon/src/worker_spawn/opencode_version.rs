//! OpenCode CLI version detection (#8438).
//!
//! A native launch `exec`s, so no Loom process survives to observe the child.
//! This probe is therefore the last point at which an unsupported CLI can be
//! refused: the `run` argv differs per major, and a guarded launch is admitted
//! only on a major whose guard was verified by a live canary.
use super::LaunchError;
use crate::proc_exec::{Completion, ExecError};
use std::{
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const SUPPORTED: &str = "supported OpenCode versions: 1.x (guarded and unguarded launches, \
     verified on 1.18.31) and 2.x (unguarded launches only, argv built for 2.0.10)";

/// A major whose `run` argv this adapter knows how to build. Anything else is
/// refused rather than guessed at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Major {
    V1,
    V2,
}

impl Major {
    fn from_number(major: u32) -> Option<Self> {
        match major {
            1 => Some(Self::V1),
            2 => Some(Self::V2),
            _ => None,
        }
    }

    /// Whether a live guarded-canary receipt exists for this major.
    ///
    /// This is code tracking evidence, not a setting: there is deliberately no
    /// configuration key or environment override. Flip a major to `true` only
    /// in the same change that adds its dated receipt under `defaults/docs/`.
    ///
    /// 2.x is `false` because its `run --auto` approves every permission that
    /// is not explicitly denied. Whether 2.x still honors the deny-by-default
    /// agent in `native_tools/provision.rs` has not been exercised, and if it
    /// does not, a role launch runs OpenCode's built-in tools outside Loom's
    /// guards while still exiting 0 — a state no fake-CLI test can detect.
    pub fn guard_verified(self) -> bool {
        match self {
            Self::V1 => true,
            Self::V2 => false,
        }
    }
}

/// Probe `bin --version`, then admit the launch for the reported major.
pub fn detect(bin: &Path, guarded: bool) -> Result<Major, LaunchError> {
    let mut probe = Command::new(bin);
    probe.arg("--version").stdin(Stdio::null());
    admit(crate::proc_exec::run_bounded(probe, PROBE_TIMEOUT), guarded)
}

fn admit(probe: Result<Completion, ExecError>, guarded: bool) -> Result<Major, LaunchError> {
    let output = match probe {
        // Same codes `exec` reports, so a missing binary is still 127.
        Err(ExecError::Spawn(error)) => {
            return Err(LaunchError {
                code: if error.kind() == std::io::ErrorKind::NotFound {
                    127
                } else {
                    126
                },
                message: format!("cannot execute worker harness: {error}"),
            })
        }
        Err(ExecError::Collect(error)) => {
            return Err(LaunchError::config(format!(
                "`opencode --version` could not be read ({error}); {SUPPORTED}"
            )))
        }
        Ok(Completion::TimedOut { .. }) => {
            return Err(LaunchError::config(format!(
                "`opencode --version` did not answer within {}s; {SUPPORTED}",
                PROBE_TIMEOUT.as_secs()
            )))
        }
        Ok(Completion::Exited(output)) => output,
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let reported = reported(&stdout);
    if !output.status.success() {
        return Err(LaunchError::config(format!(
            "`opencode --version` exited unsuccessfully; {SUPPORTED}"
        )));
    }
    let Some(number) = parse_major(&stdout) else {
        return Err(LaunchError::config(format!(
            "cannot read an OpenCode version from `opencode --version` output {reported:?}; {SUPPORTED}"
        )));
    };
    let Some(major) = Major::from_number(number) else {
        return Err(LaunchError::config(format!(
            "unsupported OpenCode version {reported:?}; {SUPPORTED}. Refusing to guess a launch \
             shape: install a supported release or point LOOM_OPENCODE_BIN at one"
        )));
    };
    if guarded && !major.guard_verified() {
        return Err(LaunchError::config(format!(
            "OpenCode {reported:?} has no live guarded-canary receipt, so this guarded \
             (role-tagged) launch is refused; {SUPPORTED}. Its `run --auto` approves anything not \
             explicitly denied, and Loom's deny-by-default tool surface is unverified on this \
             major (see .loom/docs/guardrail-parity-native.md). Unguarded free-form launches on \
             this version are unaffected"
        )));
    }
    Ok(major)
}

/// The binary's own first output line, bounded and printable, for a diagnostic.
fn reported(stdout: &str) -> String {
    stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(40)
        .collect()
}

/// `opencode v2.0.10` is what 2.0.10 prints. The 1.x format was not observed
/// when this was written, so an `opencode ` prefix and a `v` are both optional.
fn parse_major(stdout: &str) -> Option<u32> {
    let line = stdout.lines().map(str::trim).find(|l| !l.is_empty())?;
    let rest = line.strip_prefix("opencode").map_or(line, str::trim_start);
    let rest = rest.strip_prefix('v').unwrap_or(rest);
    let (major, tail) = rest.split_once('.')?;
    let numeric = !major.is_empty() && major.bytes().all(|b| b.is_ascii_digit());
    if !numeric || !tail.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    major.parse().ok()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};

    fn exited(code: i32, stdout: &str) -> Result<Completion, ExecError> {
        Ok(Completion::Exited(Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }))
    }

    #[test]
    fn parses_both_observed_and_tolerated_shapes() {
        assert_eq!(parse_major("opencode v2.0.10\n"), Some(2));
        assert_eq!(parse_major("1.18.31\n"), Some(1));
        assert_eq!(parse_major("v1.18.31"), Some(1));
        assert_eq!(parse_major("opencode 1.18.31"), Some(1));
        assert_eq!(parse_major("\n  opencode v12.4.0-beta.1  \n"), Some(12));
        for bad in [
            "", "garbage", "opencode", "v", "2", "2.", ".1", "two.0.0", "-1.0.0",
        ] {
            assert_eq!(parse_major(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn admits_known_majors_and_refuses_the_rest_with_the_supported_range() {
        assert_eq!(admit(exited(0, "1.18.31\n"), false).unwrap(), Major::V1);
        assert_eq!(admit(exited(0, "1.18.31\n"), true).unwrap(), Major::V1);
        assert_eq!(admit(exited(0, "opencode v2.0.10\n"), false).unwrap(), Major::V2);
        for probe in [
            exited(0, "opencode v3.0.0\n"),
            exited(0, "0.9.0\n"),
            exited(0, "garbage\n"),
            exited(0, ""),
            exited(1, "1.18.31\n"),
            Ok(Completion::TimedOut {
                stdout: Vec::new(),
                stderr: Vec::new(),
            }),
            Err(ExecError::Collect(std::io::Error::other("boom"))),
        ] {
            let error = admit(probe, false).unwrap_err();
            assert_eq!(error.code, 78, "{}", error.message);
            assert!(error.message.contains("1.x"), "{}", error.message);
            assert!(error.message.contains("2.x"), "{}", error.message);
        }
    }

    #[test]
    fn guarded_launch_needs_a_live_receipt_for_the_major() {
        let error = admit(exited(0, "opencode v2.0.10\n"), true).unwrap_err();
        assert_eq!(error.code, 78);
        assert!(error.message.contains("guarded"), "{}", error.message);
        assert!(error.message.contains("v2.0.10"), "{}", error.message);
        // The evidence ledger itself: changing either line needs a new receipt.
        assert!(Major::V1.guard_verified());
        assert!(!Major::V2.guard_verified());
    }

    #[test]
    fn spawn_failures_keep_the_exec_exit_codes() {
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(
            admit(Err(ExecError::Spawn(missing)), true)
                .unwrap_err()
                .code,
            127
        );
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            admit(Err(ExecError::Spawn(denied)), false)
                .unwrap_err()
                .code,
            126
        );
    }

    #[test]
    fn diagnostic_echo_of_the_binary_output_is_bounded_and_printable() {
        let noisy = format!("9.0.0\u{1b}[31m{}\nsecond line", "x".repeat(200));
        let error = admit(exited(0, &noisy), false).unwrap_err();
        assert!(!error.message.contains('\u{1b}'));
        assert!(!error.message.contains("second line"));
        assert!(error.message.len() < 500, "{}", error.message.len());
    }
}
