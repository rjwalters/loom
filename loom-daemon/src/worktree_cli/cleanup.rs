//! `worktree.sh`'s crash-debris cleanup — the **orphan guard** (#8195 slice 5,
//! epic #7810).
//!
//! # What moved here
//!
//! `cleanup_partial_worktree_state()`: the pre-flight that clears the residue
//! of a `git worktree add` that was killed part-way through, in the shell's
//! own order (the order is observable — it is the order the warnings print
//! in, and step 3 is conditional on steps 1–2 having done something):
//!
//! 1. The per-worktree file locks git holds for the duration of an add and
//!    releases on success or failure — `index.lock`, `HEAD.lock`,
//!    `gitdir.lock` under `<git-common-dir>/worktrees/issue-<N>/`. A SIGKILLed
//!    process leaves them behind, where they block every later operation
//!    against that administrative dir (#3380/#3416).
//! 2. The **orphan worktree dir**: `<worktree-root>/issue-<N>` exists on disk
//!    but `git worktree list --porcelain` does not know about it, so it is by
//!    definition the shell of a killed add — and it is `rm -rf`'d.
//! 3. `git worktree prune`, but only if 1 or 2 actually removed something.
//!
//! # Why this one, and why it is the issue's own headline
//!
//! Step 2 is the single most dangerous predicate in `worktree.sh`. It is a
//! guard whose *false* answer runs `rm -rf` on a directory that may hold
//! another agent's uncommitted work, and it has already answered falsely on a
//! live worktree, twice over, for two independent reasons — both fixed in
//! #7858/#7849 and both re-checked here structurally rather than by review:
//!
//! - `git worktree list --porcelain` emits `worktree <path>`, and a path may
//!   contain **spaces**. The shell read field 2 of a whitespace split, which
//!   truncates at the first one. Here the path is
//!   `line.strip_prefix("worktree ")` — everything after the prefix, with
//!   nothing to word-split (the same read [`super::branch_delete`] already
//!   does for slice 3, and now literally the same function: see
//!   [`registered_worktrees`]).
//! - That porcelain emits symlink-**resolved** paths, so the candidate has to
//!   be resolved the same way before comparing. The shell needed an explicit
//!   `pwd -P`; here it is [`std::fs::canonicalize`], which has no logical
//!   variant to forget.
//!
//! Porting it also closes the exact duplication the issue body flags: the
//! shell's comment already said it "mirrors `branch_delete::worktree_entries`
//! in loom-daemon, which parses the same porcelain the same way". Two
//! implementations of *is this worktree safe to touch* become one.
//!
//! # Exit code
//!
//! **Always 0.** Argued, not defaulted. The shell function returns 0 on every
//! path (including its `git rev-parse` failure, which is an explicit early
//! `return 0`), and **both** of its call sites are written
//! `cleanup_partial_worktree_state "$ISSUE_NUMBER" || true` — the status has
//! never been an answer anyone branches on, so there is no code to reserve for
//! one. A non-zero code would be read by the `set -e` script above as a reason
//! to abandon a worktree it is about to create successfully.
//!
//! # …and why a MISSING binary is safe here, unlike slice 1
//!
//! This runs on `worktree.sh`'s **always-taken** path, which is exactly what
//! got slice 1's lock delegation reverted (#8226): a hard dependency there
//! moves every `worktree.sh` caller and ~30 shell suites onto a built
//! `loom-daemon`. So this delegation is best-effort — no binary means the
//! cleanup simply does not happen — and the reason that is *honest* rather
//! than merely convenient is the direction of the degradation:
//!
//! | debris left behind | what the caller sees instead |
//! |---|---|
//! | a stale `index.lock` | `git worktree add` fails with git's own lock error |
//! | an unregistered orphan dir | `worktree.sh` exits 1: *"Directory exists but is not a registered worktree"*, naming the `rm -rf` to run |
//!
//! Both are **loud, non-destructive refusals** — the pre-#3416 behaviour this
//! cleanup was added to spare an operator, not a guard whose absence lets
//! something dangerous through. The dangerous direction is the other one
//! (deleting a live worktree), and that is unreachable when the code does not
//! run at all. Contrast slice 2/3's verbs, where a silent skip could be
//! mistaken for a completed destructive operation and the stub therefore
//! exits 2.
//!
//! # Behaviour deliberately preserved from the shell
//!
//! - The lock-file warning prints whether or not the `rm -f` succeeded, and
//!   only the successful removal sets the "prune is warranted" flag — so a
//!   root-owned lock warns without provoking a prune.
//! - `git rev-parse --git-common-dir` may answer **relative** (`.git`, from
//!   the repo root), and the shell used that answer verbatim both to build
//!   `<git-common>/worktrees/...` and — via `dirname` — the repo root. The
//!   relative form is preserved, including in the message text, and resolved
//!   against the *logical* cwd the way bash's own `pwd` would (see
//!   [`logical_cwd`]).
//! - `git worktree list --porcelain` and `git worktree prune` run in the
//!   process's cwd, with no `-C`, exactly as the shell invoked them.
//! - An orphan path that is itself a **symlink** is unlinked, not followed:
//!   `rm -rf` removes the link. Following it would delete a directory outside
//!   the worktree root entirely — the worst outcome available to this
//!   function. [`remove_path`] spells that out rather than relying on
//!   [`std::fs::remove_dir_all`]'s own handling; see its doc comment for why
//!   an equivalence that currently holds is not the thing to depend on here.
//!
//! # The one strengthening
//!
//! The issue number is parsed as a `u64` instead of interpolated as a string.
//! `worktree.sh` validates `^[0-9]+$` before it ever reaches this function, so
//! no live call is affected; what it removes is the unreachable-but-real
//! shape where a `..`-bearing token walks `<worktree-root>/issue-<tok>` out of
//! the worktree root and hands an arbitrary directory to the `rm -rf`. A guard
//! against `rm -rf` outside its own tree should not depend on a check in a
//! different file.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::wip::Out;

