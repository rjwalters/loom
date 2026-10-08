//! Build script: capture git commit + build timestamp for `--version`.
//! It also packs the install payload the binary embeds (#10717; see
//! `pack_install_payload` at the bottom).
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

mod build_stamp;

use std::path::Path;
use std::process::Command;

fn main() {
    build_stamp::emit();
    // Always re-run when build.rs itself or the stamp half changes.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build_stamp.rs");
    // `../defaults` is on the watch set (emitted by `build_stamp::emit`), so an
    // edit to the payload re-runs this and re-packs it.
    pack_install_payload();
}

/// Pack the installable file set into `$OUT_DIR/install-payload.tar.zst`
/// (#10717). `init/payload.rs` embeds it with `include_bytes!`, which is what
/// makes a resync install the files of the RUNNING release: the binary
/// carries them, so `~/GitHub/loom` (which the release-download update path
/// deliberately leaves at whatever version it was) is never read.
///
/// The set is `defaults/` as git tracks it (`git ls-files`), so untracked and
/// ignored scratch in a developer's tree never ships. A build with no git (a
/// source tarball) walks the directory instead. Entries are sorted and carry
/// no owner or mtime, so the archive is a function of the file contents and
/// their executable bits alone. Symlinks stay symlinks: `defaults/roles/*.md`
/// point into `defaults/.claude/commands/loom/`, and the installer copies
/// through them exactly as it does from a source checkout.
fn pack_install_payload() {
    let out_dir = std::env::var("OUT_DIR").unwrap_or_else(|_| ".".to_string());
    let out = Path::new(&out_dir).join("install-payload.tar.zst");
    let root = Path::new("..");

    let mut files = tracked_payload_files(root).unwrap_or_else(|| {
        let mut walked = Vec::new();
        walk_payload_dir(root, Path::new("defaults"), &mut walked);
        walked
    });
    files.sort();
    files.dedup();

    let mut builder = tar::Builder::new(Vec::new());
    builder.follow_symlinks(false);
    for rel in &files {
        let path = root.join(rel);
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue; // tracked but deleted in the working tree
        };
        let mut header = tar::Header::new_gnu();
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        if meta.file_type().is_symlink() {
            let Ok(target) = std::fs::read_link(&path) else {
                continue;
            };
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_mode(0o777);
            header.set_size(0);
            builder
                .append_link(&mut header, rel, &target)
                .unwrap_or_else(|e| panic!("pack install payload: {rel}: {e}"));
        } else if meta.is_file() {
            let bytes = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("pack install payload: read {rel}: {e}"));
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(if is_executable(&meta) { 0o755 } else { 0o644 });
            header.set_size(bytes.len() as u64);
            builder
                .append_data(&mut header, rel, bytes.as_slice())
                .unwrap_or_else(|e| panic!("pack install payload: {rel}: {e}"));
        }
    }
    let tar_bytes = builder
        .into_inner()
        .unwrap_or_else(|e| panic!("pack install payload: finish tar: {e}"));
    // Level 10: ~4.5 MB for today's ~19 MB tree, in well under a second.
    // Level 19 saves another ~0.5 MB for twenty times the build time.
    let compressed = zstd::encode_all(tar_bytes.as_slice(), 10)
        .unwrap_or_else(|e| panic!("pack install payload: compress: {e}"));
    std::fs::write(&out, compressed)
        .unwrap_or_else(|e| panic!("pack install payload: write {}: {e}", out.display()));
}

/// `defaults/` files as git tracks them, relative to the repo root. `None`
/// when git cannot answer (no git, or not a checkout).
fn tracked_payload_files(root: &Path) -> Option<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "--", "defaults"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let files: Vec<String> = out
        .stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| String::from_utf8(s.to_vec()).ok())
        .collect();
    (!files.is_empty()).then_some(files)
}

/// The no-git fallback: every file and symlink under `defaults/`.
fn walk_payload_dir(root: &Path, rel: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(root.join(rel)) else {
        return;
    };
    for entry in entries.flatten() {
        let child = rel.join(entry.file_name());
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            walk_payload_dir(root, &child, out);
        } else if entry.file_name() != ".DS_Store" {
            out.push(child.to_string_lossy().into_owned());
        }
    }
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
}
