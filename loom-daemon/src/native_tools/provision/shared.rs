//! One OpenCode binding tree per (workspace, binding content) — #8663.
//!
//! # Why this is not per-launch
//!
//! `OPENCODE_CONFIG_DIR` used to be a leaf of the per-launch session directory
//! ([`super::state`]). The two files Loom writes there are tiny, but the CLI
//! then resolves the pinned `@opencode-ai/plugin` dependency into
//! `node_modules/` beside them: ~126 MB and ~7,300 files, re-installed for
//! every launch and never removed. Three fleet hosts were holding 30–37 GB of
//! it, and one hit ENOSPC twice in a day.
//!
//! The content is identical for every launch of a given daemon build — the
//! plugin is `include_str!`'d and the dependency is pinned — so the tree is
//! keyed on the bytes themselves and reused. A pin bump or plugin edit is a
//! different key and therefore a different tree; nothing has to be invalidated.
//!
//! # What stays per-launch
//!
//! Everything mutable. `XDG_DATA_HOME` / `XDG_STATE_HOME` / `XDG_CACHE_HOME` /
//! `XDG_CONFIG_HOME`, the auth snapshot and the session store all remain inside
//! the launch's own 0700 directory ([`super::state::State::configure`]), so two
//! concurrent workers still share no credential, transcript or session state.
//! What they now share is a read-only package tree, per workspace.
//!
//! # Concurrency
//!
//! Publication is an atomic `rename` of a fully written staging tree, so a
//! reader sees either no tree or a complete one. A non-blocking
//! [`MkdirLock`] keeps N simultaneous cold launches from each building their
//! own staging copy; losing it is never fatal, because the `rename` is the
//! real serialization point and the loser adopts the winner's tree.
//!
//! The dependency install itself belongs to the CLI, not to Loom, and is not
//! serialized here: simultaneous *cold* launches can still each run it in the
//! shared tree. That window is one install per (workspace, binding version)
//! rather than one per launch, and [`super::write_opencode_bindings`] below is
//! re-run on every launch, so a tree whose manifest or plugin was damaged is
//! rewritten rather than left broken.

use super::state;
use crate::tokens_pool::locking::MkdirLock;
use anyhow::{ensure, Result};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

/// Stale threshold for the advisory publication lock.
const LOCK_STALE: Duration = Duration::from_secs(600);

/// Bump when the on-disk layout under a key changes, so an old tree is never
/// read as if it had the new shape.
const LAYOUT: &str = "opencode-bindings-v1";

/// The key a binding tree is stored under: a digest over the layout tag and
/// every provisioned byte, length-prefixed so no two inputs can collide by
/// concatenation.
#[must_use]
pub(super) fn binding_key() -> String {
    let mut hasher = Sha256::new();
    for part in [
        LAYOUT.as_bytes(),
        include_str!("../opencode.mjs").as_bytes(),
        super::OPENCODE_PLUGIN_MANIFEST.as_bytes(),
    ] {
        hasher.update(part.len().to_le_bytes());
        hasher.update(part);
    }
    hex::encode(hasher.finalize())
}

/// The `OPENCODE_CONFIG_DIR` this launch should use: a tree shared by every
/// launch of `workspace` that provisions the same bindings.
///
/// # Errors
///
/// Propagates a filesystem failure, including a tree that cannot be made
/// 0700-private and a publication that neither succeeded nor found a peer's.
pub(super) fn opencode_config_dir(workspace: &Path) -> Result<PathBuf> {
    let key = binding_key();
    let entry = workspace.join(super::reap::BINDINGS).join(&key);
    if !entry.is_dir() {
        let lock = workspace.join(format!(".binding-{key}.lock"));
        // Advisory: a peer holding it means duplicated staging work, never a
        // wrong result — `publish`'s rename decides who wins.
        let _lock = MkdirLock::try_acquire(&lock, LOCK_STALE).ok().flatten();
        if !entry.is_dir() {
            publish(workspace, &entry)?;
        }
    }
    let config_dir = entry.join("opencode");
    // Idempotent (a matching file is not rewritten), and the self-heal for a
    // tree an operator or a half-finished install left incomplete.
    super::write_opencode_bindings(&config_dir)?;
    super::reap::mark_used(&entry);
    Ok(config_dir)
}

/// Build the tree in a private staging directory and move it into place with a
/// single `rename`.
fn publish(workspace: &Path, entry: &Path) -> Result<()> {
    state::private_directory(&workspace.join(super::reap::BINDINGS))?;
    let staging = workspace.join(super::reap::STAGING);
    state::private_directory(&staging)?;
    let staged = staging.join(uuid::Uuid::new_v4().to_string());
    state::private_directory(&staged)?;
    if let Err(error) = super::write_opencode_bindings(&staged.join("opencode")) {
        // Never leave a half-built tree where the reaper has to age it out.
        let _ = fs::remove_dir_all(&staged);
        return Err(error);
    }
    if let Err(error) = fs::rename(&staged, entry) {
        // A peer publishing first is the expected loss: its tree is
        // authoritative, ours is discarded, and the content is identical by
        // construction. Anything else is a real failure, named by `error`.
        let _ = fs::remove_dir_all(&staged);
        ensure!(
            entry.is_dir(),
            "cannot publish the shared OpenCode binding tree at {} and no peer published one: \
             {error}",
            entry.display()
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "shared_tests.rs"]
mod tests;