/// The per-worktree lock files git holds for the duration of an add, in the
/// shell's order (observable: it is the order the warnings print in).
const STALE_LOCKS: [&str; 3] = ["index.lock", "HEAD.lock", "gitdir.lock"];

/// What to clean, and how loudly.
pub struct Options {
    /// The issue whose `issue-<N>` debris is being cleared.
    pub issue: u64,
    /// Print nothing at all.
    ///
    /// Mirrors the shell's `if [[ "$JSON_OUTPUT" != "true" ]]` guard around
    /// each of these warnings — NOT [`Out`]'s stderr routing. In `--json` mode
    /// `worktree.sh` has already pointed fd 1 at stderr, so routing rather
    /// than suppressing would still be invisible on stdout while adding lines
    /// to stderr the pre-port script never emitted.
    pub quiet: bool,
}

/// Run the cleanup. Returns the process exit code — always 0, see the module
/// docs.
pub fn run(opts: &Options) -> i32 {
    let out = Reporter {
        out: Out::new(false),
        quiet: opts.quiet,
    };

    // `git_common=$(git rev-parse --git-common-dir) || return 0` — not a
    // repo, or no git at all: nothing to clean and nothing to say.
    let Some(git_common) = git_common_dir() else {
        return 0;
    };

    let admin_dir = git_common
        .join("worktrees")
        .join(format!("issue-{}", opts.issue));

    let mut cleaned = remove_stale_locks(&admin_dir, &out);
    cleaned |= remove_orphan_dir(&git_common, opts.issue, &out);

    if cleaned {
        // `git worktree prune 2>/dev/null || true` — now that the orphan
        // administrative state is locally consistent.
        let _ = Command::new("git")
            .args(["worktree", "prune"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    0
}

// ---------------------------------------------------------------------------
// 1. Per-worktree file locks
// ---------------------------------------------------------------------------

/// Returns whether anything was actually removed.
fn remove_stale_locks(admin_dir: &Path, out: &Reporter) -> bool {
    let mut cleaned = false;
    for lock in STALE_LOCKS {
        let path = admin_dir.join(lock);
        // `[[ -f ]]`: follows symlinks, regular files only.
        if !path.is_file() {
            continue;
        }
        // The shell warns whether or not the `rm -f` succeeded, and only the
        // success sets `cleaned`. Preserved: a lock this process cannot remove
        // is still worth reporting, and still not a reason to prune.
        if std::fs::remove_file(&path).is_ok() {
            cleaned = true;
        }
        out.warning(&format!("Cleaned stale {lock} at {}", path.to_string_lossy()));
    }
    cleaned
}

// ---------------------------------------------------------------------------
// 2. The orphan guard
// ---------------------------------------------------------------------------

/// Returns whether the orphan dir was actually removed.
fn remove_orphan_dir(git_common: &Path, issue: u64, out: &Reporter) -> bool {
    // `repo_root=$(cd "$(dirname "$git_common")" && pwd) || repo_root="$(pwd)"`
    // — the parent of the git common dir, which is the main workspace whether
    // or not cwd is currently in it.
    let repo_root = match git_common.parent() {
        Some(parent) => absolutize(parent),
        None => logical_cwd(),
    };
    let wt_path = crate::worktree_root::worktree_root(&repo_root).join(format!("issue-{issue}"));

    // `[[ -d "$wt_path" ]]` — follows symlinks.
    if !wt_path.is_dir() {
        return false;
    }

    if is_registered(&wt_path) {
        return false;
    }

    out.warning(&format!(
        "Removing orphan worktree dir (not registered with git): {}",
        wt_path.to_string_lossy()
    ));
    remove_path(&wt_path)
}

/// Is `wt_path` a worktree `git` knows about?
///
/// The whole point of this function: answering `false` for a LIVE worktree is
/// the #7849 data-loss bug. Both halves of that bug are structural here —
/// the candidate is resolved through [`std::fs::canonicalize`] (the porcelain
/// reports symlink-resolved paths) and the porcelain path is read whole rather
/// than word-split (a path may contain spaces).
///
/// An **unresolvable** candidate answers `false`, matching the shell's
/// `abs_wt=""` branch: a directory whose own path cannot be canonicalized
/// cannot be the one git reported. `is_dir()` has already established it
/// exists, so this is a permissions/race edge, not the common case.
fn is_registered(wt_path: &Path) -> bool {
    let Ok(abs_wt) = std::fs::canonicalize(wt_path) else {
        return false;
    };
    registered_worktrees()
        .into_iter()
        .any(|entry| entry == abs_wt)
}

/// Every path `git worktree list --porcelain` reports, read in the process's
/// cwd exactly as the shell invoked it (no `-C`).
///
/// Delegates the parse to [`super::branch_delete::parse_worktree_porcelain`]
/// — the same reader slice 3 already uses for the `remove` verb. The shell's
/// own comment said this parse "mirrors `branch_delete::worktree_entries`";
/// mirroring is what drifts, so now there is one function.
fn registered_worktrees() -> Vec<PathBuf> {
    let Ok(out) = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    super::branch_delete::parse_worktree_porcelain(&String::from_utf8_lossy(&out.stdout))
        .into_iter()
        .map(|(path, _)| path)
        .collect()
}

/// `rm -rf "$wt_path"` — including the distinction that matters most here: a
/// **symlink** to a directory is unlinked, never followed.
///
/// Today's [`std::fs::remove_dir_all`] already unlinks a top-level symlink
/// rather than recursing through it, so the explicit branch below is currently
/// redundant — measured, not assumed: the differential harness was run with
/// this branch deleted and stayed green. It is kept anyway, and this is the
/// argument for keeping it rather than trimming it as dead weight:
///
/// - The property is "never delete a tree outside the worktree root", which is
///   a *worse* outcome than the #7849 bug this function exists to prevent, and
///   it is reached from the same call. A property at that stake should be
///   written down where the call is, not inferred from a library's current
///   implementation of a different guarantee (`remove_dir_all` documents
///   removing a *directory* and its contents; the symlink case is its
///   behaviour, not its contract).
/// - Because the two are equivalent today, **no test can hold the line here** —
///   a mutation that deletes this branch is green by construction. A comment
///   is the only mechanism left, so it says so explicitly rather than leaving
///   a future reader to rediscover it by deleting the branch and seeing green.
fn remove_path(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::remove_file(path).is_ok(),
        Ok(_) => std::fs::remove_dir_all(path).is_ok(),
        // Vanished between the `is_dir()` check and here: `rm -rf` is silent
        // about a missing path and removes nothing, so neither warrants a
        // prune.
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// `git rev-parse --git-common-dir`, verbatim — **including** the relative
/// `.git` it answers from a repo root, which the shell then used as-is.
fn git_common_dir() -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let trimmed = text.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

/// The cwd as **bash** would report it from `pwd` — `$PWD` when it is a valid
/// alias for the real cwd, else `getcwd()`.
///
/// This matters because `worktree.sh` `cd`s into the main workspace before
/// calling here, and a fleet host reaches its checkout through a symlink
/// (`/tmp` → `/private/tmp` on macOS is the shape the retained suite pins).
/// Bash keeps the logical path; `getcwd()` returns the physical one. The
/// *decision* this function feeds is unaffected — the candidate is
/// canonicalized before it is compared either way — but the path printed in
/// the "Removing orphan worktree dir" warning is the one an operator reads
/// back to themselves, and it should be the path they typed.
fn logical_cwd() -> PathBuf {
    let physical = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let Some(pwd) = std::env::var_os("PWD") else {
        return physical;
    };
    let logical = PathBuf::from(pwd);
    if !logical.is_absolute() {
        return physical;
    }
    match (std::fs::canonicalize(&logical), std::fs::canonicalize(&physical)) {
        (Ok(a), Ok(b)) if a == b => logical,
        _ => physical,
    }
}

/// `cd "$dir" && pwd` — make `dir` absolute against the logical cwd without
/// resolving symlinks, and normalise the `.`/`` components a relative
/// `--git-common-dir` answer introduces (`dirname ".git"` is `"."`).
///
/// Deliberately **not** [`std::fs::canonicalize`]: see [`logical_cwd`].
fn absolutize(dir: &Path) -> PathBuf {
    let joined = if dir.as_os_str().is_empty() {
        logical_cwd()
    } else if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        logical_cwd().join(dir)
    };

    // Lexical normalisation only — `..` is left alone precisely because
    // resolving it lexically is wrong in the presence of symlinks, and bash's
    // `cd` does not resolve it lexically either.
    let mut normalised = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::CurDir => {}
            other => normalised.push(other),
        }
    }
    if normalised.as_os_str().is_empty() {
        logical_cwd()
    } else {
        normalised
    }
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// [`Out`] plus the shell's all-or-nothing `--json` suppression. Same shape as
/// [`super::link`]'s reporter, for the same reason.
struct Reporter {
    out: Out,
    quiet: bool,
}

impl Reporter {
    fn warning(&self, msg: &str) {
        if !self.quiet {
            self.out.warning(msg);
        }
    }
}

#[cfg(test)]
mod tests;
