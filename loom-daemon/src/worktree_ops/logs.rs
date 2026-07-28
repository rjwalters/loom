//! `loom-daemon cleanup logs`: native port of `loom-cleanup logs` (`cleanup.py`).
//!
//! The only surviving cleanup.py functionality post-daemon-brain (#3396) is
//! log archival, which delegates to `archive-logs.sh` — this port keeps that
//! delegation model unchanged rather than reimplementing archival in Rust.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Resolve `env_var` as a bool: `env_bool` mirrors
/// `loom_tools.common.config.env_bool` — unset falls back to `default`;
/// `"0"`/`"false"`/`"no"`/`"off"` (any case) is `false`, anything else `true`.
fn env_bool(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
        Err(_) => default,
    }
}

fn env_int(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn find_archive_logs_script(repo_root: &Path) -> Option<PathBuf> {
    for candidate in [
        repo_root.join("scripts").join("archive-logs.sh"),
        repo_root
            .join(".loom")
            .join("scripts")
            .join("archive-logs.sh"),
    ] {
        if candidate.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = std::fs::metadata(&candidate) {
                    if meta.permissions().mode() & 0o111 != 0 {
                        return Some(candidate);
                    }
                    continue;
                }
            }
            #[cfg(not(unix))]
            return Some(candidate);
        }
    }
    None
}

/// Delegate to `archive-logs.sh`. Returns the subprocess exit code (0 on
/// success); failures are logged to stderr but not treated as fatal by the
/// caller (mirrors `cleanup.py::run_archive_logs`).
pub fn run_archive_logs(
    repo_root: &Path,
    dry_run: bool,
    prune_only: bool,
    retention_days: Option<i64>,
) -> i32 {
    let Some(script) = find_archive_logs_script(repo_root) else {
        eprintln!("archive-logs.sh not found in scripts/ or .loom/scripts/");
        return 1;
    };

    let mut cmd = Command::new(&script);
    cmd.current_dir(repo_root);
    if dry_run {
        cmd.arg("--dry-run");
    }
    if prune_only {
        cmd.arg("--prune-only");
    }
    if let Some(days) = retention_days {
        cmd.arg("--retention-days").arg(days.to_string());
    }

    match cmd.output() {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines() {
                println!("{line}");
            }
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                eprintln!(
                    "archive-logs.sh exited {}: {}",
                    out.status.code().unwrap_or(-1),
                    stderr.trim()
                );
            }
            out.status.code().unwrap_or(1)
        }
        Err(e) => {
            eprintln!("archive-logs.sh execution failed: {e}");
            1
        }
    }
}

/// `loom-daemon cleanup logs [--dry-run] [--prune-only] [--retention-days N]`.
/// Mirrors `cleanup.py::handle_logs` + its `LOOM_CLEANUP_ENABLED` /
/// `LOOM_ARCHIVE_LOGS` / `LOOM_RETENTION_DAYS` env-var gates.
pub fn handle_logs(
    repo_root: &Path,
    dry_run: bool,
    prune_only: bool,
    retention_days: Option<i64>,
) -> i32 {
    if !env_bool("LOOM_CLEANUP_ENABLED", true) {
        println!(
            "Cleanup disabled (LOOM_CLEANUP_ENABLED={:?})",
            std::env::var("LOOM_CLEANUP_ENABLED").ok()
        );
        return 0;
    }

    println!("Log Archival Cleanup");

    let archive_logs_enabled = env_bool("LOOM_ARCHIVE_LOGS", true);
    if !archive_logs_enabled && !prune_only {
        println!("Log archival disabled (LOOM_ARCHIVE_LOGS=0); skipping");
        return 0;
    }

    let effective_retention = retention_days.or_else(|| Some(env_int("LOOM_RETENTION_DAYS", 7)));
    let rc = run_archive_logs(repo_root, dry_run, prune_only, effective_retention);
    if rc == 0 {
        println!("Log archival complete");
    }
    rc
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn env_bool_defaults_and_overrides() {
        std::env::remove_var("LOOM_TEST_BOOL_FLAG");
        assert!(env_bool("LOOM_TEST_BOOL_FLAG", true));
        for off in ["0", "false", "no", "off", "OFF"] {
            std::env::set_var("LOOM_TEST_BOOL_FLAG", off);
            assert!(!env_bool("LOOM_TEST_BOOL_FLAG", true), "{off} should resolve false");
        }
        std::env::set_var("LOOM_TEST_BOOL_FLAG", "1");
        assert!(env_bool("LOOM_TEST_BOOL_FLAG", false));
        std::env::remove_var("LOOM_TEST_BOOL_FLAG");
    }

    #[test]
    fn missing_archive_script_returns_error_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(run_archive_logs(dir.path(), true, false, None), 1);
    }
}
