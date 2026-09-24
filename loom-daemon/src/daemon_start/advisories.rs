//! The two advisory lines a successful start prints, plus the config resolver
//! they share.
//!
//! Neither can fail a start. `print_safehouse_status` is a purely **static**,
//! pre-connect check — "would the daemon even try?" — because proving a live
//! connection needs the daemon's own socket, which `loom-daemon status` covers.
//! `print_calibrate_hint` is bounded and silent on every error.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::out;
use super::platform;

/// `loom_config_get <repo_root> <dotted> <default>`.
///
/// Two behaviours are preserved that a straight port of the Rust resolver would
/// drop, and both are observable:
///
/// * **No `jq` ⇒ the default.** The shell resolver short-circuits before
///   reading anything. On a host without `jq`, `safehouse.enabled` therefore
///   resolved to `false` and the start printed "not configured" no matter what
///   the file said. Resolving it natively here would make this line say
///   something new on exactly the hosts least able to explain why.
/// * **An empty value ⇒ the default.** `[[ -z "$value" ]]` cannot tell an
///   explicit `""` from a missing key.
fn config_get(repo_root: &Path, dotted: &str, default: &str) -> String {
    if !platform::have("jq") {
        return default.to_string();
    }
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let value = crate::config_resolver::get_path(&effective, dotted);
    let rendered = match value {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    };
    if rendered.is_empty() {
        default.to_string()
    } else {
        rendered
    }
}

/// `_loom_mcp_truthy` — lower-cased, whitespace-stripped, then `1|true|yes|on`.
fn mcp_truthy(raw: &str) -> bool {
    let normalised: String = raw
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    matches!(normalised.as_str(), "1" | "true" | "yes" | "on")
}

fn safehouse_enabled(repo_root: &Path) -> bool {
    // `${VAR+set}` — an explicitly EMPTY value disables, matching the daemon's
    // own `env_bool`. `var_os().is_some()` is the same test.
    if let Some(v) = std::env::var_os("LOOM_SAFEHOUSE_ENABLED") {
        return mcp_truthy(&v.to_string_lossy());
    }
    mcp_truthy(&config_get(repo_root, "safehouse.enabled", "false"))
}

fn safehouse_socket(repo_root: &Path) -> String {
    for name in ["LOOM_SAFEHOUSE_SOCKET", "SAFEHOUSED_SOCKET"] {
        if let Ok(v) = std::env::var(name) {
            if !v.is_empty() {
                return v;
            }
        }
    }
    let cfg = config_get(repo_root, "safehouse.socket", "");
    if !cfg.is_empty() && cfg != "null" {
        return cfg;
    }
    String::new()
}

/// `print_safehouse_status()` (#4345, caveat #4464/#4225).
pub fn print_safehouse_status(repo_root: &Path) {
    if !safehouse_enabled(repo_root) {
        out::say("Safehouse:     not configured (safehouse.enabled is false/absent)");
        return;
    }
    let socket = safehouse_socket(repo_root);
    if socket.is_empty() {
        out::warn(
            "Safehouse:     configured, unreachable (enabled but no socket path resolved -- set safehouse.socket, $LOOM_SAFEHOUSE_SOCKET, or $SAFEHOUSED_SOCKET)",
        );
        return;
    }
    if is_socket(&socket) {
        out::ok(&format!(
            "Safehouse:     configured (socket present at {socket}) -- see 'loom-daemon status' for live connection state"
        ));
        // #4464: omitting `safehouse.room` is only valid when safehoused joined
        // exactly ONE room. #4225: attention-class routing can supply the room
        // instead via `safehouse.rooms.signal`, so the caveat must not fire for
        // a host that set only that.
        let room = env_or_config(repo_root, "LOOM_SAFEHOUSE_ROOM", "safehouse.room");
        let signal =
            env_or_config(repo_root, "LOOM_SAFEHOUSE_ROOM_SIGNAL", "safehouse.rooms.signal");
        if room.is_empty() && signal.is_empty() {
            out::say(
                "               note: safehouse.room is unset (and so is safehouse.rooms.signal) -- valid only if safehoused joined exactly one room; a multi-room host needs an explicit room id or every send is rejected",
            );
        }
    } else {
        out::warn(&format!(
            "Safehouse:     configured, unreachable (socket {socket} does not exist -- is safehoused running?)"
        ));
    }
}

/// `${ENV:-$(loom_config_get …)}` — an empty env value falls through to config.
fn env_or_config(repo_root: &Path, env_name: &str, dotted: &str) -> String {
    match std::env::var(env_name) {
        Ok(v) if !v.is_empty() => v,
        _ => config_get(repo_root, dotted, ""),
    }
}

