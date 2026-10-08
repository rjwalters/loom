//! The std-only half of `build.rs`: the git stamp and its `rerun-if-changed`
//! watch set (#4053). Kept in its own file, with no crate dependencies, so
//! `defaults/scripts/tests/test-build-rs-watchset.sh` can compile it in a
//! hermetic scratch crate (#10717 added `tar`/`zstd` to `build.rs`).
//!
//! See `build.rs` for the watch-set rationale.

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

/// `clean` / `dirty` over TRACKED files only (untracked files are ignored,
/// as D33 specifies), or `unknown` when git cannot answer.
fn tree_state() -> &'static str {
    let Ok(out) = Command::new("git")
        // `--no-optional-locks`: never take `index.lock` to write back
        // refreshed stat data, which would race an agent's concurrent
        // `git commit` and touch the `index` this script watches.
        .args([
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--untracked-files=no",
        ])
        .output()
    else {
        return "unknown";
    };
    if !out.status.success() {
        "unknown"
    } else if out.stdout.iter().all(u8::is_ascii_whitespace) {
        "clean"
    } else {
        "dirty"
    }
}
pub fn emit() {
    // Re-run when HEAD moves (branch advance, checkout, commit) or the index
    // changes. Resolved correct-by-construction via `git rev-parse --git-path`
    // so this works in a linked worktree and a packed-refs repo too (#4053).
    emit_git_rerun_paths();

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

    // Provenance build identity (#9027, harness-ops D33): the FULL 40-hex
    // commit and whether tracked files differed from it. `unknown` when git
    // cannot answer — never omitted, never guessed. The short commit above is
    // unchanged: self_update compares it and `--version` displays it.
    let full = git_output(&["rev-parse", "HEAD"])
        .filter(|s| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = if full == "unknown" {
        "unknown"
    } else {
        tree_state()
    };
    println!("cargo:rustc-env=LOOM_DAEMON_GIT_COMMIT_FULL={full}");
    println!("cargo:rustc-env=LOOM_DAEMON_GIT_DIRTY={dirty}");
    // The dirty flag goes stale if an edit does not re-run this script: watch
    // the crate's own inputs (cargo scans a directory recursively), so any
    // edit that changes the binary also re-derives the flag. HEAD movement
    // (commit, checkout, reset) is already watched above.
    for input in ["src", "Cargo.toml", "../Cargo.lock", "../defaults"] {
        if Path::new(input).exists() {
            println!("cargo:rerun-if-changed={input}");
        }
    }

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
