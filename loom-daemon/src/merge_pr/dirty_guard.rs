//! The post-merge worktree-cleanup data-loss guard (#5031, classified by
//! #5658) — a slice of the `merge-pr.sh` port, #8191.
//!
//! # What it protects
//!
//! `merge-pr.sh`'s post-merge cleanup keys the worktree to remove ONLY by
//! branch name (`feature/issue-<N>`). `worktree.sh`'s naming convention makes
//! that name collide deterministically whenever two hosts independently claim
//! the same issue number, so the `git worktree remove --force` at the end of a
//! merge can land on a *different*, still-live builder's worktree. On
//! 2026-08-03 it did: a live `/loom:sweep 5001` session was mid-edit in
//! `.loom/worktrees/issue-5001` with nothing committed when a separate host's
//! PR #5026 merged, and the cleanup destroyed those edits unrecoverably.
//!
//! This guard runs immediately before that `--force` and refuses the removal
//! when the worktree still holds uncommitted work. The merge itself has already
//! succeeded by then, so a skipped cleanup costs nothing and is recoverable
//! later (`loom-clean`, the daemon's reaper, or the next merge); the work it
//! saves is not recoverable at all. That asymmetry decides every judgement
//! call in this module — including [`Verdict::CouldNotRun`]'s direction at the
//! wrapper.
//!
//! # The three decisions, and where each one used to live
//!
//! 1. **Which porcelain lines represent user work.** Loom writes its own
//!    untracked breadcrumbs into every managed worktree; if those counted as
//!    work the guard would refuse to remove *every* managed worktree, which is
//!    worse than no guard. The retired shell re-encoded the marker list as a
//!    `grep -vE` alternation. This port asks
//!    [`safety::is_loom_own_untracked_path`] instead — the same predicate
//!    `worktree.sh remove` was ported onto by #8195 slice 3 — so merge-pr.sh
//!    stops being the third place that list is written down. #8279 is what
//!    happened the last time two places listed the markers separately; this
//!    file was the copy that survived that fix.
//!
//! 2. **Whether the remaining dirt plausibly IS in-flight work** (#5658).
//!    Lockfile-shaped churn from a routine install is common and does not imply
//!    a live sibling session, so asserting the cross-host-dispatch hypothesis
//!    there sends an operator hunting a phantom. Any tracked source change or
//!    untracked non-artifact file still offers it, even alongside trivial dirt:
//!    mixed means real work wins.
//!
//! 3. **What to say.** The refusal text is reproduced here byte-for-byte and
//!    replayed by the shell through its own `warning`/`echo`, because
//!    `test-merge-pr-dirty-worktree-guard.sh` asserts on that text and an
//!    operator greps for it.
//!
//! The porcelain READ itself deliberately stays in the shell and arrives on
//! stdin — the same split [`super::hold_state`] took with `forge_get_pr_comments`.
//! It is not squeamishness: `git status` failing (the #5177 orphaned-directory
//! case the removal path exists to clean up) has always meant "no dirt, proceed",
//! and keeping the read where that `|| true` already lives is how this slice
//! avoids turning an orphaned directory into a permanent refusal.
//!
//! # Two divergences from the retired `grep -vE`, both deliberate
//!
//! The retired filter was a whole-LINE regex:
//!
//! ```text
//! grep -vE '[ /]\.loom-managed$|[ /]\.loom-in-use$|[ /]\.loom-checkpoint$|[ /]\.no-changes-needed$|[ /]\.snapshots/'
//! ```
//!
//! **A. A rename INTO a marker name was filtered as bookkeeping.** Porcelain
//! renders a rename as `R  old -> new`, so `R  src/app.rs -> .loom-managed`
//! ends in ` .loom-managed` and the `$`-anchored alternation dropped it. A
//! rename is a tracked-file change — real work — and if it was the only dirt,
//! the guard saw a clean worktree and force-removed it. This port extracts the
//! path field and asks the marker predicate about it *without* splitting the
//! rename arrow, so `old -> .loom-managed` is not any marker's name and the
//! line survives as dirt. This is the one divergence that changes a REFUSE/
//! REMOVE outcome, and it moves toward refusing — the direction the guard
//! exists for.
//!
//! **B. A marker under a quoted path is now recognised.** `git status` quotes a
//! path containing non-ASCII or control bytes, so a marker inside such a
//! directory arrived as `?? "sub\303\251/.loom-managed"` — the trailing quote
//! defeated the `$` anchor and Loom's own breadcrumb counted as user work. The
//! port strips surrounding quotes before testing, matching
//! `worktree_cli::remove::dirty_lines`. This one moves toward removing, but
//! only for a file that is unambiguously Loom's own bookkeeping; it does not
//! widen what counts as "not work".
//!
//! Everywhere else the two must agree, which is what
//! `tests/merge_pr_dirty_guard_differential.rs` pins against a frozen copy of
//! the retired pipeline.
//!
//! # A divergence deliberately NOT taken
//!
//! `worktree_cli::remove::dirty_lines` reads porcelain with
//! `--untracked-files=all` and does **not** filter the `.snapshots/` WIP
//! directory; merge-pr.sh reads it without `-uall` and does filter
//! `.snapshots/`. Both differences are preserved exactly as they were. Making
//! `.snapshots` part of [`safety::is_loom_own_untracked_path`] would look
//! tidier, but that predicate also gates `stash-push --include-untracked`
//! (which MOVES every unfiltered file out of the worktree) and the daemon's
//! `pr-<N>` reclaim classifier, so widening it is a behaviour change to two
//! paths this slice does not touch. The asymmetry is recorded here instead of
//! being quietly unified.

