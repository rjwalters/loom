//! Resolving the **installed** `loom-daemon` CLI the probe should invoke.
//!
//! # Why not [`crate::daemon_bin_resolve::resolve_daemon_bin`]
//!
//! That resolver starts from `current_exe()` — it answers "where am I?". This
//! one answers "where is the daemon CLI an operator would run?", which is a
//! different question and, for this caller, the only correct one. The watchdog
//! may itself be the running binary while the *installed* CLI is missing,
//! stale, or a stub; #4381 is exactly that case, where the installed binary had
//! been replaced by something that answered `--version` and then hung forever.
//!
//! Using `current_exe()` here would also silently break the graceful-degrade
//! path: the probe could never fail to resolve a binary, because it would
//! always find itself, so "no resolvable loom-daemon ⇒ skip the probe, keep
//! doing the other two checks" would become unreachable.
//!
//! Mirrors `lib/locate-daemon-bin.sh::loom_locate_daemon_bin`'s precedence,
//! which 21 scripts share. `LOOM_PREFER_REPO_BUILD=1` is honoured because the
//! test suites depend on it to pin a freshly built binary.

use std::path::{Path, PathBuf};

fn executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

fn on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(name))
            .find(|c| executable(c))
    })
}

/// In-repo build candidates, in the shell's order.
fn repo_candidates(root: Option<&Path>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(dir) = std::env::var_os("CARGO_TARGET_DIR") {
        let d = PathBuf::from(dir);
        out.push(d.join("release/loom-daemon"));
        out.push(d.join("debug/loom-daemon"));
    }
    if let Some(root) = root {
        out.push(root.join("loom-daemon/target/release/loom-daemon"));
        out.push(root.join("loom-daemon/target/debug/loom-daemon"));
        out.push(root.join("target/release/loom-daemon"));
        out.push(root.join("target/debug/loom-daemon"));
    }
    out
}

/// Resolve, or `None` when nothing is usable.
///
/// `None` is a normal outcome, not an error: the caller must SKIP the probe and
/// keep doing the other two checks. A watchdog that pages because its own
/// optional helper is missing is worse than one that stays quiet about it.
#[must_use]
pub fn daemon_bin(repo_root: Option<&Path>) -> Option<PathBuf> {
    if let Some(explicit) = super::env::var("LOOM_DAEMON_BIN").map(PathBuf::from) {
        if executable(&explicit) {
            return Some(explicit);
        }
    }
    if super::env::var("LOOM_PREFER_REPO_BUILD").as_deref() == Some("1") {
        if let Some(c) = repo_candidates(repo_root)
            .into_iter()
            .find(|c| executable(c))
        {
            return Some(c);
        }
    }
    if let Some(p) = on_path("loom-daemon") {
        return Some(p);
    }
    let machine_dir = super::env::var("LOOM_DAEMON_BIN_DIR")
        .map_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/bin"), PathBuf::from);
    let machine_bin = machine_dir.join("loom-daemon");
    if executable(&machine_bin) {
        return Some(machine_bin);
    }
    repo_candidates(repo_root)
        .into_iter()
        .find(|c| executable(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_executable_loom_daemon_bin_is_not_accepted() {
        // The shell guarded with `-x`, not `-n`: naming a path that is not
        // executable must fall through to the next tier, not resolve to it.
        let dir = tempfile::tempdir().expect("tempdir");
        let f = dir.path().join("loom-daemon");
        std::fs::write(&f, b"not executable").expect("write");
        assert!(!executable(&f));
    }

    #[test]
    fn repo_candidates_follow_the_shells_order() {
        let root = PathBuf::from("/repo");
        let c = repo_candidates(Some(&root));
        let names: Vec<String> = c.iter().map(|p| p.display().to_string()).collect();
        let idx = |s: &str| names.iter().position(|n| n == s).expect(s);
        assert!(
            idx("/repo/loom-daemon/target/release/loom-daemon")
                < idx("/repo/loom-daemon/target/debug/loom-daemon"),
            "release is preferred over debug"
        );
        assert!(
            idx("/repo/loom-daemon/target/debug/loom-daemon")
                < idx("/repo/target/release/loom-daemon"),
            "the crate-local target dir is searched before the workspace one"
        );
    }

    #[test]
    fn no_repo_root_still_yields_the_cargo_target_dir_tier_only() {
        // With no root and no CARGO_TARGET_DIR there is nothing to try, and the
        // caller must degrade rather than guess at a path.
        if std::env::var_os("CARGO_TARGET_DIR").is_none() {
            assert!(repo_candidates(None).is_empty());
        }
    }
}
