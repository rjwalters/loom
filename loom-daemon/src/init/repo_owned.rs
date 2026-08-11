//! Ownership boundary for the managed-directory clean sweep (issue #5971).
//!
//! On reinstall, `sync_managed_dir` cleans each managed `.loom/` directory
//! before re-copying from `defaults/`. That sweep used to delete **every**
//! destination-only file it found, with no ownership check — so a repo-owned
//! file living inside a Loom-managed directory was silently destroyed. The
//! reported incident: a consumer repo's own `.loom/hooks/post-worktree.sh`
//! (a documented extension point Loom itself invokes from `worktree.sh`)
//! disappeared on an `install.sh --quick --yes --confirm-reinstall` upgrade,
//! so the hook simply stopped firing.
//!
//! This module answers one question for a single destination path: **is Loom
//! entitled to delete it?** Three signals, checked in this order by
//! [`OwnershipBoundary::classify`]:
//!
//! 1. **Loom ships it right now** (the caller passes `shipped_now`, derived
//!    from the corresponding `defaults/` source tree). Removable — the sweep
//!    deletes it and the copy step immediately re-writes it, so the net
//!    effect on disk is unchanged from before this module existed.
//! 2. **The repo declared it repo-owned** by listing its `.loom/`-relative
//!    path in `.loom/resync-ignore`. Never removable. This reuses the
//!    existing, already-documented pin convention that
//!    `resync-installed.sh` honors ("never overwrite this file"), extended
//!    to also mean "never delete this file".
//! 3. **A previous install recorded it** in `.loom/install-metadata.json`'s
//!    `installed_files`. Removable — Loom wrote it, so Loom may retire it.
//!
//! Anything else has **no ownership evidence at all** and is preserved
//! (and reported), because the failure modes are not symmetric: a
//! wrongly-kept stale Loom file is cosmetic drift that the manifest-driven
//! sweeps in `scripts/install-loom.sh` / `scripts/uninstall-loom.sh` still
//! clean up, whereas a wrongly-deleted repo file is unrecoverable data loss.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// What the clean sweep is allowed to do with one destination file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ownership {
    /// Loom owns this path — the sweep may delete it.
    Loom,
    /// The repo declared this path repo-owned in `.loom/resync-ignore`.
    DeclaredRepoOwned,
    /// No ownership evidence either way — preserve conservatively.
    Unknown,
}

/// Ownership evidence gathered from a workspace, built once per init run.
#[derive(Debug, Default)]
pub struct OwnershipBoundary {
    /// `.loom/`-relative paths pinned in `.loom/resync-ignore`
    /// (e.g. `hooks/post-worktree.sh`).
    pinned: HashSet<String>,
    /// Repo-relative paths from `.loom/install-metadata.json`'s
    /// `installed_files` (e.g. `.loom/scripts/worktree.sh`).
    installed: HashSet<String>,
}

impl OwnershipBoundary {
    /// Read `.loom/resync-ignore` and `.loom/install-metadata.json` from
    /// `workspace`. Missing or unparseable files simply contribute no
    /// evidence — this never fails, and never blocks an install.
    pub fn load(workspace: &Path) -> Self {
        let loom = workspace.join(".loom");
        Self {
            pinned: parse_resync_ignore(&loom.join("resync-ignore")),
            installed: parse_installed_files(&loom.join("install-metadata.json")),
        }
    }

    /// Classify one repo-relative destination path (e.g.
    /// `.loom/hooks/post-worktree.sh`).
    ///
    /// `shipped_now` is `true` when the current `defaults/` tree ships a file
    /// at the corresponding source path; such a file is Loom's regardless of
    /// any pin, because the copy step re-writes it moments later either way
    /// (a pin on a shipped path is a *resync* concern, handled by
    /// `resync-installed.sh`, not a deletion concern).
    pub fn classify(&self, rel_path: &str, shipped_now: bool) -> Ownership {
        if shipped_now {
            return Ownership::Loom;
        }
        if self.is_declared_repo_owned(rel_path) {
            return Ownership::DeclaredRepoOwned;
        }
        if self.installed.contains(rel_path) {
            return Ownership::Loom;
        }
        Ownership::Unknown
    }

    /// True when `rel_path` (repo-relative, e.g. `.loom/hooks/foo.sh`) is
    /// pinned in `.loom/resync-ignore`.
    pub fn is_declared_repo_owned(&self, rel_path: &str) -> bool {
        let key = rel_path.strip_prefix(".loom/").unwrap_or(rel_path);
        self.pinned.contains(key)
    }
}

