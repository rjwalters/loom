//! `perform_relaunch` / `perform_systemd_relaunch` — the opted-in exit-6
//! fallback (#4118, #4260 sub-issue C), plus the env harvest both depend on.
//!
//! The exit-6 fallback USED to tell the operator to `launchctl bootstrap` the
//! EXISTING plist. That plist is stale by construction — it is the pre-#4077
//! file that caused the refused restart — so bootstrapping it relaunched
//! WITHOUT `KeepAlive:SuccessfulExit` and WITHOUT `LOOM_DAEMON_SUPERVISOR`,
//! and the next roll refused identically, forever. The correct fix is to
//! RE-RENDER via `loom-daemon-start.sh`, which hardcodes both supervised keys.
//!
//! The harvest is what keeps that re-render from silently narrowing autonomy
//! to FLAGS-OFF defaults (#4011): the live plist's / unit's own `LOOM_*` and
//! token values are read back and re-exported before the start wrapper runs.
//! A harvest that FAILS must never be mistaken for "there was nothing to
//! preserve" — it returns 6 and refuses to relaunch at all.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

use super::out;
use super::supervisor::Detected;
use super::util;

/// The key allowlist `render_launchd_plist` / `render_systemd_unit`
/// (`loom-daemon-start.sh`) actually forward, mirrored from
/// `lib/daemon-env-harvest.sh` so the two can never disagree about which keys
/// are preserved.
///
/// `PATH` and `HOME` are excluded because the start wrapper resolves `PATH`
/// deterministically — round-tripping the plist's `PATH` here would grow the
/// string on every roll and re-introduce the non-deterministic-render bug
/// #4172 fixed. `LOOM_DAEMON_SUPERVISOR` is excluded because the start wrapper
/// hardcodes it.
fn is_forwarded_key(key: &str) -> bool {
    if key == "LOOM_DAEMON_SUPERVISOR" {
        return false;
    }
    if matches!(key, "GH_TOKEN" | "GITEA_TOKEN" | "FORGE_TOKEN") {
        return true;
    }
    // jq's `^LOOM_[A-Za-z0-9_]*$` — everything after `LOOM_` is a word
    // character (an empty run is allowed, so a bare `LOOM_` matches).
    key.strip_prefix("LOOM_")
        .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

/// `harvest_plist_env <plist>` — the live plist's forwarded env, or `None`
/// for the `return 2` cases.
///
/// The `plutil` AND `jq` availability test is reproduced even though the JSON
/// is parsed in-process here. It is observable behaviour, not an
/// implementation detail: on a macOS host with `plutil` but no `jq` the shell
/// refused the relaunch rather than re-rendering, and quietly starting to
/// succeed there would widen what `--relaunch` does on a host the shell
/// deliberately stopped on.
pub fn harvest_plist_env(plist: &Path) -> Option<BTreeMap<String, String>> {
    if !plist.is_file() {
        out::say_err(&format!(
            "Cannot harvest launchd env: plist not found at {}",
            plist.display()
        ));
        return None;
    }
    if !util::have("plutil") || !util::have("jq") {
        out::say_err("Cannot harvest launchd env: plutil and jq are both required on the macOS launchd path.");
        return None;
    }
    let json = Command::new("plutil")
        .args(["-convert", "json", "-o", "-"])
        .arg(plist)
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string());
    let Some(json) = json else {
        out::say_err(&format!(
            "Cannot harvest launchd env: plist at {} is not parseable by plutil.",
            plist.display()
        ));
        return None;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&json) else {
        out::say_err(&format!(
            "Cannot harvest launchd env: failed to extract EnvironmentVariables from {}.",
            plist.display()
        ));
        return None;
    };
    let mut harvested = BTreeMap::new();
    if let Some(dict) = value
        .get("EnvironmentVariables")
        .and_then(|v| v.as_object())
    {
        for (key, raw) in dict {
            if !is_forwarded_key(key) {
                continue;
            }
            // plist EnvironmentVariables values are strings by definition;
            // anything else is rendered rather than dropped, so a
            // hand-mangled plist still round-trips a visible value instead of
            // silently losing a key.
            let value = match raw {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            harvested.insert(key.clone(), value);
        }
    }
    Some(harvested)
}

/// `harvest_unit_env <unit_path>` — the live unit's `Environment=` lines,
/// same allowlist, no base64 (a systemd `Environment=` line is single-valued).
pub fn harvest_unit_env(unit_path: &Path) -> Option<BTreeMap<String, String>> {
    if !unit_path.is_file() {
        out::say_err(&format!(
            "Cannot harvest systemd unit env: unit file not found at {}",
            unit_path.display()
        ));
        return None;
    }
    let text = std::fs::read_to_string(unit_path).unwrap_or_default();
    let mut harvested = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("Environment=") else {
            continue;
        };
        // `${rest%%=*}` / `${rest#*=}` — the FIRST `=` splits, so a value
        // containing `=` survives intact.
        let Some((key, value)) = rest.split_once('=') else {
            continue;
        };
        if is_forwarded_key(key) {
            harvested.insert(key.to_string(), value.to_string());
        }
    }
    Some(harvested)
}

