//! The `# LOOM_RUNTIME_PREFERENCE` marker in the per-launch record (Issue
//! #8599), driven through the real `spawn-worker` path.
//!
//! A sibling of `worker_spawn.rs` rather than a section of it, for the same
//! reason `worker_spawn_api_keys.rs` is one: that file sits just under the
//! 1000-line file-size ratchet and these tests would carry it over. The
//! `worker` helper below is a deliberate copy of its own.
//!
//! What is under test is the seam, not the resolver: the daemon decides the
//! tier in its own process and hands the already-rendered marker to the child
//! in `LOOM_RUNTIME_PREFERENCE_MARKER` (see `launch_env`); the child writes
//! exactly that line, or — with no preference resolution — writes nothing at
//! all, which is what keeps #8554's "absent config ⇒ byte-identical"
//! invariant true of the launch record too.
use std::process::Command;

#[path = "support/worker_cli.rs"]
mod worker_cli;
use worker_cli::fixture;

const MARKER: &str = "# LOOM_RUNTIME_PREFERENCE order=claude,codex,opencode:zai-metered tier=2 \
                      tap=opencode:zai-metered skipped=claude:unavailable(claude_tokens: 0/21 \
                      spawnable) source=preference";

fn worker(root: &std::path::Path, runtime: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    c.args(["spawn-worker", "--"])
        .current_dir(root)
        .env("LOOM_WORKSPACE", root)
        .env("LOOM_RUNTIME", runtime)
        .env_remove("LOOM_ROLE")
        .env_remove("LOOM_MODEL")
        .env_remove("LOOM_MODEL_PROFILE")
        .env_remove("LOOM_RUNTIME_PREFERENCE_MARKER")
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env("LOOM_SHARED_API_KEYS_DIR", "")
        .env(
            "LOOM_NATIVE_GUARD_DIR",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../defaults/hooks"),
        )
        .env("LOOM_NATIVE_TOOLS_DIR", fixture().parent().unwrap().join("state"))
        .env_remove("LOOM_NATIVE_AUTH_FILE")
        .env("LOOM_PI_BIN", fixture())
        .env("LOOM_OPENCODE_BIN", fixture())
        // The OpenCode adapter probes `--version` before exec (#8438).
        .env("FIXTURE_VERSION", "1.18.31")
        .env_remove("FIXTURE_VERSION_EXIT");
    c
}

/// Run one launch and return the per-launch record it wrote.
fn launch_log(marker: Option<&str>) -> String {
    let d = tempfile::tempdir().unwrap();
    let log = d.path().join("worker.log");
    let mut cmd = worker(d.path(), "opencode");
    if let Some(marker) = marker {
        cmd.env("LOOM_RUNTIME_PREFERENCE_MARKER", marker);
    }
    let out = cmd
        .args(["--log", log.to_str().unwrap(), "-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    std::fs::read_to_string(log).unwrap()
}

/// The marker is written into the launch record verbatim, as a SIBLING line of
/// `# LOOM_RUNTIME_RESOLVED` — never as extra fields on it, which
/// `crash_signals::resolved_runtime_after` would read as part of the runtime
/// name.
#[test]
fn a_preference_resolved_launch_records_the_chosen_tier_beside_the_resolved_runtime() {
    let text = launch_log(Some(MARKER));
    assert!(text.contains(MARKER), "{text}");
    let resolved = text
        .lines()
        .find(|l| l.starts_with("# LOOM_RUNTIME_RESOLVED"))
        .unwrap_or_else(|| panic!("no resolved line: {text}"));
    assert_eq!(resolved, "# LOOM_RUNTIME_RESOLVED runtime=opencode", "{text}");
    assert!(
        text.lines().any(|l| l == MARKER),
        "the marker must be its own whole line: {text}"
    );
}

/// With no preference resolution — every host that has not configured
/// `runtimes.preference`, plus every operator-pinned launch — the launch
/// record carries no preference line at all. Absent, not an empty marker.
#[test]
fn an_unconfigured_launch_records_no_preference_line() {
    let text = launch_log(None);
    assert!(text.contains("# LOOM_RUNTIME_RESOLVED runtime=opencode"), "{text}");
    assert!(!text.contains("LOOM_RUNTIME_PREFERENCE"), "{text}");
}