/// Parse `.loom/resync-ignore`: one `.loom/`-relative path per line, `#`
/// comments and blank lines ignored. Mirrors `is_ignored()` in
/// `defaults/scripts/resync-installed.sh` (exact match, no globbing) so the
/// two readers can never disagree about what a line means. A `.loom/` prefix
/// is tolerated and stripped, since that is the form operators see in
/// installer output.
fn parse_resync_ignore(path: &Path) -> HashSet<String> {
    let Ok(contents) = fs::read_to_string(path) else {
        return HashSet::new();
    };
    contents
        .lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.strip_prefix("./")
                .unwrap_or(line)
                .strip_prefix(".loom/")
                .unwrap_or(line)
                .to_string()
        })
        .collect()
}

/// Parse `installed_files` out of `.loom/install-metadata.json`.
///
/// An absent file, unreadable JSON, or an **empty** array all yield an empty
/// set — "no record", not "Loom owns nothing". `write_install_metadata`
/// writes a stub with an empty `installed_files` (the shell installer later
/// overwrites it with the real list), so an empty array genuinely carries no
/// information and must not be read as an ownership claim.
fn parse_installed_files(path: &Path) -> HashSet<String> {
    let Ok(contents) = fs::read_to_string(path) else {
        return HashSet::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return HashSet::new();
    };
    value
        .get("installed_files")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn workspace_with(resync_ignore: Option<&str>, metadata: Option<&str>) -> TempDir {
        let tmp = TempDir::new().unwrap();
        let loom = tmp.path().join(".loom");
        fs::create_dir_all(&loom).unwrap();
        if let Some(body) = resync_ignore {
            fs::write(loom.join("resync-ignore"), body).unwrap();
        }
        if let Some(body) = metadata {
            fs::write(loom.join("install-metadata.json"), body).unwrap();
        }
        tmp
    }

    #[test]
    fn missing_files_yield_no_evidence() {
        let tmp = TempDir::new().unwrap();
        let boundary = OwnershipBoundary::load(tmp.path());
        assert_eq!(boundary.classify(".loom/hooks/post-worktree.sh", false), Ownership::Unknown);
    }

    #[test]
    fn resync_ignore_pin_declares_repo_ownership() {
        let tmp = workspace_with(Some("# a repo-owned hook\nhooks/post-worktree.sh\n\n"), None);
        let boundary = OwnershipBoundary::load(tmp.path());
        assert!(boundary.is_declared_repo_owned(".loom/hooks/post-worktree.sh"));
        assert_eq!(
            boundary.classify(".loom/hooks/post-worktree.sh", false),
            Ownership::DeclaredRepoOwned
        );
        // A different path in the same directory is unaffected.
        assert_eq!(boundary.classify(".loom/hooks/other.sh", false), Ownership::Unknown);
    }

    #[test]
    fn resync_ignore_tolerates_a_loom_prefix() {
        let tmp = workspace_with(Some(".loom/hooks/post-worktree.sh\n"), None);
        let boundary = OwnershipBoundary::load(tmp.path());
        assert!(boundary.is_declared_repo_owned(".loom/hooks/post-worktree.sh"));
    }

    #[test]
    fn installed_files_record_makes_a_path_loom_owned() {
        let tmp =
            workspace_with(None, Some(r#"{"installed_files": [".loom/scripts/retired.sh"]}"#));
        let boundary = OwnershipBoundary::load(tmp.path());
        assert_eq!(boundary.classify(".loom/scripts/retired.sh", false), Ownership::Loom);
        assert_eq!(
            boundary.classify(".loom/scripts/never-installed.sh", false),
            Ownership::Unknown
        );
    }

    #[test]
    fn empty_installed_files_is_no_record_not_an_ownership_claim() {
        let tmp = workspace_with(None, Some(r#"{"installed_files": []}"#));
        let boundary = OwnershipBoundary::load(tmp.path());
        assert_eq!(boundary.classify(".loom/scripts/anything.sh", false), Ownership::Unknown);
    }

    #[test]
    fn malformed_metadata_is_treated_as_no_record() {
        let tmp = workspace_with(None, Some("{not json"));
        let boundary = OwnershipBoundary::load(tmp.path());
        assert_eq!(boundary.classify(".loom/scripts/anything.sh", false), Ownership::Unknown);
    }

    #[test]
    fn a_currently_shipped_path_is_always_loom_owned() {
        // Even a pinned path: the copy step rewrites it moments later, so the
        // pin cannot protect it from the clean-then-copy cycle and pretending
        // otherwise would only make the report lie.
        let tmp = workspace_with(Some("hooks/guard-destructive.sh\n"), None);
        let boundary = OwnershipBoundary::load(tmp.path());
        assert_eq!(boundary.classify(".loom/hooks/guard-destructive.sh", true), Ownership::Loom);
    }
}