/// `perform_relaunch <plist>` — returns the start wrapper's exit code, or 6
/// when the harvest failed.
pub fn perform_relaunch(sup: &Detected, start_script: &Path) -> i32 {
    out::say("--relaunch: re-rendering the LaunchAgent and relaunching under launchd supervision.");

    // 1. Preserve the live plist's autonomy/auth env across the re-render.
    let Some(harvested) = harvest_plist_env(&sup.launchd_plist) else {
        out::err("Refusing to relaunch: could not read the live plist's EnvironmentVariables.");
        out::err("Relaunching now would silently narrow the autonomy flags to FLAGS-OFF defaults (#4011) — aborting.");
        return 6;
    };
    let count = export_all(&harvested);
    out::say(&format!(
        "Preserved {count} LOOM_*/token env var(s) from the live plist across the re-render (PATH/HOME/LOOM_DAEMON_SUPERVISOR excluded by design)."
    ));

    // 2. Stop the old daemon GRACEFULLY with SIGTERM rather than calling
    //    `launchctl bootout` directly. Bootout itself no longer kills
    //    in-flight sweeps (#5081 — every sweep runs in its own process group
    //    and reparents to pid 1); this is belt-and-braces against a DOUBLE
    //    bootout, since the start wrapper's own launchd block bootouts the
    //    loaded job again before re-bootstrapping. `kill -TERM` makes the
    //    daemon exit non-zero, so the stale plist's `KeepAlive=false` does not
    //    relaunch it.
    if let Some(pid) = sup.launchd_job_pid().and_then(|p| p.parse::<i32>().ok()) {
        if util::pid_alive(pid) {
            out::say(&format!(
                "Sending SIGTERM to the running daemon (pid {pid}) — sweep children reparent and keep working; in-flight sweeps are not otherwise at risk here (bootout no longer kills them either, #5081)."
            ));
            sigterm_and_wait(pid);
        }
    }

    // 3. Re-render + bootstrap via loom-daemon-start.sh. In launchd mode the
    //    plist's EnvironmentVariables — not .daemon.flags — is the durable
    //    config, so no flags are passed.
    out::say(&format!(
        "Invoking {} to re-render the supervised plist and relaunch.",
        start_script.display()
    ));
    run_start_script(start_script)
}

