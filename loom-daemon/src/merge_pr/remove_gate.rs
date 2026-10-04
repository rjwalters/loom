//! The identity/ownership gate in front of `merge-pr.sh`'s
//! `_remove_loom_worktree` (#8191 slice): the #3710 primary-worktree hard
//! guard, then the `.loom-managed` sentinel guard with its `--worktree-path`
//! opt-in bypass.
//!
//! # What it decides
//!
//! Given the `git worktree list --porcelain` text, the target path, the
//! target's canonical path, whether the operator opted in explicitly
//! (`--worktree-path`), and whether `<path>/.loom-managed` exists, answer
//! PROCEED or REFUSE, plus the operator-visible message records:
//!
//! 1. **Primary guard (#3710).** The FIRST `worktree ` record is the main
//!    checkout, which is never removable regardless of sentinel, branch, or a
//!    customized `worktree.root`. An input with no record at all is "git
//!    reported no worktrees" — the same "not primary" the retired
//!    `_primary_worktree_path` answer meant. A *failed invocation* is a
//!    different thing and is REFUSE on the shell side (see below).
//! 2. **Sentinel guard.** Without `--worktree-path` an unmarked worktree is
//!    user-owned and refused. With it, the missing sentinel is bypassed and
//!    the bypass is announced.
//!
//! The comparison is exact string equality of the primary path against the
//! caller-supplied canonical path — as the shell did — so the canonicalisation
//! (`cd … && pwd -P`, which the CWD-inside-worktree check reuses) stays where
//! it already is.
//!
//! # Fail direction
//!
//! A pure function: no failure mode of its own. The shell wrapper treats any
//! non-zero exit, or a first line that is neither verdict token (a missing or
//! older binary), as REFUSE — a gate that did not run must not authorise a
//! `git worktree remove --force`; a skipped cleanup is always recoverable.

use super::worktree_preserve::Level;
use super::worktrees;

/// The gate's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Continue to the dirty-work guard and the removal.
    Proceed,
    /// Leave the worktree in place.
    Refuse,
}

impl Verdict {
    /// The protocol token the shell wrapper matches on its first line.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Verdict::Proceed => "LOOM-REMOVE-GATE PROCEED",
            Verdict::Refuse => "LOOM-REMOVE-GATE REFUSE",
        }
    }
}

/// Every fact the gate needs.
pub struct Context<'a> {
    /// `git worktree list --porcelain`, as the shell captured it.
    pub porcelain: &'a str,
    /// The target path as passed to `_remove_loom_worktree`.
    pub path: &'a str,
    /// The target's canonical path (`pwd -P`, or `path` if unresolvable).
    pub real: &'a str,
    /// `--worktree-path` explicit opt-in (`allow_unmanaged`).
    pub allow_unmanaged: bool,
    /// Whether `<path>/.loom-managed` is a regular file.
    pub sentinel_present: bool,
}

/// Decide, returning the verdict and the message records to replay.
#[must_use]
pub fn decide(ctx: &Context<'_>) -> (Verdict, Vec<(Level, String)>) {
    if worktrees::primary_path(ctx.porcelain).is_some_and(|p| !p.is_empty() && p == ctx.real) {
        return (
            Verdict::Refuse,
            vec![(
                Level::Warning,
                format!(
                    "Refusing to remove the primary/main worktree at {} (never removable regardless of .loom-managed sentinel, branch, or worktree.root)",
                    ctx.real
                ),
            )],
        );
    }
    if ctx.sentinel_present {
        return (Verdict::Proceed, Vec::new());
    }
    if ctx.allow_unmanaged {
        return (
            Verdict::Proceed,
            vec![(
                Level::Info,
                format!(
                    "Bypassing sentinel guard (--worktree-path explicit opt-in for {})",
                    ctx.path
                ),
            )],
        );
    }
    (
        Verdict::Refuse,
        vec![(
            Level::Warning,
            format!(
                "Worktree at {} lacks .loom-managed sentinel — refusing to remove (user-owned)",
                ctx.path
            ),
        )],
    )
}

/// Render as the wrapper's protocol: the verdict token, then one
/// `LEVEL<TAB>message` line per record.
#[must_use]
pub fn render(verdict: Verdict, lines: &[(Level, String)]) -> String {
    let mut out = format!("{}\n", verdict.token());
    for (level, message) in lines {
        out.push_str(level.token());
        out.push('\t');
        out.push_str(message);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests;
