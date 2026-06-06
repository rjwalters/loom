//! Build script: capture git commit + build timestamp for `--version`.
//!
//! Motivated by issue #3470 (and the broader #3287 Option D recommendation):
//! when a consumer install fails with a "MISSING: <file>" error from the
//! post-install metadata verification, the most common proximate cause is a
//! stale `target/release/loom-daemon` binary built from a source tree that
//! predates the fix for that missing file. Today `loom-daemon --version`
//! emits only `loom-daemon 0.10.0`, which is identical across rebuilds of
//! the same crate version — operators cannot tell at a glance whether the
//! binary on disk matches `HEAD`.
//!
//! This script embeds the short HEAD hash and an ISO-8601 build timestamp
//! into the binary via `cargo:rustc-env`. They are surfaced in `main.rs`
//! via `env!("LOOM_DAEMON_GIT_COMMIT")` / `env!("LOOM_DAEMON_BUILD_TIME")`
//! and folded into the clap `--version` long string.
//!
//! Both fall back to a placeholder when the build host lacks `git` (e.g.,
//! building from a tarball release) or `date` — we never want missing
//! tooling to break the build. The fallback is loud enough to be obvious
//! in operator output (e.g., `loom-daemon 0.10.0 (commit unknown)`).

use std::process::Command;

fn main() {
    // Re-run if `git` HEAD moves or the index changes. This is best-effort:
    // if `.git` is absent (release tarball), we skip and use the fallback.
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/index");
    // Always re-run when build.rs itself changes.
    println!("cargo:rerun-if-changed=build.rs");

    let commit = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                String::from_utf8(out.stdout)
                    .ok()
                    .map(|s| s.trim().to_string())
            } else {
                None
            }
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=LOOM_DAEMON_GIT_COMMIT={commit}");

    // Build timestamp in ISO-8601 UTC. We use `date -u +%FT%TZ` for
    // portability across macOS and Linux without pulling chrono into the
    // build-script dependency graph (build scripts compile separately and
    // the extra dep noticeably slows clean builds).
    let timestamp = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                String::from_utf8(out.stdout)
                    .ok()
                    .map(|s| s.trim().to_string())
            } else {
                None
            }
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=LOOM_DAEMON_BUILD_TIME={timestamp}");
}