/// `perform_systemd_relaunch <unit_path> <unit>`.
///
/// Re-rendering the unit file alone does not make an ALREADY-ACTIVE unit pick
/// up the new binary/env — `enable --now` on an active unit is a no-op start,
/// not a restart. So this SIGTERMs the running daemon first (`Restart=on-success`
/// does not fire on a signal death, mirroring launchd's
/// `KeepAlive:SuccessfulExit`), leaving the unit inactive, and THEN invokes the
/// start wrapper, which against an inactive unit genuinely starts a fresh
/// process. Same effect as `systemctl --user restart`, while reusing
/// `render_systemd_unit` rather than duplicating it.
pub fn perform_systemd_relaunch(sup: &Detected, start_script: &Path) -> i32 {
    out::say(&format!(
        "--relaunch: re-rendering the systemd --user unit {} and relaunching under supervision.",
        sup.systemd_unit
    ));

    let Some(harvested) = harvest_unit_env(Path::new(&sup.systemd_unit_path)) else {
        out::err("Refusing to relaunch: could not read the live unit's Environment= values.");
        out::err("Relaunching now would silently narrow the autonomy flags to FLAGS-OFF defaults (#4011) — aborting.");
        return 6;
    };
    let count = export_all(&harvested);
    out::say(&format!(
        "Preserved {count} LOOM_*/token env var(s) from the live unit across the re-render (PATH/HOME/LOOM_DAEMON_SUPERVISOR excluded by design)."
    ));

    // Stop the old daemon GRACEFULLY so its sweep children reparent and keep
    // working, instead of `systemctl stop` (which SIGKILLs the whole cgroup
    // after TimeoutStopSec, tearing down sweep children the way a launchd
    // bootout was once believed to).
    if let Some(pid) = sup
        .systemd_unit_pid()
        .and_then(|p| p.parse::<i32>().ok())
        .filter(|p| *p != 0)
    {
        if util::pid_alive(pid) {
            out::say(&format!(
                "Sending SIGTERM to the running daemon (pid {pid}) — sweep children reparent and keep working (NOT 'systemctl stop', which tears down the whole cgroup)."
            ));
            sigterm_and_wait(pid);
        }
    }

    out::say(&format!(
        "Invoking {} to re-render the supervised unit and relaunch.",
        start_script.display()
    ));
    run_start_script(start_script)
}

/// `export "$k=$v"` for each harvested pair; returns the count.
///
/// Process-global, exactly as `export` was: the start wrapper is spawned from
/// this process and must inherit them.
fn export_all(harvested: &BTreeMap<String, String>) -> usize {
    for (key, value) in harvested {
        if key.is_empty() {
            continue;
        }
        // SAFETY: single-threaded at this point in the run.
        unsafe {
            std::env::set_var(key, value);
        }
    }
    harvested.iter().filter(|(k, _)| !k.is_empty()).count()
}

/// `kill -TERM <pid>`, then up to five 1s waits for it to go away.
fn sigterm_and_wait(pid: i32) {
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    for _ in 0..5 {
        if !util::pid_alive(pid) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// `"$START_SCRIPT"` with stdio inherited — its exit code is the relaunch's.
fn run_start_script(start_script: &Path) -> i32 {
    Command::new(start_script)
        .status()
        .ok()
        .and_then(|s| s.code())
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_forwarded_key_allowlist_matches_the_shell_libs() {
        for key in [
            "LOOM_WORK_FINDER",
            "LOOM_MAIN_HEALTH_GATE",
            "LOOM_",
            "GH_TOKEN",
            "GITEA_TOKEN",
            "FORGE_TOKEN",
        ] {
            assert!(is_forwarded_key(key), "{key}");
        }
        for key in [
            "PATH",
            "HOME",
            "LOOM_DAEMON_SUPERVISOR",
            "LOOM-DASH",
            "OTHER",
        ] {
            assert!(!is_forwarded_key(key), "{key}");
        }
    }

    #[test]
    fn a_unit_env_value_containing_an_equals_sign_survives() {
        let tmp = std::env::temp_dir().join(format!("loom-update-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let unit = tmp.join("x.service");
        std::fs::write(
            &unit,
            "[Service]\nEnvironment=PATH=/sentinel\nEnvironment=LOOM_A=1=2\nEnvironment=LOOM_DAEMON_SUPERVISOR=systemd\nExecStart=/x\n",
        )
        .unwrap();
        let harvested = harvest_unit_env(&unit).unwrap();
        assert_eq!(harvested.get("LOOM_A").map(String::as_str), Some("1=2"));
        assert!(!harvested.contains_key("PATH"));
        assert!(!harvested.contains_key("LOOM_DAEMON_SUPERVISOR"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn an_absent_unit_file_is_an_error_not_an_empty_harvest() {
        // "Never silently return an empty set" is the #4011 contract: an empty
        // harvest would let the caller re-render into FLAGS-OFF defaults.
        assert!(harvest_unit_env(Path::new("/definitely/not/here.service")).is_none());
    }
}
