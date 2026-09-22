//! Which manager owns the running daemon, and what binary that manager will
//! actually exec.
//!
//! launchd and `systemd --user` are both checked AHEAD of the `.daemon.pid`
//! tier, and that ordering is not a preference: a supervised relaunch assigns
//! a FRESH pid every time (launchd's `KeepAlive:SuccessfulExit`, systemd's
//! `Restart=on-success`), so the pid file goes stale after the first relaunch
//! even for a job this script's own start wrapper created — and a
//! hand-bootstrapped daemon has no state files at all.
//!
//! `resolve_supervisor_bin` (#6009) reads the path the detected supervisor
//! will exec from the supervisor's OWN persisted config — systemd's
//! `ExecStart=`, launchd's `ProgramArguments[0]` — deliberately SEPARATE from
//! `locate_daemon_bin`'s PATH-based resolution. The two are not guaranteed to
//! agree (a stray `loom-daemon` earlier on PATH than the one the supervisor
//! was pointed at), and PATH resolution alone has no way to notice that.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::util;
use crate::daemon_start::platform;

/// Which manager owns the running daemon.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Manager {
    Launchd,
    Systemd,
    Pidfile,
    None,
}

impl Manager {
    /// The bare word the script interpolated into its messages
    /// (`${DAEMON_MANAGER}`).
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Manager::Launchd => "launchd",
            Manager::Systemd => "systemd",
            Manager::Pidfile => "pidfile",
            Manager::None => "none",
        }
    }
}

/// Everything the ownership-detection block resolved.
pub struct Detected {
    pub use_launchd: bool,
    pub launchd_label: String,
    pub launchd_service: String,
    pub launchd_plist: PathBuf,
    pub is_linux_systemd: bool,
    pub systemd_unit: String,
    pub systemd_unit_path: String,
    pub manager: Manager,
    pub was_running: bool,
    /// The path the detected supervisor will actually exec, when it could be
    /// read from that supervisor's own config.
    pub supervisor_bin: Option<PathBuf>,
}

impl Detected {
    /// Resolve the launchd/systemd identities, then the owning manager.
    ///
    /// `pid_file` is the already-resolved `LOOM_PID_FILE`-first path (#6386),
    /// passed in rather than re-derived so this can never plan against a
    /// different file than the stop half acts on.
    pub fn probe(pid_file: &Path) -> Self {
        // ---- launchd (macOS) ----
        // `uname -s`, not `cfg!(target_os)`: the retained suite installs a
        // fake `uname` reporting Darwin so the launchd branches can be driven
        // on a Linux runner. Answering from the build target would make those
        // scenarios pass by never entering the branch they exist to test.
        let is_darwin = util::uname_s() == "Darwin";
        let mut use_launchd = is_darwin;
        if util::env_falsy("LOOM_DAEMON_LAUNCHD") {
            use_launchd = false;
        }
        let launchd_label = platform::launchd_label();
        // Resolve the domain ONLY when launchd interaction is on (#4130):
        // probing `launchctl print gui/<uid>` with LOOM_DAEMON_LAUNCHD=0 would
        // reach the machine-global domain the disabled path must never touch
        // (#4078). The placeholder is inert — every launchd call below
        // short-circuits on `use_launchd`.
        let launchd_service = if use_launchd {
            format!("{}/{launchd_label}", platform::launchd_domain())
        } else {
            format!("/{launchd_label}")
        };
        let launchd_plist =
            util::home().join(format!("Library/LaunchAgents/{launchd_label}.plist"));

        // ---- systemd --user (Linux) ----
        let is_linux_systemd =
            !util::env_falsy("LOOM_DAEMON_SYSTEMD") && platform::is_linux_systemd();
        let systemd_unit = if is_linux_systemd {
            platform::systemd_unit()
        } else {
            util::env_non_empty("LOOM_SYSTEMD_UNIT")
                .unwrap_or_else(|| "loom-daemon.service".to_string())
        };
        let systemd_unit_path = if is_linux_systemd {
            platform::systemd_unit_path().display().to_string()
        } else {
            String::new()
        };

        let mut d = Detected {
            use_launchd,
            launchd_label,
            launchd_service,
            launchd_plist,
            is_linux_systemd,
            systemd_unit,
            systemd_unit_path,
            manager: Manager::None,
            was_running: false,
            supervisor_bin: None,
        };

        if d.launchd_job_loaded() {
            d.manager = Manager::Launchd;
            d.was_running = true;
        } else if d.systemd_unit_loaded() {
            d.manager = Manager::Systemd;
            d.was_running = true;
        } else if pid_file.is_file() {
            let pid = std::fs::read_to_string(pid_file)
                .unwrap_or_default()
                .trim()
                .parse::<i32>()
                .unwrap_or(0);
            if pid != 0 && util::pid_alive(pid) {
                d.manager = Manager::Pidfile;
                d.was_running = true;
            }
        }

        if matches!(d.manager, Manager::Launchd | Manager::Systemd) {
            d.supervisor_bin = d.resolve_supervisor_bin();
        }
        d
    }

