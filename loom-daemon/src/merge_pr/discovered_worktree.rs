//! What `merge-pr.sh`'s post-merge cleanup does with a worktree it DISCOVERED
//! by branch name, because the Loom-convention path was missing (#8191 slice;
//! the `if _is_primary_worktree_path … elif [[ -f …/.loom-managed ]] … else`
//! block of the discovery fallback before this port).
//!
//! # What it decides
//!
//! Three-way classification, in this order:
//!
//! 1. **The primary checkout (#4171).** The PR branch is checked out in the
//!    main working copy, which is not a removable worktree at all: say so, and
//!    never suggest `git worktree remove` / `--worktree-path`, neither of which
//!    can apply.
//! 2. **A `.loom-managed` worktree at a non-standard path.** Safe to remove in
//!    principle: the verdict is [`Action::Decide`], and the shell hands it to
//!    the shared remove-vs-preserve decision (`worktree-preserve`, #4186/#6694).
//! 3. **A user-owned worktree.** Never auto-removed (the #3334 ownership
//!    model); four warnings surface the path and the two ways to clean it up.
//!
//! # What stays in the shell
//!
//! `git worktree list` and the primary-path comparison (`--primary`), and the
//! removal itself. The sentinel is read here, as the shell's `[[ -f ]]` did.
//!
//! # Fail direction
//!
//! A pure function. The shell treats a missing/older binary, a non-zero exit
//! or an unrecognised first line as "no verdict", which removes NOTHING:
//! `Decide` is the only answer that can lead to a removal, and it must be
//! positively received.

use super::worktree_preserve::Level;

/// What the shell should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Run the remove-vs-preserve decision on the discovered worktree.
    Decide,
    /// Only replay the records; remove nothing.
    Note,
}

impl Action {
    /// The protocol token on the first line.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Action::Decide => "LOOM-DISCOVERED DECIDE",
            Action::Note => "LOOM-DISCOVERED NOTE",
        }
    }
}

/// Classify a discovered worktree and word the operator-visible records.
#[must_use]
pub fn decide(
    branch: &str,
    path: &str,
    primary: bool,
    sentinel_present: bool,
) -> (Action, Vec<(Level, String)>) {
    if primary {
        return (
            Action::Note,
            vec![(
                Level::Info,
                format!(
                    "PR branch '{branch}' is checked out in the primary repository checkout ({path}) — not a removable worktree."
                ),
            )],
        );
    }
    if sentinel_present {
        return (Action::Decide, Vec::new());
    }
    let w = |m: String| (Level::Warning, m);
    (
        Action::Note,
        vec![
            w(format!("Discovered worktree for branch '{branch}' at: {path}")),
            w("Worktree lacks .loom-managed sentinel — not removing (user-owned).".to_string()),
            w(format!("To clean it up, re-run with: --worktree-path '{path}'")),
            w(format!("Or manually: git worktree remove '{path}'")),
        ],
    )
}

/// Render as the wrapper's protocol: the action token, then one
/// `LEVEL<TAB>message` line per record.
#[must_use]
pub fn render(action: Action, lines: &[(Level, String)]) -> String {
    let mut out = format!("{}\n", action.token());
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
