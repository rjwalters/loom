//! `loom-daemon worktree-link --retire-aliases` — retire the `node_modules`
//! aliases that worktrees created BEFORE #8944 still carry (#9152).
//!
//! # Why this exists
//!
//! #8944 stopped [`super::run`] creating a `node_modules` symlink on a pnpm
//! workspace, because pnpm purges THROUGH such a link into the main
//! workspace (see the parent module's "Why pnpm workspaces are excluded").
//! That fix only fires on the create path. Re-running `worktree.sh <N>` for an
//! existing worktree returns early ("preserving existing work") and never
//! reaches `worktree-link`, so every worktree created before the fix keeps
//! its dangerous alias until it is removed and recreated.
//!
//! # Why a standalone verb, not the reuse path
//!
//! Firing this from `worktree.sh`'s reuse path would be a create-path
//! control-flow change, and that path has arms under concurrent repair
//! (#8351 stale-reset, #8702 claim-lock pre-flight, #8486 post-add cargo
//! target dir) — the linking family was chosen precisely because it touches
//! none of them. An operator runs this once per repo instead; it changes no
//! control flow anywhere.
//!
//! # What it removes, and what it can never remove
//!
//! Only a **symlink** named `node_modules` (the root one, or a nested
//! per-package one at the depths family 2 links) whose target resolves to an
//! existing directory **inside the main workspace** and **outside the
//! worktree itself**. Everything else is left alone: a real `node_modules`
//! directory (someone's own `pnpm install`), a link pointing elsewhere on
//! disk, a dangling link, and every path in the main workspace itself.
//!
//! The removal is [`fs::remove_file`], i.e. `unlink(2)`, which removes the
//! link and never its target. It is also structurally incapable of deleting a
//! real directory: `unlink` on a directory fails with `EISDIR`/`EPERM`, so
//! even a race that swapped the link for a directory between the check and
//! the removal ends in a warning, not a deletion. No `remove_dir_all` exists
//! in this file.
//!
//! The `info/exclude` entry the original link added is left in place on
//! purpose: it ignores a path that no longer exists, and a later real
//! `node_modules` wants ignoring anyway.
//!
//! # Gate
//!
//! Runs only on a pnpm workspace ([`is_pnpm_workspace`], the same detection
//! the create path uses) whose resolved [`NodeModulesPolicy`] is not
//! [`NodeModulesPolicy::Link`] — a repo that set
//! `worktree.linkNodeModules=true` has opted into the aliasing and is not
//! second-guessed. On any other repo it reports why and does nothing.
//!
//! # Exit code
//!
//! Unlike [`super::run`] this is an operator verb, so failure is reported: 0
//! when every alias found was retired (or there was nothing to do), 1 when
//! the main workspace could not be resolved or an unlink failed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{is_pnpm_workspace, NodeModulesPolicy, Out, Reporter, MAX_SCAN_DEPTH};

/// What to retire, and where.
pub struct RetireOptions {
    /// The main workspace root, whose `node_modules` the aliases point into.
    pub repo_root: PathBuf,
    /// One worktree to fix. `None` means every worktree `git worktree list`
    /// knows about (the main workspace itself is always skipped).
    pub worktree: Option<PathBuf>,
    /// Print nothing.
    pub quiet: bool,
}

