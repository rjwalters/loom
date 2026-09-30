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
//! Slice 6 is [`reset`]: the rescue-or-refuse guard in front of the
//! stale-worktree `git reset --hard`, and with it the last of the three
//! data-loss classes the issue leads with — *"rescue foreign work instead of
//! discarding it on a raced reset/remove"* (#6706), from the #6320 incident
//! where an unqualified reset discarded a second builder's uncommitted work.
//! It is also where the issue's "two implementations of *is this worktree safe
//! to touch*" is literally true: the shell's liveness probe *claimed* to mirror
//! [`crate::worktree_ops::safety::find_processes_using_directory`] and had
//! drifted from it (cwd-only vs. the #7466 any-open-fd scan), so the port
//! reaches the one `/proc` walk and one `lsof` parse in the codebase through a
//! flag instead of keeping a bash copy alongside.
//!
//! [`issue_lock`] is not a port slice: it is `worktree.sh` growing a NEW
//! guard (#8553) that stands entirely on the Rust side by construction — this
//! file is frozen by the file-size ratchet, so the shell side stays a single
//! delegating call. It reads a lock [`lock`] never touches: the daemon's
//! per-issue sweep-CLAIM lock (`sweep_registry::locks`), not the repo-global
//! worktree-add mutex.
//!
//! Slice 7 is [`branch_conflict`]: `_handle_feature_branch_in_main_worktree`,
//! the recovery `_try_worktree_add` falls into when `git worktree add` refuses
//! because the target branch is already checked out in the main workspace. It
//! is the one arm of the create path that is *pure string parsing of an
//! arbitrary git error message* — a `grep -o … | sed 's/…//'` extraction of a
//! quoted path, then a raw string comparison — which is #7858's class again,
//! this time in a guard that decides whether to auto-switch a workspace
//! rather than whether to `rm -rf` one.
//!
//! Slice 8 is [`submodules`]: the other post-`git worktree add` provisioning
//! step, the sibling of [`link`]. Fifty lines of shell holding four defects
//! that never changed a printed line — an `awk '{print $2}'` work list
//! (#7858's class again, and here the truncated string is used as both a
//! `--reference` directory and a git pathspec), a `timeout(1)` that a stock
//! macOS does not have, a `--reference` fast path whose `[[ -d ]]` test was
//! evaluated from the wrong directory and therefore never once fired, and a
//! `$$`-keyed failure flag written into world-writable `/tmp`.
//!
//! Slice 9 is [`upstream`]: the upstream-tracking correction and the
//! stale-worktree drift report. It is the one slice that took **two** blocks of
//! the script at once, because they were two hand-maintained copies of the same
//! fix — #6095/#6100 corrected a branch's wrong upstream on the reuse arm, and
//! #6257 discovered eighteen months later that "a completely different code
//! path" (its own words, still in the script) needed the identical correction
//! and re-implemented it by hand. This is the epic's *"two implementations of
//! the same question"* in its most literal form, and both defects shipped
//! **silently**: their failure mode is not a wrong message but no message, and
//! an upstream left pointing at the default branch so a later
//! `git pull --ff-only` fast-forwards a PR branch onto `main`'s tip.
//!
//! [`stale_ref`] is not a port slice either: like [`issue_lock`] it is
//! `worktree.sh` growing NEW logic (#8287) that has to stand on the Rust side
//! because that file is frozen — and, more pointedly, because the logic's hard
//! part is a [`branch_landed`] question, which now has exactly one
//! implementation. Its first attempt (PR #8351) added the decision as inline
//! bash to a `contract`-category library and the shell budget ratchet refused
//! it; #8354 is that refusal honoured rather than argued with. It answers
//! *which* reference the already-registered-worktree fast path may judge
//! staleness — and therefore `git reset --hard` — against, when a live
//! `origin/<branch>` carries the branch's real commits and the local branch
//! does not (the #8147/#8190 incident).
//!
//! [`closed_pr_branch`] is the same kind of addition (#9083): the third arm of
//! the branch-resolution contract the shell had no answer for — a pushed
//! `origin/feature/issue-N` whose tip is the head of a PR CLOSED WITHOUT
//! MERGING. It is deliberately NOT a fourth [`branch_landed`] verdict; see its
//! module doc for why that primitive's three-way answer stays three-way.
//!
//! Slice 10 is [`sparse`]: `--sparse <paths...>` and `--full`, on both arms
//! they reach. It is the one part of the create path that is opt-in, so it
//! could leave the shell outright without moving the ordinary
//! `worktree.sh <N>` onto a built binary. It carried a silent exit 128 on any
//! cone git rejects, a cone-to-JSON builder that did not escape, and a
//! re-configure arm whose "is this a registered worktree?" was an unanchored
//! `grep` substring match — which could disagree with [`cleanup`]'s orphan
//! guard about the same directory, and on its false-positive side wrote a
//! [`sentinel`] into a directory git did not know about. The port asks
//! [`cleanup::is_registered`]. [`sentinel`] is the first Rust writer of the
//! `.loom-managed` marker, pinned byte-for-byte to the shell's until the
//! remaining writers move.
//!
//! Slice 11 is [`check`]: the predicate *"am I inside a linked worktree?"* and
//! both decisions it gated — the `--check` verb and the create path's
//! auto-navigation out of a worktree. It is the only slice whose retired code
//! was **wrong from every position a caller can stand in**: the comparison was
//! `--git-common-dir` (which git answers *relative to the current directory*
//! whenever it can) against an absolute `<toplevel>/.git`, so it was
//! constant-true. `--check` reported the primary clone as a worktree and its
//! "not in a worktree" arm was unreachable; every `worktree.sh <N>` run from
//! the primary clone printed four spurious navigation lines and then `cd`'d to
//! `dirname ".git"` — a no-op by luck rather than by logic. Invisible because
//! `--json` suppresses all four lines, so nothing a machine reads ever changed
//! and no retained suite asserts either consumer's output. Same class as slices
//! 5, 8 and 10: a path compared logically instead of physically.

pub mod baseline;
pub mod branch_conflict;
pub mod branch_delete;
pub mod branch_landed;
pub mod check;
pub mod cleanup;
pub mod closed_pr_branch;
pub mod default_branch;
pub mod issue_lock;
pub mod link;
pub mod lock;
pub mod remove;
pub mod reset;
pub mod sentinel;
pub mod snapshot;
pub mod sparse;
pub mod stale_ref;
pub mod submodules;
pub mod upstream;
pub mod wip;
