//! Resolve the Loom repository root for the `worktree_ops` CLIs (`clean`,
//! `cleanup`, `recover-orphans`).
//!
//! This module used to carry its own walk that accepted **any** ancestor
//! containing a `.loom/` directory, with no `.git` check — so on any host with
//! machine-level daemon state at `~/.loom` (the token pool provisioned by
//! `loom-daemon tokens bootstrap`), running these CLIs from `$HOME` resolved
//! `$HOME` as the "repo root" and then ran `gh` there, which failed with
//! `fatal: not a git repository`. Repo-root resolution now comes from the
//! single-source [`crate::repo_root`] helper shared by every entry point
//! (issue #5140).

pub use crate::repo_root::{find_repo_root, resolve_repo_root};