use crate::worktree_ops::safety;

/// The only stdout a caller may treat as "no user work here, removal may
/// proceed".
///
/// A positive sentinel rather than silence, matching [`super::labels::CLEAN`],
/// [`super::loom_pr_guard::CLEAN`] and [`super::hold_state::CLEAN`]. Here it
/// carries real weight: the caller's next action is an irreversible
/// `--force` removal, and "printed nothing because the verb does not exist" must
/// never read as "checked, nothing to save".
pub const CLEAN: &str = "LOOM-DIRTY-GUARD-CLEAN";

/// How a replayed line is to be printed by the shell wrapper.
///
/// `merge-pr.sh` interleaves this guard's output with dozens of other messages
/// through its own `warning`/`echo`, and its retained suite stubs those names,
/// so the protocol names the shell function rather than a color — the same
/// choice `merge-pr delete-branch` made (#8191). `Plain` exists because the
/// retired code ended with a bare, uncolored `echo` of the remediation command,
/// and an operator copy-pastes that line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Replay through the shell's `warning` (yellow).
    Warning,
    /// Replay through a bare `echo` — no color, so it pastes cleanly.
    Plain,
}

impl Level {
    /// The protocol token, as the wrapper's `case` reads it.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Level::Warning => "WARNING",
            Level::Plain => "PLAIN",
        }
    }
}

/// Is this porcelain line one of Loom's own runtime breadcrumbs rather than
/// user work?
///
/// Operates on the path field (`XY<space>` then the path, porcelain v1), with
/// surrounding quotes stripped — divergence B in the module docs. The rename
/// arrow is deliberately NOT split: divergence A.
///
/// A line too short or too malformed to have a path field reports `false`, so
/// it is kept as dirt. That is the conservative direction here — an
/// unclassifiable line stops a `--force`, it never authorizes one.
#[must_use]
pub fn is_loom_runtime_marker_line(line: &str) -> bool {
    let path = path_field(line).trim_matches('"');
    if safety::is_loom_own_untracked_path(path) {
        return true;
    }
    in_snapshots_dir(path)
}

/// Does `path` sit inside a `.snapshots/` directory?
///
/// The retired regex was `[ /]\.snapshots/`: the component must be named
/// exactly `.snapshots` and must be followed by a `/`, so a *file* called
/// `.snapshots` is user work while `.snapshots/issue-1-…​.patch` and the
/// collapsed untracked-directory line `.snapshots/` are not. `a.snapshots/x`
/// matched neither then nor now.
fn in_snapshots_dir(path: &str) -> bool {
    let mut components = path.split('/').peekable();
    while let Some(component) = components.next() {
        // The final component has no `/` after it, so it cannot be the
        // directory the retired regex required.
        if components.peek().is_none() {
            return false;
        }
        if component == ".snapshots" {
            return true;
        }
    }
    false
}

/// Porcelain v1's path field: two status characters, one space, then the path.
///
/// Byte-indexed, matching `${line:3}` under the `C` locale the differential
/// pins. A byte 3 that splits a multi-byte character yields nothing — git never
/// emits such a line (the first three bytes are always two ASCII status
/// characters and a space), and both callers treat an empty field
/// conservatively: not a marker, and not artifact churn.
fn path_field(line: &str) -> &str {
    line.get(3..).unwrap_or("")
}

/// Is this line whitespace-only, as `grep -vE '^[[:space:]]*$'` meant it?
///
/// The six characters POSIX `[[:space:]]` covers in the `C` locale, spelled
/// out rather than delegated to `char::is_whitespace` — that is Unicode-wide
/// (it includes U+00A0 and friends), which would silently drop a line the
/// retired filter kept. PR #8199 shipped exactly this class of `[[:space:]]`
/// mistranslation in an earlier slice; naming the set is how it stays fixed.
fn is_blank(line: &str) -> bool {
    line.chars()
        .all(|c| matches!(c, ' ' | '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r'))
}