    /// `launchd_job_loaded`.
    #[must_use]
    pub fn launchd_job_loaded(&self) -> bool {
        if !self.use_launchd || !util::have("launchctl") {
            return false;
        }
        Command::new("launchctl")
            .arg("print")
            .arg(&self.launchd_service)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// `launchd_job_pid` — the first `pid = N` line, non-digits stripped.
    #[must_use]
    pub fn launchd_job_pid(&self) -> Option<String> {
        let out = Command::new("launchctl")
            .arg("print")
            .arg(&self.launchd_service)
            .stderr(Stdio::null())
            .output()
            .ok()?;
        parse_launchctl_pid(&String::from_utf8_lossy(&out.stdout))
    }

    /// `launchctl print <service> 2>&1` — for the diagnostic snapshot.
    #[must_use]
    pub fn launchctl_print_combined(&self) -> String {
        Command::new("launchctl")
            .arg("print")
            .arg(&self.launchd_service)
            .output()
            .map(|o| {
                let mut text = String::from_utf8_lossy(&o.stdout).to_string();
                text.push_str(&String::from_utf8_lossy(&o.stderr));
                text
            })
            .unwrap_or_default()
    }

    /// `systemd_unit_loaded`.
    #[must_use]
    pub fn systemd_unit_loaded(&self) -> bool {
        if !self.is_linux_systemd || !util::have("systemctl") {
            return false;
        }
        systemctl_status(&["--user", "is-active", "--quiet", &self.systemd_unit])
            || systemctl_status(&["--user", "is-enabled", "--quiet", &self.systemd_unit])
    }

    /// `systemd_unit_pid` — `systemctl --user show -p MainPID --value`.
    #[must_use]
    pub fn systemd_unit_pid(&self) -> Option<String> {
        systemctl_value("MainPID", &self.systemd_unit)
    }

    /// `systemd_unit_active_state`.
    #[must_use]
    pub fn systemd_unit_active_state(&self) -> String {
        systemctl_value("ActiveState", &self.systemd_unit).unwrap_or_default()
    }

    /// `systemd_unit_result`.
    #[must_use]
    pub fn systemd_unit_result(&self) -> String {
        systemctl_value("Result", &self.systemd_unit).unwrap_or_default()
    }

    /// `systemctl --user status <unit> --no-pager --full 2>&1`.
    #[must_use]
    pub fn systemctl_status_combined(&self) -> String {
        Command::new("systemctl")
            .args([
                "--user",
                "status",
                &self.systemd_unit,
                "--no-pager",
                "--full",
            ])
            .output()
            .map(|o| {
                let mut text = String::from_utf8_lossy(&o.stdout).to_string();
                text.push_str(&String::from_utf8_lossy(&o.stderr));
                text
            })
            .unwrap_or_default()
    }

    /// `describe_manager`.
    #[must_use]
    pub fn describe_manager(&self) -> String {
        match self.manager {
            Manager::Launchd => {
                format!("Running daemon manager: launchd (label {}).", self.launchd_label)
            }
            Manager::Systemd => {
                format!("Running daemon manager: systemd --user (unit {}).", self.systemd_unit)
            }
            Manager::Pidfile => {
                "Running daemon manager: PID-file/nohup (.loom/.daemon.pid).".to_string()
            }
            Manager::None => "Running daemon manager: not running.".to_string(),
        }
    }

    /// `resolve_supervisor_bin` (#6009).
    fn resolve_supervisor_bin(&self) -> Option<PathBuf> {
        match self.manager {
            Manager::Launchd => {
                // `[[ -r "$LAUNCHD_PLIST" ]]`, then
                // `command -v /usr/libexec/PlistBuddy`. The latter names an
                // ABSOLUTE path, which `command -v` answers by testing the file
                // itself rather than scanning `$PATH` — so this is an
                // executable test, not a PATH lookup.
                if !self.launchd_plist.is_file() {
                    return None;
                }
                if !util::is_executable(Path::new("/usr/libexec/PlistBuddy")) {
                    return None;
                }
                let out = Command::new("/usr/libexec/PlistBuddy")
                    .arg("-c")
                    .arg("Print :ProgramArguments:0")
                    .arg(&self.launchd_plist)
                    .stderr(Stdio::null())
                    .output()
                    .ok()?;
                let bin = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
                if bin.is_empty() {
                    return None;
                }
                Some(PathBuf::from(bin))
            }
            Manager::Systemd => {
                if self.systemd_unit_path.is_empty() {
                    return None;
                }
                let text = std::fs::read_to_string(&self.systemd_unit_path).ok()?;
                // The LAST `ExecStart=` wins if the unit somehow has more than
                // one (systemd's own override semantics), and only the binary
                // token is taken — `render_systemd_unit` writes `ExecStart=<bin>`
                // with no args, but a hand-edited unit may carry some.
                let line = text.lines().rfind(|l| l.starts_with("ExecStart="))?;
                let value = line.strip_prefix("ExecStart=").unwrap_or(line);
                let first = value.split_whitespace().next()?;
                if first.is_empty() {
                    return None;
                }
                Some(PathBuf::from(first))
            }
            _ => None,
        }
    }
}

fn systemctl_status(args: &[&str]) -> bool {
    Command::new("systemctl")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn systemctl_value(property: &str, unit: &str) -> Option<String> {
    let out = Command::new("systemctl")
        .args(["--user", "show", "-p", property, "--value", unit])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// The `awk -F'= ' '/^[[:space:]]*pid = /{gsub(/[^0-9]/, "", $2); print $2; exit}'`
/// the script used on `launchctl print` output.
///
/// Reproduced rather than delegated to [`crate::restart_verify::parse_launchctl_pid`]:
/// this one returns the digits as a STRING, including the empty string a
/// `pid = ` line with no digits would yield, and the caller compares it to the
/// pre-restart value textually. A typed `Option<u32>` collapses "no pid line"
/// and "a pid line with nothing parseable" into the same answer.
#[must_use]
pub fn parse_launchctl_pid(output: &str) -> Option<String> {
    for line in output.lines() {
        let trimmed = line.trim_start_matches([' ', '\t']);
        if !trimmed.starts_with("pid = ") {
            continue;
        }
        // `-F'= '` splits on the literal two characters; `$2` is everything
        // between the first and second separator, or the rest of the line.
        let mut fields = line.split("= ");
        let _ = fields.next();
        let field2 = fields.next().unwrap_or("");
        let digits: String = field2.chars().filter(char::is_ascii_digit).collect();
        return Some(digits);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launchctl_pid_parse_takes_the_first_pid_line_only() {
        let text = "\tstate = running\n\tpid = 4242\n\tlast exit status = 0\n\tpid = 9999\n";
        assert_eq!(parse_launchctl_pid(text).as_deref(), Some("4242"));
        assert_eq!(parse_launchctl_pid("\tstate = not running\n"), None);
        // A `pid = ` line with no digits yields the empty string, not None —
        // the caller's `[[ -n "$cur_pid" ]]` test is what rejects it, and
        // collapsing the two here would change which branch it takes.
        assert_eq!(parse_launchctl_pid("\tpid = none\n").as_deref(), Some(""));
    }

    #[test]
    fn the_manager_word_is_what_the_messages_interpolate() {
        assert_eq!(Manager::Launchd.word(), "launchd");
        assert_eq!(Manager::Systemd.word(), "systemd");
        assert_eq!(Manager::Pidfile.word(), "pidfile");
        assert_eq!(Manager::None.word(), "none");
    }
}