/// Retire every qualifying alias. See the module docs for the exit code.
pub fn retire_aliases(opts: &RetireOptions) -> i32 {
    let out = Reporter {
        out: Out::new(false),
        quiet: opts.quiet,
    };

    if !is_pnpm_workspace(&opts.repo_root) {
        out.info("Not a pnpm workspace: a node_modules symlink is not the #8944 hazard here.");
        out.info("Nothing retired.");
        return 0;
    }
    if NodeModulesPolicy::resolve(&opts.repo_root) == NodeModulesPolicy::Link {
        out.info("worktree.linkNodeModules=true opts this repo back into node_modules aliasing.");
        out.info("Nothing retired.");
        return 0;
    }
    let Ok(main) = fs::canonicalize(&opts.repo_root) else {
        out.warning(&format!(
            "Cannot resolve main workspace {}; nothing retired",
            opts.repo_root.display()
        ));
        return 1;
    };

    let worktrees = match &opts.worktree {
        Some(one) => vec![one.clone()],
        None => list_worktrees(&opts.repo_root),
    };

    let mut retired = 0usize;
    let mut failed = false;
    for worktree in worktrees {
        let Ok(worktree_canon) = fs::canonicalize(&worktree) else {
            continue;
        };
        // Never touch the main workspace's own tree, however it was named.
        if worktree_canon == main {
            continue;
        }
        let mut retired_here = false;
        for link in candidate_links(&worktree) {
            let Some(target) = alias_target(&link, &main, &worktree_canon) else {
                continue;
            };
            match unlink_symlink(&link) {
                Ok(()) => {
                    retired += 1;
                    retired_here = true;
                    out.success(&format!(
                        "Retired node_modules alias {} -> {} (#9152)",
                        link.display(),
                        target.display()
                    ));
                }
                Err(err) => {
                    failed = true;
                    out.warning(&format!("Could not retire {}: {err}", link.display()));
                }
            }
        }
        if retired_here {
            out.info(&format!(
                "  Next: run 'pnpm install' in {} (hardlinks from pnpm's store; cheap).",
                worktree.display()
            ));
        }
    }

    if retired == 0 && !failed {
        out.info("No node_modules aliases into the main workspace found; nothing retired.");
    }
    i32::from(failed)
}

/// `git worktree list --porcelain`'s `worktree <path>` lines.
///
/// An unanswerable git (not a repo, no git on PATH) yields no worktrees, so
/// the verb degrades to "nothing found" rather than guessing paths.
fn list_worktrees(repo_root: &Path) -> Vec<PathBuf> {
    let Ok(output) = Command::new("git")
        .current_dir(repo_root)
        .args(["worktree", "list", "--porcelain"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect()
}

/// Every `node_modules` SYMLINK at the depths the create path linked: the
/// root one (depth 1) and nested per-package ones (depths 2..=3).
///
/// Never follows a symlinked directory and never descends into a real
/// `node_modules` — an alias's contents are the main workspace's, and a real
/// install's contents are not this verb's business. Sorted, so the report
/// order is deterministic.
fn candidate_links(worktree: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    scan_links(worktree, 1, &mut found);
    found.sort();
    found
}

fn scan_links(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        let name = entry.file_name();
        if name == "node_modules" {
            if meta.file_type().is_symlink() {
                found.push(path);
            }
            continue;
        }
        if name == ".git" || !meta.is_dir() {
            continue;
        }
        scan_links(&path, depth + 1, found);
    }
}

/// The canonical target if `link` is an alias this verb may retire, else
/// `None`. All four conditions are required:
///
/// 1. `link` itself is a symlink (`lstat`, not `stat`).
/// 2. Its target resolves (a dangling link is harmless and left alone).
/// 3. The resolved target is a directory inside the main workspace.
/// 4. ...and NOT inside this worktree (a link to the worktree's own tree is
///    not an alias of anyone else's install).
fn alias_target(link: &Path, main: &Path, worktree: &Path) -> Option<PathBuf> {
    let meta = fs::symlink_metadata(link).ok()?;
    if !meta.file_type().is_symlink() {
        return None;
    }
    let target = fs::canonicalize(link).ok()?;
    if !fs::metadata(&target).ok()?.is_dir() {
        return None;
    }
    if !target.starts_with(main) || target.starts_with(worktree) {
        return None;
    }
    Some(target)
}

/// `unlink(2)` the link — never its target, and never a directory.
///
/// Re-checks `lstat` immediately before removing, and even if that check is
/// raced, [`fs::remove_file`] cannot delete a directory.
fn unlink_symlink(link: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(link)?;
    if !meta.file_type().is_symlink() {
        return Err(std::io::Error::other("no longer a symlink; left alone"));
    }
    fs::remove_file(link)
}

#[cfg(test)]
mod tests;