/// The porcelain lines that represent USER work: neither blank nor one of
/// Loom's own runtime breadcrumbs.
///
/// Order is preserved — the refusal quotes these lines verbatim so the operator
/// sees them exactly as `git status` reported them.
#[must_use]
pub fn user_dirt(porcelain: &str) -> Vec<&str> {
    porcelain
        .lines()
        .filter(|line| !is_blank(line))
        .filter(|line| !is_loom_runtime_marker_line(line))
        .collect()
}

/// What one dirty line plausibly represents (#5658).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Churn {
    /// A tracked source change, or an untracked non-artifact file: the
    /// cross-host-dispatch hypothesis is worth offering.
    RealWork,
    /// Lockfile-shaped output a routine install regenerates. Still refuses the
    /// removal — it is still uncommitted state — but without asserting a
    /// concurrent session that probably does not exist.
    Artifact,
}

/// Classify one dirty porcelain line.
///
/// A rename is judged by its DESTINATION (`${dirty_path##* -> }`, i.e. the text
/// after the LAST arrow), which is what the retired loop did — and, unlike the
/// marker filter above, the retired loop was right to: what the file became is
/// what is now sitting uncommitted on disk.
///
/// Quotes are NOT stripped here, deliberately. The retired loop did not strip
/// them either, and the only outcome this affects is which of two advisory
/// sentences prints: a quoted path is a non-ASCII filename, which is not what
/// a package manager regenerates, so treating it as real work is both faithful
/// and the conservative reading.
#[must_use]
pub fn classify(line: &str) -> Churn {
    let path = path_field(line);
    let path = path.rsplit(" -> ").next().unwrap_or(path);
    if path.ends_with(".lock") || path.ends_with("-lock.json") {
        Churn::Artifact
    } else {
        Churn::RealWork
    }
}

/// Does this dirty set contain anything that looks like genuine in-flight work?
///
/// Mixed sets answer `true`: a real edit alongside a regenerated lockfile is
/// still a live sibling's work, and #5658's narrowing was only ever meant to
/// stop the hypothesis being asserted when NOTHING in the set supports it.
#[must_use]
pub fn has_real_work(dirt: &[&str]) -> bool {
    dirt.iter().any(|line| classify(line) == Churn::RealWork)
}

/// Everything the guard needs that does not come from `git status`.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// The worktree being considered for removal (`$worktree_path`).
    pub worktree_path: &'a str,
    /// The repository root, for the remediation command (`$REPO_ROOT`).
    pub repo_root: &'a str,
    /// The branch checked out in that worktree, when it resolved. Empty when
    /// detached, bare, or unresolvable — the refusal then simply omits the
    /// `on branch '…'` clause, exactly as `${live_branch:+…}` did.
    pub branch: &'a str,
}

/// The refusal, as `(level, message)` records in print order — or `None` when
/// there is no user work and the removal may proceed.
///
/// Reproduces the retired shell's five outputs in the retired order. It is a
/// record list rather than one blob because the wrapper replays each through
/// the shell's own logging, and because the dirty lines are quoted verbatim:
/// a line of git output must not be able to change how the lines around it are
/// parsed.
#[must_use]
pub fn assess(ctx: &Context<'_>, porcelain: &str) -> Option<Vec<(Level, String)>> {
    let dirt = user_dirt(porcelain);
    if dirt.is_empty() {
        return None;
    }

    let mut out = Vec::with_capacity(dirt.len() + 4);
    let on_branch = if ctx.branch.is_empty() {
        String::new()
    } else {
        format!(" on branch '{}'", ctx.branch)
    };
    out.push((
        Level::Warning,
        format!(
            "Refusing to remove worktree at {} — it has uncommitted changes{on_branch} \
(data-loss guard, #5031):",
            ctx.worktree_path
        ),
    ));
    for line in &dirt {
        out.push((Level::Warning, (*line).to_string()));
    }
    out.push((
        Level::Warning,
        if has_real_work(&dirt) {
            "A different, still-live builder session likely shares this branch name (cross-host \
duplicate dispatch). Leaving it in place so that work is not lost."
                .to_string()
        } else {
            "The dirt above looks like environment/artifact churn (e.g. a regenerated lockfile), \
not concurrent work — verify and remove manually."
                .to_string()
        },
    ));
    out.push((
        Level::Warning,
        "Remove it manually once those changes are saved/committed:".to_string(),
    ));
    out.push((
        Level::Plain,
        format!(
            "  git -C \"{}\" worktree remove \"{}\" --force",
            ctx.repo_root, ctx.worktree_path
        ),
    ));
    Some(out)
}

#[cfg(test)]
mod tests;