/// `[[ -S "$socket" ]]`.
fn is_socket(path: &str) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        std::fs::metadata(path).is_ok_and(|m| m.file_type().is_socket())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// `print_calibrate_hint()` (#4390, re-based #4512, bounded #4799).
///
/// Bounded because a `$DAEMON_BIN` with no `calibrate` handler at all — a test
/// fixture stub, or a future breaking CLI change — made the shell's `$(...)`
/// block forever, and a signal arriving while a shell is blocked inside a
/// command substitution is deferred until it returns. The `jq` gate is kept for
/// the same reason as in [`config_get`]: without `jq` the shell returned before
/// running `calibrate` at all, so a host without it never paid for the probe.
pub fn print_calibrate_hint(daemon_bin: &Path, repo_root: &Path) {
    if !platform::have("jq") {
        return;
    }
    let budget = calibrate_timeout_secs();
    let Some(json) = bounded_output(daemon_bin, repo_root, budget) else {
        return;
    };
    if json.is_empty() {
        return;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) else {
        return;
    };
    if v.get("binding_term").and_then(serde_json::Value::as_str) != Some("ceiling") {
        return;
    }
    let measurements = v.get("measurements");
    // `jq -r … // empty` renders a number without quotes; the shell then
    // required `^[0-9]+$`, so a float or a negative ceiling bailed out.
    let ceiling = measurements
        .and_then(|m| m.get("configured_max_concurrent"))
        .map(render_scalar)
        .unwrap_or_default();
    if !ceiling.bytes().all(|b| b.is_ascii_digit()) || ceiling.is_empty() {
        return;
    }
    let Ok(ceiling_n) = ceiling.parse::<u64>() else {
        return;
    };
    if ceiling_n == 0 {
        return;
    }
    let idle = measurements
        .and_then(|m| m.get("cpu_idle_fraction"))
        .and_then(serde_json::Value::as_f64);
    let Some(idle) = idle else { return };
    // `($f * 100) | floor` — a negative fraction floors negative and then fails
    // the `^[0-9]+$` guard.
    let idle_pct = (idle * 100.0).floor();
    if idle_pct < 0.0 {
        return;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let idle_pct = idle_pct as u64;

    // 50% mirrors `calibrate::IDLE_HEADROOM_FRACTION` — the "grossly
    // under-subscribed" bar (#4512's motivating host measured 95% idle at cap 2).
    if idle_pct >= 50 {
        out::warn(&format!(
            "maxConcurrent {ceiling} binds while the host is {idle_pct}% idle -- consider raising autonomous.workFinder.maxConcurrent ('loom-daemon calibrate' for the full reading)"
        ));
    }
}

fn render_scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `CALIBRATE_HINT_TIMEOUT_SECS` — `${LOOM_CALIBRATE_HINT_TIMEOUT_SECS:-5}`,
/// with a non-numeric value reset to 5.
fn calibrate_timeout_secs() -> u64 {
    std::env::var("LOOM_CALIBRATE_HINT_TIMEOUT_SECS")
        .ok()
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse().ok())
        .unwrap_or(5)
}

/// `bounded_run <secs> "$DAEMON_BIN" calibrate --workspace <root> --json`.
///
/// Returns `None` on any non-zero exit, on a timeout, or when the child could
/// not be spawned — every one of which the shell turned into `|| return 0`.
fn bounded_output(daemon_bin: &Path, repo_root: &Path, budget_secs: u64) -> Option<String> {
    let mut child = Command::new(daemon_bin)
        .arg("calibrate")
        .arg("--workspace")
        .arg(repo_root)
        .arg("--json")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + Duration::from_secs(budget_secs);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut buf = String::new();
                if let Some(mut so) = child.stdout.take() {
                    use std::io::Read;
                    let _ = so.read_to_string(&mut buf);
                }
                return if status.success() { Some(buf) } else { None };
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    // The `-k 2` escalation exists because a child blocked in a
                    // foreground read defers SIGTERM; kill outright and report
                    // the timeout as "no hint", exactly like `|| return 0`.
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

/// The advisory host-sleep check (#3350) — `"$SLEEP_CHECK" || true`.
pub fn run_host_sleep_check(repo_root: &Path) {
    let installed = repo_root.join(".loom/scripts/check-host-sleep.sh");
    let script = if is_executable(&installed) {
        installed
    } else {
        repo_root.join("defaults/scripts/check-host-sleep.sh")
    };
    if !is_executable(&script) {
        return;
    }
    let _ = Command::new(&script).status();
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| !m.is_dir() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// `loom_host_prevent_sleep_enabled <repo_root>` (#6311).
///
/// Never fails a caller: a malformed value at any tier warns and falls back to
/// disabled.
#[must_use]
pub fn host_prevent_sleep_enabled(repo_root: &Path) -> bool {
    let (raw, desc) = if let Some(v) = std::env::var_os("LOOM_HOST_PREVENT_SLEEP") {
        (v.to_string_lossy().into_owned(), "$LOOM_HOST_PREVENT_SLEEP".to_string())
    } else {
        (
            config_get(repo_root, "host.preventSleep", ""),
            "host.preventSleep (resolved config)".to_string(),
        )
    };
    let lower = raw.to_lowercase();
    match lower.as_str() {
        "1" | "true" | "yes" | "on" => true,
        "" | "0" | "false" | "no" | "off" => false,
        _ => {
            out::say_err(&format!(
                "[host-sleep-config] WARNING: malformed value for {desc} ('{raw}'); expected true/false. Falling back to disabled (this knob never blocks a sweep, issue #6311)."
            ));
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthy_matches_the_shells_normalisation() {
        for v in ["1", "TRUE", " yes ", "On"] {
            assert!(mcp_truthy(v), "{v}");
        }
        for v in ["", "0", "false", "enabled", "y"] {
            assert!(!mcp_truthy(v), "{v}");
        }
    }

    #[test]
    fn an_empty_config_value_falls_back_to_the_default() {
        // `[[ -z "$value" ]]` could not tell `""` from a missing key, and the
        // safehouse socket resolution depends on that.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".loom")).expect("mkdir");
        std::fs::write(dir.path().join(".loom/config.json"), r#"{"safehouse": {"socket": ""}}"#)
            .expect("write");
        if platform::have("jq") {
            assert_eq!(config_get(dir.path(), "safehouse.socket", "fallback"), "fallback");
        }
    }
}
