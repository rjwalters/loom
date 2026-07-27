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
//!
//! ## Watch-set correctness (issue #4053)
//!
//! The baked commit must be re-derived whenever `HEAD` moves, or `--version`
//! silently ships a stale commit and any self-update loop that trusts it
//! (`self_update::check()`, `loom-daemon-update.sh`) becomes an infinite
//! detect-stale → rebuild → still-stale retry. The naive
//! `cargo:rerun-if-changed` set — `../.git/HEAD` + `../.git/index` — does NOT
//! track HEAD movement: `.git/HEAD` is a *symbolic* ref whose content
//! (`ref: refs/heads/main`) never changes when the branch advances, and the
//! file that actually moves (`.git/refs/heads/<branch>`, or `.git/packed-refs`
//! after a `git gc`) was not watched at all. Worse, in a **linked worktree**
//! (`.git` is a *file*, not a directory — the environment every Loom Builder
//! compiles in) neither `../.git/HEAD` nor `../.git/index` even resolves, so
//! the whole watch set evaporated.
//!
//! The fix resolves the on-disk files git actually touches via
//! `git rev-parse --git-path` (which transparently follows the linked-worktree
//! gitdir indirection and the packed-refs relocation) plus the symbolic-ref
//! target for the current branch — correct by construction in the main
//! checkout, a linked worktree, a detached HEAD, and a packed-refs repo alike.
//! Only paths that actually exist are emitted: cargo re-runs a build script
//! unconditionally for a `rerun-if-changed` path that does not exist, so
//! emitting a non-existent path would trade a stale-commit bug for an
//! every-build recompile.

use std::path::Path;
use std::process::Command;

/// Run `git <args>` and return trimmed stdout on success, else `None`.
fn git_output(args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
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
}

/// Resolve a git metadata file (e.g. `HEAD`, `packed-refs`, `refs/heads/main`)
/// to its real on-disk path via `git rev-parse --git-path`. This handles the
/// linked-worktree indirection (where the per-worktree gitdir lives under the
/// common `.git/worktrees/<name>/`) and the packed-refs relocation for free.
fn git_path(spec: &str) -> Option<String> {
    git_output(&["rev-parse", "--git-path", spec])
}

/// Emit the `cargo:rerun-if-changed` watch set that correctly tracks HEAD
/// movement. Best-effort: when `git` is absent or fails (release tarball),
/// nothing is emitted and the build falls through to the `unknown` commit
/// fallback below. See the module doc for the full rationale.
fn emit_git_rerun_paths() {
    let mut specs: Vec<String> = Vec::new();

    // The always-present metadata files. `HEAD` catches branch switches and a
    // detached-HEAD move; `index` mirrors the historical watch entry; the
    // symbolic-ref target and `packed-refs` are the files that actually move
    // when the checked-out branch advances (loose ref pre-gc, packed-refs
    // post-gc — watch BOTH, since a `git gc` migrates the ref between them).
    for spec in ["HEAD", "index", "packed-refs"] {
        specs.push(spec.to_string());
    }

    // The resolved ref for the current branch (e.g. `refs/heads/main`). A
    // detached HEAD has no symbolic ref — skip it there (HEAD itself moves in
    // that case, and HEAD is already watched above).
    if let Some(symref) = git_output(&["symbolic-ref", "-q", "HEAD"]) {
        specs.push(symref);
    }

    for spec in specs {
        if let Some(path) = git_path(&spec) {
            // Only watch paths that exist. cargo re-runs the build script on
            // EVERY build for a `rerun-if-changed` path that does not exist,
            // which would defeat incremental compilation. A ref that later
            // appears (or migrates loose<->packed) is picked up on the next
            // build-script re-run, which the still-existing sibling paths
            // (HEAD/index/packed-refs) reliably trigger.
            if Path::new(&path).exists() {
                println!("cargo:rerun-if-changed={path}");
            }
        }
    }
}

fn main() {
    // Re-run when HEAD moves (branch advance, checkout, commit) or the index
    // changes. Resolved correct-by-construction via `git rev-parse --git-path`
    // so this works in a linked worktree and a packed-refs repo too (#4053).
    emit_git_rerun_paths();
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
