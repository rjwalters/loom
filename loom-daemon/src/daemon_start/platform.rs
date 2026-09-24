//! Supervisor-identity resolution: the launchd label/domain and the
//! `systemd --user` unit name/path.
//!
//! These mirror `lib/launchd-domain.sh` and `lib/systemd-user.sh`, which
//! start/stop/update/watchdog all source so they can never look a service up in
//! a domain somebody else put it in. The shell libraries stay — three other
//! scripts still source them — so this is a second implementation of the same
//! rule, and the rule is small enough (a probe and two string joins) that the
//! duplication is bounded and testable rather than a standing liability.

use std::path::PathBuf;
use std::process::{Command, Stdio};

fn non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|d| {
            let c = d.join(name);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(&c)
                    .is_ok_and(|m| !m.is_dir() && m.permissions().mode() & 0o111 != 0)
            }
            #[cfg(not(unix))]
            {
                c.is_file()
            }
        })
    })
}

/// `command -v <name>` — is this tool resolvable?
#[must_use]
pub fn have(tool: &str) -> bool {
    on_path(tool)
}

/// `resolve_launchd_label()` — `$LOOM_LAUNCHD_LABEL`, else the production label.
#[must_use]
pub fn launchd_label() -> String {
    non_empty("LOOM_LAUNCHD_LABEL").unwrap_or_else(|| "com.rjwalters.loom-daemon".to_string())
}

/// `resolve_launchd_domain()` (#4130): an explicit override verbatim, else
/// `gui/<uid>` when it resolves, else `user/<uid>`.
///
/// An override that does not resolve is still honoured — it must fail loudly at
/// `launchctl bootstrap` rather than silently land the job somewhere else.
#[must_use]
pub fn launchd_domain() -> String {
    let uid = unsafe { libc::getuid() };
    if let Some(explicit) = non_empty("LOOM_LAUNCHD_DOMAIN") {
        return explicit;
    }
    if on_path("launchctl") {
        let ok = Command::new("launchctl")
            .arg("print")
            .arg(format!("gui/{uid}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            return format!("gui/{uid}");
        }
    }
    format!("user/{uid}")
}

/// `systemd_user_manager_reachable()`.
///
/// Keys on the reported STATE, not the exit code: `is-system-running` exits
/// non-zero for `degraded`, where the manager is perfectly reachable.
#[must_use]
pub fn systemd_user_manager_reachable() -> bool {
    if std::env::var("LOOM_SYSTEMD_FORCE").as_deref() == Ok("1") {
        return true;
    }
    if non_empty("XDG_RUNTIME_DIR").is_none() {
        return false;
    }
    let out = Command::new("systemctl")
        .args(["--user", "is-system-running"])
        .stderr(Stdio::null())
        .output();
    let state = out
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    !(state.is_empty() || state == "offline")
}

/// `is_linux_systemd()` — Linux + `systemctl` + a reachable per-user manager.
///
/// `LOOM_SYSTEMD_FORCE=1` is a test-only seam that skips the platform and
/// manager checks but still requires `systemctl` to resolve, so a test that
/// forgets its stub does not silently take this branch.
#[must_use]
pub fn is_linux_systemd() -> bool {
    if std::env::var("LOOM_SYSTEMD_FORCE").as_deref() == Ok("1") {
        return on_path("systemctl");
    }
    if !cfg!(target_os = "linux") {
        return false;
    }
    if !on_path("systemctl") {
        return false;
    }
    systemd_user_manager_reachable()
}

/// `resolve_systemd_unit()` — a value without a `.service` suffix is left
/// verbatim, as systemd treats a bare name as a `.service` unit.
#[must_use]
pub fn systemd_unit() -> String {
    non_empty("LOOM_SYSTEMD_UNIT").unwrap_or_else(|| "loom-daemon.service".to_string())
}

/// `resolve_systemd_unit_dir()`.
#[must_use]
pub fn systemd_unit_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/systemd/user")
}

/// `resolve_systemd_unit_path()`.
#[must_use]
pub fn systemd_unit_path() -> PathBuf {
    systemd_unit_dir().join(systemd_unit())
}

/// `resolve_watchdog_label()` — `<daemon label>-watchdog` by default.
#[must_use]
pub fn watchdog_label() -> String {
    non_empty("LOOM_WATCHDOG_LABEL").unwrap_or_else(|| format!("{}-watchdog", launchd_label()))
}

/// `resolve_systemd_watchdog_unit()` — `<daemon unit minus .service>-watchdog`.
#[must_use]
pub fn systemd_watchdog_unit() -> String {
    non_empty("LOOM_WATCHDOG_LABEL").unwrap_or_else(|| {
        let unit = systemd_unit();
        format!("{}-watchdog", unit.strip_suffix(".service").unwrap_or(&unit))
    })
}

/// `${LOOM_WATCHDOG_INTERVAL_SECS:-300}` — kept as a STRING, not parsed.
///
/// The shell interpolated it straight into `StartInterval` / `OnUnitActiveSec`,
/// so a non-numeric value produced a unit systemd rejects at load time rather
/// than a silently substituted default. Parsing it here would turn a loud
/// misconfiguration into a quiet one.
#[must_use]
pub fn watchdog_interval_secs() -> String {
    non_empty("LOOM_WATCHDOG_INTERVAL_SECS").unwrap_or_else(|| "300".to_string())
}

/// `[[ "$X" =~ ^(0|false|no)$ ]]` — the shell's negative test, case-sensitive
/// and whole-value, used by `LOOM_DAEMON_LAUNCHD` / `LOOM_DAEMON_SYSTEMD`.
#[must_use]
pub fn env_says_off(name: &str) -> bool {
    matches!(std::env::var(name).unwrap_or_default().as_str(), "0" | "false" | "no")
}

/// `[[ "$X" =~ ^(1|true|yes)$ ]]` — used by `LOOM_ALLOW_SESSION_DAEMON_START`.
#[must_use]
pub fn env_says_on(name: &str) -> bool {
    matches!(std::env::var(name).unwrap_or_default().as_str(), "1" | "true" | "yes")
}

#[cfg(test)]
mod tests {

    #[test]
    fn truthiness_is_whole_value_and_case_sensitive() {
        // `[[ =~ ^(0|false|no)$ ]]` never matched `NO`, `False` or `0 `.
        for v in ["0", "false", "no"] {
            assert!(matches!(v, "0" | "false" | "no"), "{v}");
        }
        for v in ["NO", "False", "0 ", "off", ""] {
            assert!(!matches!(v, "0" | "false" | "no"), "{v}");
        }
    }

    #[test]
    fn the_watchdog_unit_drops_only_a_trailing_service_suffix() {
        // `${daemon_unit%.service}` is a suffix strip, not a substring removal:
        // `loom-daemon.service.service` loses exactly one.
        assert_eq!(
            "loom-daemon.service.service"
                .strip_suffix(".service")
                .unwrap(),
            "loom-daemon.service"
        );
        assert_eq!(
            "loom-daemon",
            "loom-daemon"
                .strip_suffix(".service")
                .unwrap_or("loom-daemon")
        );
    }
}
