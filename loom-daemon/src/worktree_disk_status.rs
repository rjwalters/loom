//! Per-repo `.loom/worktrees` census — how many worktrees a managed repo is
//! carrying, split by naming class, and how many bytes they occupy (Issue
//! #5939).
//!
//! # Why this exists
//!
//! `loom-daemon status` already reports **disk headroom** (`disk_headroom`,
//! the `min(disk, ram, configured_max)` term that bounds dispatch). What it
//! could not report is *why* that headroom is low. A host silently carrying
//! 39 GB of merged-PR worktrees rendered identically to a host that was
//! genuinely out of space, and an operator could not tell the two apart
//! without shelling out to `du` — which is exactly how #5939's 110-worktree /
//! 27 GB accumulation went unnoticed while the scheduled cleaner reported
//! `0 cleaned` every run.
//!
//! The count is also the observable that makes the `pr-<N>` reclaim gap
//! self-evident: `issue-*: 14, pr-*: 110` on one line says, without further
//! analysis, that one naming class is not being reclaimed.
//!
//! # Where it runs
//!
//! **Client-side, in the `status` CLI** — deliberately not inside the IPC
//! handler. This is a filesystem walk over potentially tens of GB, and the
//! daemon-status IPC handler is documented (see
//! [`crate::types::DaemonStatusReport`]) to stay fast, with slow probes
//! collected by the CLI instead — the same split the per-token usage table
//! (`collect_token_usage`) and the forge pipeline snapshot
//! (`pipeline_snapshot`) already use. The CLI always shares a host with the
//! daemon it queries (a Unix socket, not a network), so a host-local walk of
//! the daemon's own `per_repo` roots measures exactly the right filesystem.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::worktree_ops::naming;

/// One managed repo's worktree census, as collected by
/// [`collect_worktree_disk_summary`] and rendered by `loom-daemon status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorktreeDiskSummary {
    /// The managed workspace root this census describes (the same
    /// [`crate::types::RepoStatus::root`] the daemon reported).
    pub root: PathBuf,
    /// Directories directly under this root's worktree root — every class,
    /// including names neither the `issue-<N>` nor the `pr-<N>` filter
    /// recognizes.
    pub total_count: usize,
    /// Of [`Self::total_count`], how many match `issue-<N>`
    /// ([`naming::issue_from_worktree`]).
    pub issue_count: usize,
    /// Of [`Self::total_count`], how many match `pr-<N>`
    /// ([`naming::pr_from_worktree`]) — the class #5939 found no automatic
    /// path was reclaiming.
    pub pr_count: usize,
    /// Of [`Self::total_count`], how many match neither filter (an operator's
    /// own `git worktree add`, a scratch directory, a naming scheme added
    /// after this code). Non-zero here is the early warning that the reaper's
    /// filters have fallen behind the naming schemes actually in use — the
    /// precise failure mode #5939 reports, generalized.
    pub other_count: usize,
    /// Total apparent size, in bytes, of everything under the worktree root.
    /// `None` when the worktree root does not exist or could not be read at
    /// all — never silently `0`, so "no worktrees" and "could not measure"
    /// stay distinguishable.
    pub total_bytes: Option<u64>,
}

/// Split worktree directory names into `(issue, pr, other)` counts — pure, so
/// the classification is unit-testable without a filesystem.
#[must_use]
pub fn classify_worktree_names<S: AsRef<str>>(names: &[S]) -> (usize, usize, usize) {
    let mut issue = 0;
    let mut pr = 0;
    let mut other = 0;
    for name in names {
        let name = name.as_ref();
        if naming::issue_from_worktree(name).is_some() {
            issue += 1;
        } else if naming::pr_from_worktree(name).is_some() {
            pr += 1;
        } else {
            other += 1;
        }
    }
    (issue, pr, other)
}

/// Total apparent size, in bytes, of everything under `path`.
///
/// Symlinks are counted at their own (tiny) size and never followed — a
/// worktree containing a symlink to a large tree outside it must not be
/// charged for that tree, and following one could otherwise loop.
///
/// Best-effort: an unreadable subdirectory or file contributes nothing rather
/// than failing the whole measurement (a permissions hiccup in one worktree
/// must not blank out a repo's census). Returns `None` only when `path`
/// itself cannot be read at all.
#[must_use]
pub fn dir_size_bytes(path: &Path) -> Option<u64> {
    fn walk(path: &Path, total: &mut u64) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            // `DirEntry::file_type` does NOT follow symlinks (it is
            // `symlink_metadata`-backed), so a symlinked directory lands in
            // the `else` branch and is charged its own link size only.
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => walk(&entry.path(), total),
                Ok(_) => {
                    if let Ok(meta) = entry.metadata() {
                        *total = total.saturating_add(meta.len());
                    }
                }
                Err(_) => {}
            }
        }
    }
    if std::fs::read_dir(path).is_err() {
        return None;
    }
    let mut total = 0u64;
    walk(path, &mut total);
    Some(total)
}

