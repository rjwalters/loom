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
//! - [`landed`] — `aggressive`'s three-way view (#7812) of the one shared
//!   "has this branch landed on the default branch?" ladder,
//!   [`crate::worktree_cli::branch_landed`] (converged in #8470); `aggressive`
//!   consumes it instead of its own raw-reachability heuristic.
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
pub(crate) mod clean_owner;
/// The hygiene read path (W6): issue and PR state, fresh and conditional,
/// for every reaping and cleaning consumer.
pub(crate) mod forge_state;
pub mod gh;
/// One hygiene pass (W6 PR2): what a pass may hold, and the fresh read
/// every removal makes first.
pub(crate) mod hygiene_pass;
/// The one hygiene answer remembered across passes: a merged PR.
pub(crate) mod hygiene_terminal;
pub mod landed;
/// Leg 0 of both open-linked-PR probes: the cached open-PR listing (#10514).
pub(crate) mod linked_pr_listing;
pub(crate) mod liveness;
pub mod logs;
/// `pub` rather than `pub(crate)` since #9444: the `record-rework` subcommand
/// lives in the binary crate and derives an issue number from a PR's branch
/// name, which is exactly what `issue_from_branch` already answers. A second
/// `strip_prefix("feature/issue-")` in the CLI would be a copy of the
/// convention, not a use of it.
pub mod naming;
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
