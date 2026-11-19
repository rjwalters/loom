//! Proves the live-`gh` guard (#10138) is active in an integration test binary
//! (#10225): a bare `gh` resolves to the guard's stand-in, which exits 97 and
//! names the argv, and a child `loom-daemon` inherits the guarded `PATH`.

use std::path::PathBuf;
use std::process::Command;

/// Where a bare `gh` resolves on `PATH`, as `execvp` looks it up.
fn resolve_bare_gh() -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("gh"))
        .find(|p| {
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

#[test]
fn bare_gh_resolves_to_the_guard_in_an_integration_binary() {
    let stand_in = resolve_bare_gh().expect("a gh on PATH");
    assert!(
        stand_in
            .parent()
            .and_then(|d| d.file_name())
            .is_some_and(|n| n.to_string_lossy().starts_with("loom-live-gh-guard-")),
        "bare gh must resolve to the guard stand-in, got {}",
        stand_in.display()
    );

    // Private log, so this deliberate spawn does not trip the exit-time check.
    let scratch = tempfile::tempdir().unwrap();
    let log = scratch.path().join("calls.log");
    let out = Command::new(&stand_in)
        .arg("--version")
        .env("LOOM_LIVE_GH_LOG", &log)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(97));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.starts_with("live gh in test: --version"), "{stderr}");
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "--version\n");
}

#[test]
fn child_process_inherits_the_guarded_path() {
    let scratch = tempfile::tempdir().unwrap();
    let log = scratch.path().join("calls.log");
    let out = Command::new("sh")
        .args(["-c", "gh --version"])
        .env("LOOM_LIVE_GH_LOG", &log)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(97));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("live gh in test: --version"), "{stderr}");
}

// Fail this test binary on any spawn of the real `gh` (#10225, extends #10138).
#[allow(dead_code)]
#[path = "../src/live_gh_guard.rs"]
mod live_gh_guard;
