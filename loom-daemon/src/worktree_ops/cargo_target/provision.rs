//! Creation-time half of the per-worktree target-dir scheme (issue #8458).
//!
//! [`super::per_worktree`] owns the *predicates* — the structural attribution
//! check and the marker reader — which the removal paths need. This owns the
//! *decision and the write*: whether the repo opted in, what directory this
//! worktree should build into, and the `mkdir` + marker write that makes it so.
//!
//! # Why this is Rust and not shell
//!
//! `worktree.sh` and `spawn-claude.sh` are both in the shell budget's
//! `contract` (portable) pool, whose growth the ratchet refuses with no
//! `Shell-Budget-Growth:` override (epic #7810, `.loom/docs/shell-language-policy.md`).
//! A first cut of this issue put the whole decision in
//! `defaults/scripts/lib/cargo-target-dir.sh` and added 138 portable lines; the
//! gate refused it, correctly. The two scripts now call
//! `loom-daemon cargo-target-dir provision|path` (see `cli/cargo_target_dir.rs`)
//! and the logic lives here.
//!
//! The bash library keeps only the two predicates the **removal** paths cannot
//! delegate — `merge-pr.sh` and `worktree.sh remove` already resolve a target
//! dir in bash, and marker-first resolution has to happen inside that existing
//! resolver, not beside it (the issue's "single source of truth" criterion).
//!
//! # What is deliberately NOT done here
//!
//! Nothing is relocated. If Cargo's output for this worktree would already land
//! *inside* the worktree — the unredirected host — provisioning is a no-op:
//! `<worktree>/target` is already per-worktree and is removed with the worktree,
//! so splitting it a level deeper buys nothing and costs a full rebuild. That
//! rebuild-for-no-benefit is exactly the #6013/#6014 failure mode, so the gate
//! is structural rather than advisory.

use std::path::{Path, PathBuf};

use super::per_worktree;

/// Config key (and its `LOOM_`-prefixed env override) that turns the scheme on.
pub const CONFIG_KEY: &str = "cargo.perWorktreeTargetDir";
/// Env override for [`CONFIG_KEY`]. Precedence: env > config > default OFF.
pub const ENABLE_ENV: &str = "LOOM_PER_WORKTREE_TARGET_DIR";

/// What [`provision`] decided, so the caller can render it and tests can assert
/// on it without inspecting the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provision {
    /// A valid marker was already present; its directory is authoritative.
    /// `worktree.sh <N>` is idempotent and re-run against live worktrees, and
    /// re-deriving could relocate the dir out from under build output already
    /// in it.
    Existing(PathBuf),
    /// Provisioned: the directory was created and the marker written.
    Provisioned(PathBuf),
    /// Not a cargo tree — nothing to redirect.
    NotCargo,
    /// The repo has not opted in.
    Disabled,
    /// Cargo's output for this worktree already lands inside the worktree.
    Unredirected(PathBuf),
    /// The derived path did not carry the per-worktree shape, so writing a
    /// marker naming it would strand the directory forever.
    Refused(PathBuf),
    /// The `mkdir` or the marker write failed.
    Failed(String),
}

impl Provision {
    /// The directory the caller should export, when there is one.
    #[must_use]
    pub fn dir(&self) -> Option<&Path> {
        match self {
            Self::Existing(p) | Self::Provisioned(p) => Some(p),
            _ => None,
        }
    }

    /// One operator-facing line, or `None` for the outcomes that describe every
    /// host where the feature is simply not in play.
    #[must_use]
    pub fn report_line(&self) -> Option<String> {
        match self {
            Self::NotCargo | Self::Disabled | Self::Unredirected(_) => None,
            Self::Existing(p) => {
                Some(format!("per-worktree cargo target dir (reused): {}", p.display()))
            }
            Self::Provisioned(p) => Some(format!("per-worktree cargo target dir: {}", p.display())),
            Self::Refused(p) => Some(format!(
                "not provisioning a per-worktree cargo target dir: {} is not attributable to \
                 this worktree",
                p.display()
            )),
            Self::Failed(why) => {
                Some(format!("could not provision a per-worktree cargo target dir: {why}"))
            }
        }
    }
}

/// Is the scheme turned on for `repo_root`? `LOOM_PER_WORKTREE_TARGET_DIR`
/// (env) > `cargo.perWorktreeTargetDir` (config) > **default OFF**.
///
/// Default OFF is the #6013/#6014 lesson rather than timidity. On a host with
/// no redirect configured the scheme is a no-op anyway; on a host *with* one it
/// trades cross-worktree reuse of third-party `deps/` for isolation. That trade
/// is excellent when a `rustc-wrapper` (sccache) carries third-party crates —
/// the configuration #8453 measured — and is a fleet-wide rebuild storm when
/// nothing does, which is precisely the #6013/#6014 incident. So it is the
/// operator's switch, thrown on the same host where the shared
/// `build.target-dir` was configured.
#[must_use]
pub fn enabled(repo_root: &Path) -> bool {
    if let Ok(raw) = std::env::var(ENABLE_ENV) {
        if !raw.trim().is_empty() {
            return truthy(raw.trim());
        }
    }
    let config = crate::config_resolver::resolve_effective_config(repo_root);
    match crate::config_resolver::get_path(&config, CONFIG_KEY) {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => truthy(s.trim()),
        _ => false,
    }
}

