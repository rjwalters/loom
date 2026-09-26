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
//!
//! Slice 3 is [`remove`]: `worktree.sh remove <N>`, the verb every irreversible
//! operation in that script is reachable from (`git worktree remove --force`,
//! the #5177 `rm -rf` fallback, `git branch -D`). It brings three supporting
//! ports with it, because the shell obtained all three by `source`-ing or
//! `eval`-ing them and Rust cannot: [`default_branch`]
//! (`lib/default-branch.sh`), [`branch_landed`] (`lib/branch-landed.sh` — the
//! proof that gates the force-delete), and [`branch_delete`] (`merge-pr.sh`'s
//! `_maybe_delete_local_branch`, which `worktree.sh` used to `awk` out of the
//! live script source and `eval` into its own process).
//!
//! Slice 4 is [`link`]: the post-`git worktree add` provisioning family —
//! root and nested `node_modules`, `worktree.linkPaths`, `.mcp.json`, and the
//! `info/exclude` entry each one needs (#3528/#5474). It is the part of the
//! create path that is *all path interpolation* — four `ln -s "$src" "$dst"`
//! pairs, a `find … -print0 | read -d ''` loop and a `grep -qxF "$entry"
//! "$file"` — which is precisely #7858's class, and it is the one cohesive
//! unit in that path touching none of the arms under concurrent repair
//! (#8351, #8702, #8486).
//!
//! Slice 5 is [`cleanup`]: the crash-debris pre-flight, and with it the
//! **orphan guard** — the predicate whose false answer `rm -rf`s a worktree
//! directory. It is the site of #7858/#7849, the data-loss class the issue
//! leads with: a `git worktree list --porcelain` path split on whitespace
//! (truncating at the first space) and a candidate resolved logically instead
//! of physically, either of which made a LIVE worktree with uncommitted work
//! in it read as an unregistered orphan. Both halves are structural here, and
//! the porcelain read is now literally [`branch_delete::parse_worktree_porcelain`]
//! — the shell's own comment said it "mirrors" that function, and mirroring is
//! what drifts.
//!
//! [`branch_landed`] is the daemon's ONE Rust copy of that ladder:
//! [`crate::worktree_ops::landed`] (`clean --aggressive`) is an adapter over
//! [`branch_landed::ladder`] since #8470 — see [`branch_landed`]'s module doc
//! for how the two call sites differ and the strictness change it made.
//!
//! [`issue_lock`] is not a port slice: it is `worktree.sh` growing a NEW
//! guard (#8553) that stands entirely on the Rust side by construction — this
//! file is frozen by the file-size ratchet, so the shell side stays a single
//! delegating call. It reads a lock [`lock`] never touches: the daemon's
//! per-issue sweep-CLAIM lock (`sweep_registry::locks`), not the repo-global
//! worktree-add mutex.

pub mod baseline;
pub mod branch_delete;
pub mod branch_landed;
pub mod cleanup;
pub mod default_branch;
pub mod issue_lock;
pub mod link;
pub mod lock;
pub mod remove;
pub mod snapshot;
pub mod wip;
