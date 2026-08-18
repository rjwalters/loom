//! Resolve the running `loom-daemon` binary for self-spawning helper
//! subprocesses, surviving the binary being replaced out from under the
//! running process (issue #6471).
//!
//! # Why
//!
//! [`crate::token_ranking_refresh::ScriptRankingRefreshRunner::resolve_bin`]
//! and [`crate::install_self_check::repair_token_ranking`] both shell out to
//! **the currently-running daemon's own binary** via `std::env::current_exe()`
//! (`tokens check --ranking`, a read-only self-probe — see those modules'
//! docs for why `current_exe()` rather than a resolved script). On Linux,
//! `current_exe()` reads `/proc/self/exe`; once `auto_update` overwrites the
//! installed binary (unlinking the inode the running process was started
//! from), that symlink target grows a literal ` (deleted)` suffix and no
//! longer names a file that exists. Every self-spawn then fails with `ENOENT`
//! for the whole window between the binary being replaced and the (possibly
//! deferred, up to `deferDeadline`) restart landing — silently degrading the
//! very fail-safe (refuse-and-keep-running) that a deferred roll relies on.
//!
//! # Strategy
//!
//! 1. Resolve `std::env::current_exe()`.
//! 2. If its file name does not carry the kernel's ` (deleted)` marker,
//!    return it unchanged (the common case).
//! 3. Strip the marker. If the resulting path still exists as a file — the
//!    normal case for this repo's install strategy, which overwrites the
//!    binary in place rather than repointing a versioned symlink (see
//!    `defaults/scripts/cli/loom-daemon-update.sh`) — use it. This
//!    deliberately spawns the **new** binary from the still-running **old**
//!    daemon process; that's fine for these two call sites, both of which are
//!    read-only self-probes with no dependency on daemon-process/subprocess
//!    version parity.
//! 4. Otherwise (the file at the stripped path is also gone — a `rm`-then-
//!    rebuild install strategy, or a genuinely relocated binary), fall back
//!    to a `$PATH` lookup for `loom-daemon`.
//! 5. If none of the above resolves to an existing file, return an error
//!    naming exactly what was tried. Both callers treat this the same as any
//!    other probe failure: logged and skipped, never fatal.

use std::path::{Path, PathBuf};

/// Suffix the Linux kernel appends to a `/proc/self/exe` readlink target when
/// the underlying inode has been unlinked (e.g. `auto_update` overwriting the
/// installed binary while the old process is still running).
const DELETED_SUFFIX: &str = " (deleted)";

/// Resolve the current daemon binary to spawn helper subprocesses from,
/// surviving a deleted-inode `current_exe()` — see module docs.
pub fn resolve_daemon_bin() -> Result<PathBuf, String> {
    let exe =
        std::env::current_exe().map_err(|e| format!("could not resolve current_exe(): {e}"))?;
    resolve_from_current_exe(&exe, which_on_path)
}

/// Testable core: takes the raw `current_exe()` result and a PATH-lookup
/// function (injected so tests never depend on the real `$PATH` / a real
/// `loom-daemon` binary being installed).
fn resolve_from_current_exe(
    exe: &Path,
    path_lookup: impl FnOnce(&str) -> Option<PathBuf>,
) -> Result<PathBuf, String> {
    let Some(stripped) = strip_deleted_suffix(exe) else {
        return Ok(exe.to_path_buf());
    };
    if stripped.is_file() {
        return Ok(stripped);
    }
    if let Some(found) = path_lookup("loom-daemon") {
        return Ok(found);
    }
    Err(format!(
        "current_exe() resolved to a deleted inode ({}) — the binary was likely replaced by \
         auto_update, but neither the replacement at {} nor a `loom-daemon` on $PATH could be \
         found",
        exe.display(),
        stripped.display()
    ))
}

/// If `path`'s final component ends with the kernel's ` (deleted)` marker,
/// return the path with that suffix stripped (the original, now-unlinked
/// filesystem path). `None` for an ordinary (non-deleted) path.
fn strip_deleted_suffix(path: &Path) -> Option<PathBuf> {
    let file_name = path.file_name()?.to_str()?;
    let stripped_name = file_name.strip_suffix(DELETED_SUFFIX)?;
    Some(path.with_file_name(stripped_name))
}

/// Locate `name` on `$PATH`, mirroring a shell's lookup (no shell spawned) —
/// the last-resort fallback when neither `current_exe()` nor its de-suffixed
/// sibling resolves to an existing file.
fn which_on_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var).find_map(|dir| {
        let candidate = dir.join(name);
        candidate.is_file().then_some(candidate)
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;

    fn touch_executable(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).unwrap();
        }
    }

    #[test]
    fn test_ordinary_path_is_returned_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("loom-daemon");
        touch_executable(&exe);
        let resolved = resolve_from_current_exe(&exe, |_| None).unwrap();
        assert_eq!(resolved, exe);
    }

    #[test]
    fn test_deleted_suffix_stripped_when_replacement_exists() {
        // Simulates the exact production scenario: auto_update overwrote the
        // binary in place, so the stripped path IS a live, existing file even
        // though current_exe() still reports the pre-replacement (deleted)
        // inode's path.
        let tmp = tempfile::tempdir().unwrap();
        let real_bin = tmp.path().join("loom-daemon");
        touch_executable(&real_bin);
        let deleted_path = tmp.path().join("loom-daemon (deleted)");

        let resolved = resolve_from_current_exe(&deleted_path, |_| None).unwrap();
        assert_eq!(resolved, real_bin, "must resolve to the replacement binary, not the fallback");
    }

    #[test]
    fn test_falls_back_to_path_lookup_when_stripped_path_is_also_gone() {
        let tmp = tempfile::tempdir().unwrap();
        // Nothing exists at the stripped path (tmp.path()/loom-daemon) —
        // simulates an install strategy that removes-then-rebuilds rather
        // than overwriting in place.
        let deleted_path = tmp.path().join("loom-daemon (deleted)");

        let path_bin = tmp.path().join("on-path").join("loom-daemon");
        touch_executable(&path_bin);

        let resolved = resolve_from_current_exe(&deleted_path, |name| {
            assert_eq!(name, "loom-daemon");
            Some(path_bin.clone())
        })
        .unwrap();
        assert_eq!(resolved, path_bin);
    }

    #[test]
    fn test_errors_clearly_when_nothing_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let deleted_path = tmp.path().join("loom-daemon (deleted)");

        let err = resolve_from_current_exe(&deleted_path, |_| None).unwrap_err();
        assert!(err.contains("deleted inode"), "{err}");
        assert!(err.contains("loom-daemon"), "{err}");
    }

    #[test]
    fn test_which_on_path_finds_an_executable_on_a_synthetic_path() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("loom-daemon");
        touch_executable(&bin);

        let joined = std::env::join_paths([tmp.path()]).unwrap();
        let found = std::env::split_paths(&joined).find_map(|dir| {
            let candidate = dir.join("loom-daemon");
            candidate.is_file().then_some(candidate)
        });
        assert_eq!(found, Some(bin));
    }

    #[test]
    fn test_strip_deleted_suffix() {
        assert_eq!(
            strip_deleted_suffix(Path::new("/a/b/loom-daemon (deleted)")),
            Some(PathBuf::from("/a/b/loom-daemon"))
        );
        assert_eq!(strip_deleted_suffix(Path::new("/a/b/loom-daemon")), None);
    }
}