/// Collect `root`'s worktree census (Issue #5939).
///
/// Best-effort in the same sense as
/// [`crate::quarantine_stash_status::collect_stash_summary`]: a root with no
/// worktree root yet, or one that cannot be read, degrades to zero counts and
/// `total_bytes: None` rather than propagating an error — one repo's census
/// must never block `loom-daemon status` for its siblings.
#[must_use]
pub fn collect_worktree_disk_summary(root: &Path) -> WorktreeDiskSummary {
    let worktrees_dir = crate::worktree_root::worktree_root(root);
    let names: Vec<String> = match std::fs::read_dir(&worktrees_dir) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect(),
        Err(_) => {
            return WorktreeDiskSummary {
                root: root.to_path_buf(),
                ..WorktreeDiskSummary::default()
            }
        }
    };
    let (issue_count, pr_count, other_count) = classify_worktree_names(&names);
    WorktreeDiskSummary {
        root: root.to_path_buf(),
        total_count: names.len(),
        issue_count,
        pr_count,
        other_count,
        total_bytes: dir_size_bytes(&worktrees_dir),
    }
}

/// Render a byte count the way `du -h` would — the units an operator reading
/// `status` next to a disk-headroom figure is already thinking in.
#[must_use]
pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn classifies_each_naming_class() {
        let names = [
            "issue-42",
            "issue-5939",
            "pr-5312",
            "pr-5349",
            "pr-5362",
            "docs-guide",
            "pr-abc",
            "issue-",
        ];
        let (issue, pr, other) = classify_worktree_names(&names);
        assert_eq!(issue, 2);
        assert_eq!(pr, 3);
        // `docs-guide`, `pr-abc` and `issue-` match neither filter.
        assert_eq!(other, 3);
    }

    #[test]
    fn classify_of_nothing_is_all_zero() {
        let (issue, pr, other) = classify_worktree_names::<&str>(&[]);
        assert_eq!((issue, pr, other), (0, 0, 0));
    }

    #[test]
    fn dir_size_bytes_sums_nested_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a"), vec![0u8; 100]).unwrap();
        std::fs::create_dir_all(tmp.path().join("nested/deeper")).unwrap();
        std::fs::write(tmp.path().join("nested/b"), vec![0u8; 250]).unwrap();
        std::fs::write(tmp.path().join("nested/deeper/c"), vec![0u8; 1]).unwrap();
        assert_eq!(dir_size_bytes(tmp.path()), Some(351));
    }

    #[test]
    fn dir_size_bytes_is_none_for_a_missing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(dir_size_bytes(&tmp.path().join("nope")), None);
    }

    #[test]
    fn dir_size_bytes_of_an_empty_directory_is_zero_not_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(dir_size_bytes(tmp.path()), Some(0));
    }

    #[cfg(unix)]
    #[test]
    fn dir_size_bytes_does_not_follow_symlinks_out_of_the_tree() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("big"), vec![0u8; 10_000]).unwrap();

        let tmp = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("link")).unwrap();

        let measured = dir_size_bytes(tmp.path()).unwrap();
        assert!(
            measured < 10_000,
            "symlinked tree must not be charged to this worktree (got {measured})"
        );
    }

    // `collect_worktree_disk_summary` reads `LOOM_WORKTREE_ROOT` through
    // `worktree_root()`, which is process-global — join the crate-default
    // unkeyed serial group, same as `worktree_reaper`'s reaper tests (#5164).
    #[test]
    #[serial]
    fn summary_counts_both_classes_and_measures_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path().join(".loom/worktrees");
        for name in ["issue-100", "pr-5312", "pr-5349", "scratch"] {
            std::fs::create_dir_all(wt.join(name)).unwrap();
            std::fs::write(wt.join(name).join("f"), vec![0u8; 10]).unwrap();
        }

        let summary = collect_worktree_disk_summary(tmp.path());
        assert_eq!(summary.root, tmp.path());
        assert_eq!(summary.total_count, 4);
        assert_eq!(summary.issue_count, 1);
        assert_eq!(summary.pr_count, 2);
        assert_eq!(summary.other_count, 1);
        assert_eq!(summary.total_bytes, Some(40));
    }

    #[test]
    #[serial]
    fn summary_for_a_root_with_no_worktree_dir_is_empty_not_a_panic() {
        let tmp = tempfile::tempdir().unwrap();
        let summary = collect_worktree_disk_summary(tmp.path());
        assert_eq!(summary.total_count, 0);
        assert_eq!(summary.pr_count, 0);
        assert_eq!(summary.total_bytes, None, "unmeasurable is None, never a false 0");
    }

    #[test]
    fn format_bytes_uses_du_style_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_bytes(27 * 1024 * 1024 * 1024), "27.0 GB");
    }
}
