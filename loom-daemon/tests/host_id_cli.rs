//! `loom-daemon host-id` (Issue #11530).
//!
//! Pins the cross-process half of the host-identity contract the in-crate
//! unit tests cannot: the binary prints exactly the resolved id for each
//! tier (config, env, persisted), two processes agree on one persisted id,
//! `$HOSTNAME` never leaks into it, and `unknown-host` is a non-zero exit.
//!
//! Every child gets `LOOM_CONFIG_DEFAULTS_FILE` and `LOOM_HOST_ID_FILE`
//! pinned to fixtures, so no test can read the operator's machine tier or
//! create the real `~/.loom/host-id`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_loom-daemon")
}

/// Run `loom-daemon host-id <args>` with every identity input pinned.
fn host_id(
    config: Option<&Path>,
    env_id: Option<&str>,
    id_file: &Path,
    hostname: &str,
    args: &[&str],
) -> Output {
    let mut cmd = Command::new(bin());
    cmd.arg("host-id")
        .args(args)
        .env(
            "LOOM_CONFIG_DEFAULTS_FILE",
            config.map_or(std::ffi::OsStr::new(""), Path::as_os_str),
        )
        .env("LOOM_HOST_ID_FILE", id_file)
        .env("HOSTNAME", hostname)
        .env_remove("LOOM_HOST_ID");
    if let Some(v) = env_id {
        cmd.env("LOOM_HOST_ID", v);
    }
    cmd.output().unwrap()
}

fn stdout(out: &Output) -> String {
    assert!(out.status.success(), "host-id failed: {out:?}");
    String::from_utf8(out.stdout.clone()).unwrap()
}

#[test]
fn config_tier_wins_and_prints_exactly_one_line() {
    let dir = tempfile::tempdir().unwrap();
    let tier = dir.path().join("defaults.json");
    std::fs::write(&tier, r#"{"fleet":{"hostId":"loom-worker-1"}}"#).unwrap();
    let id_file = dir.path().join("host-id");

    let out = host_id(Some(&tier), Some("other"), &id_file, "h", &[]);
    assert_eq!(stdout(&out), "loom-worker-1\n");
    assert!(!id_file.exists(), "a configured id must not create the persisted file");

    let out = host_id(Some(&tier), Some("other"), &id_file, "h", &["--source"]);
    assert_eq!(stdout(&out), "loom-worker-1\n", "--source must not touch stdout");
    assert!(String::from_utf8_lossy(&out.stderr).contains("source: config"), "{out:?}");
}

#[test]
fn env_tier_wins_without_config_and_blank_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let id_file = dir.path().join("host-id");
    std::fs::write(&id_file, "persisted-one\n").unwrap();

    let out = host_id(None, Some("loom-worker-9"), &id_file, "h", &[]);
    assert_eq!(stdout(&out), "loom-worker-9\n");

    let out = host_id(None, Some("  "), &id_file, "h", &[]);
    assert_eq!(stdout(&out), "persisted-one\n");
}

#[test]
fn persisted_tier_is_generated_once_and_never_the_hostname() {
    let dir = tempfile::tempdir().unwrap();
    let id_file = dir.path().join("dot-loom").join("host-id");

    let first = host_id(None, None, &id_file, "shell-exported", &["--source"]);
    let id = stdout(&first);
    assert!(id.ends_with('\n') && id.lines().count() == 1, "{id:?}");
    let id = id.trim_end().to_string();
    assert!(id.starts_with("loom-host-"), "{id}");
    assert_ne!(id, "shell-exported");
    assert!(
        String::from_utf8_lossy(&first.stderr).contains("source: generated"),
        "{first:?}"
    );

    // A second process, from a "different launch context" ($HOSTNAME changed).
    let second = host_id(None, None, &id_file, "launchd-context", &["--source"]);
    assert_eq!(stdout(&second), format!("{id}\n"));
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("source: persisted"),
        "{second:?}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&id_file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "owner-only, got {mode:o}");
    }
}

#[test]
fn unknown_host_exits_non_zero_with_empty_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("a-file");
    std::fs::write(&blocker, "").unwrap();
    let id_file = blocker.join("host-id"); // parent is a file: cannot be created

    let out = host_id(None, None, &id_file, "studio", &[]);
    assert!(!out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "stdout must carry no id: {out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("LOOM_HOST_ID") && stderr.contains("fleet.hostId"), "{stderr}");
}

// Fail this test binary on any spawn of the real `gh` (#10225, extends #10138).
#[allow(dead_code)]
#[path = "../src/live_gh_guard.rs"]
mod live_gh_guard;