fn truthy(value: &str) -> bool {
    matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

/// Decide and act: give `worktree_path` its own Cargo target dir and record it,
/// so the builds Loom drives there cannot collide with a sibling's and the
/// removal paths can attribute and reclaim the directory.
///
/// Never fails the caller — a worktree must still be created when this cannot
/// be done — so every problem is an [`Provision`] variant, not an `Err`.
pub fn provision(repo_root: &Path, worktree_path: &Path) -> Provision {
    provision_with(repo_root, worktree_path, &resolve_root)
}

/// [`provision`] with the target-dir resolution injected, so the decision is
/// testable without a real `cargo metadata`.
pub fn provision_with(
    repo_root: &Path,
    worktree_path: &Path,
    resolve_root: &dyn Fn(&Path) -> PathBuf,
) -> Provision {
    // An existing valid marker is authoritative — see [`Provision::Existing`].
    // Checked BEFORE the opt-in so that turning the feature off does not strand
    // a directory an earlier run already provisioned and pointed builds at.
    if let Some(existing) = per_worktree::marker_value(worktree_path) {
        let _ = std::fs::create_dir_all(&existing);
        return Provision::Existing(existing);
    }
    if !worktree_path.join("Cargo.toml").is_file() {
        return Provision::NotCargo;
    }
    if !enabled(repo_root) {
        return Provision::Disabled;
    }

    // FULL resolution, not the cheap attribution pre-check: this is the "what
    // would Cargo do here" question, and the shared root that makes the feature
    // worth having comes from `~/.cargo/config.toml` — a source the attribution
    // pre-check deliberately refuses to look at.
    let root = resolve_root(worktree_path);
    let root_real = realish(&root);
    let worktree_real = realish(worktree_path);
    if root_real == worktree_real || root_real.starts_with(&worktree_real) {
        return Provision::Unredirected(root);
    }

    let Some(name) = worktree_path.file_name().and_then(|n| n.to_str()) else {
        return Provision::Refused(root);
    };
    let dir = per_worktree::dir_for(&root, name);

    // Self-check: never write a marker the reclaim path would not recognize.
    // A shape mismatch here would strand the directory forever.
    if !per_worktree::is_attributable(worktree_path, &dir) {
        return Provision::Refused(dir);
    }

    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Provision::Failed(format!("could not create {}: {e}", dir.display()));
    }
    let marker = worktree_path.join(per_worktree::MARKER_FILE);
    if let Err(e) = std::fs::write(&marker, format!("{}\n", dir.display())) {
        return Provision::Failed(format!("could not write {}: {e}", marker.display()));
    }
    Provision::Provisioned(dir)
}

/// The directory a worktree that does not exist yet *would* get, for the spawn
/// path — which runs before the sweep creates the worktree and so cannot read a
/// marker. Returns `None` when the feature is off, or when Cargo's output for
/// that worktree would already land inside it (the unredirected host, where
/// exporting anything would relocate a build cache for no benefit).
#[must_use]
pub fn planned_dir(repo_root: &Path, worktree_path: &Path) -> Option<PathBuf> {
    if !enabled(repo_root) {
        return None;
    }
    let root = resolve_root(repo_root);
    let root_real = realish(&root);
    let worktree_real = realish(worktree_path);
    if root_real == worktree_real || root_real.starts_with(&worktree_real) {
        return None;
    }
    let name = worktree_path.file_name().and_then(|n| n.to_str())?;
    let dir = per_worktree::dir_for(&root, name);
    per_worktree::is_attributable(worktree_path, &dir).then_some(dir)
}

/// Cargo's own answer for `root`, with this process's `CARGO_TARGET_DIR` and a
/// real `cargo metadata` in play — the production resolution, shared by
/// [`provision`] and [`planned_dir`] so the two can never disagree about where
/// the shared root is.
fn resolve_root(root: &Path) -> PathBuf {
    super::resolve_target_dir_with(
        root,
        super::env_cargo_target_dir().as_deref(),
        &super::cargo_metadata_target_directory,
    )
}

/// Best-effort physical path; a non-existent path is returned unchanged rather
/// than dropped, because the containment checks still need something to compare.
fn realish(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
mod tests;
