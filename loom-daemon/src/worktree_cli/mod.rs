//! `worktree.sh`'s logic, ported incrementally (#8195, epic #7810).
//!
//! `worktree.sh` is the second most defect-dense script in the portable pool —
//! 26 fix-commits in six months over 1,812 code lines — and the most dangerous
//! by consequence: it performs 32 irreversible operations (`rm -rf`,
//! `git worktree remove`, `git branch -D`, `git push`, `--force`). Three of
//! those fixes were data-loss classes: an unquoted path causing `rm -rf` on a
//! LIVE worktree (#7858), another agent's uncommitted work discarded on a
//! raced remove (#6706), and a lock released without checking ownership
//! (#6017).
//!
//! It is frozen by the file-size ratchet at 1,812 lines, so every future fix
//! must be net-zero or smaller — in a file whose recent fixes were *adding*
//! guards. A port is what breaks that.
//!
//! Slice 1 is [`lock`]: the repo-global worktree-add lock that every
//! destructive path is supposed to stand behind, and which #6014/#6017 showed
//! could be released by a holder that no longer owned it.
//!
//! Slice 2 is the WIP-shelving family — [`snapshot`] and the [`baseline`]
//! `stash-push`/`stash-pop` pair, over shared plumbing in [`wip`]. They are
//! the verbs whose whole job is *not losing uncommitted work*: they exist
//! because `refs/stash` is repo-global and two builders shelving at once
//! clobbered each other (#4821), and `stash-push` runs `git reset --hard HEAD`
//! once its capture has succeeded.

pub mod baseline;
pub mod lock;
pub mod snapshot;
pub mod wip;
