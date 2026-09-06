//! Native Rust port of the `loom_tools` worktree/cleanup family (epic #4081
//! Phase 3, family 2 — issue #4272).
//!
//! Ports `clean.py` (2154 lines), `orphan_recovery.py` (1296 lines), and
//! `cleanup.py` (255 lines) to `loom-daemon clean` / `loom-daemon
//! recover-orphans` / `loom-daemon cleanup logs`. `worktree.py` was **not**
//! ported — it is pure argparse-over-bash glue with zero execution-path
//! callers outside its own tests, so it and its entry point are deleted
//! outright (`defaults/scripts/worktree.sh` never delegated to it).
//!
//! Submodule map:
//! - [`clean`] — `loom-clean`: worktree/branch/tmux/agent-config/build-artifact
//!   cleanup, plus `--daemon` crash recovery.
//! - [`cargo_target`] — reclaim a worktree's REDIRECTED cargo target dir when
//!   the worktree is removed (#7239): a `CARGO_TARGET_DIR`/`build.target-dir`
//!   redirect points build output outside the worktree, where no removal path
//!   ever looked.
//! - [`aggressive`] — `loom-clean --aggressive`'s vestigial-worktree decision tree.
//! - [`orphan_recovery`] — `loom-recover-orphans`.
//! - [`logs`] — `loom-cleanup logs` (the only cleanup.py functionality that
//!   survived the daemon-brain retirement, #3396).
//! - [`removal_log`] — the worktree-removal ledger (#5950): every Loom-owned
//!   worktree removal, from any path, appended to one greppable file so
//!   "what removed this worktree?" has a single answer.
//! - `repo`, `naming`, `safety`, `claim_file`, `spawn_loop_state`, `liveness`
//!   — internal helpers shared across the above (not part of the public
//!   surface; see each module's doc comment for its Python counterpart).
//! - [`gh`] — likewise internal to the family, but exported crate-wide (not
//!   just within this lib crate) so `loom-daemon checkpoint read`'s CLI arm
//!   (`cli/legacy_script_cmds.rs`, binary crate) can reuse its `gh
//!   issue view`/`gh api` issue-state lookup for checkpoint staleness (#5403)
//!   instead of adding a second forge call path.

pub mod aggressive;
pub mod cargo_target;
pub mod claim_file;
pub mod clean;
pub mod gh;
pub(crate) mod liveness;
pub mod logs;
pub(crate) mod naming;
pub mod orphan_recovery;
pub mod removal_log;
pub mod repo;
pub(crate) mod safety;
mod spawn_loop_state;

pub use claim_file::{
    has_valid_claim, is_abandoned as claim_is_abandoned, is_expired as claim_is_expired,
};

/// Re-exported (issue #4876) because it appears in the public signature of
/// [`clean::WorktreeProbes`], which the daemon-side reaper constructs.
pub use safety::InUseMarker;
