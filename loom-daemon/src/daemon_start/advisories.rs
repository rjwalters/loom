//! The advisory line a successful start prints. It cannot fail a start:
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
///   reading anything.
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
    fn an_empty_config_value_falls_back_to_the_default() {
        // `[[ -z "$value" ]]` could not tell `""` from a missing key.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".loom")).expect("mkdir");
        std::fs::write(dir.path().join(".loom/config.json"), r#"{"host": {"preventSleep": ""}}"#)
            .expect("write");
        if platform::have("jq") {
            assert_eq!(config_get(dir.path(), "host.preventSleep", "fallback"), "fallback");
        }
    }
}
