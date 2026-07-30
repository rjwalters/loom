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
//! - [`aggressive`] — `loom-clean --aggressive`'s vestigial-worktree decision tree.
//! - [`orphan_recovery`] — `loom-recover-orphans`.
//! - [`logs`] — `loom-cleanup logs` (the only cleanup.py functionality that
//!   survived the daemon-brain retirement, #3396).
//! - `repo`, `naming`, `safety`, `claim_file`, `spawn_loop_state`, `liveness`,
//!   `gh` — internal helpers shared across the above (not part of the public
//!   surface; see each module's doc comment for its Python counterpart).

pub mod aggressive;
pub mod claim_file;
pub mod clean;
mod gh;
mod liveness;
pub mod logs;
pub(crate) mod naming;
pub mod orphan_recovery;
pub mod repo;
mod safety;
mod spawn_loop_state;

pub use claim_file::{
    has_valid_claim, is_abandoned as claim_is_abandoned, is_expired as claim_is_expired,
};
