//! The removal itself at the end of `merge-pr.sh`'s `_remove_loom_worktree`
//! (#6372, an #8191 slice): `git worktree remove --force`, then — on failure
//! only — one `git worktree prune` and one retry, and the operator-facing
//! report of how that went.
//!
//! # Why this, of all the cleanup steps
//!
//! Every guard in front of it is already Rust: the #3710 primary-worktree
//! check and the `.loom-managed` sentinel (`remove-gate`), the #5031 dirty-work
//! refusal ([`super::dirty_guard`]), the #6694/#6264 remove-vs-preserve rule
//! ([`super::worktree_preserve`]). What stayed in the shell was the one step
//! those guards exist to protect — the irreversible `--force` — inside an
//! `if`/`elif`/`if` ladder whose meaning rides on which of two assignments to
//! `remove_err` ran last. This module makes that sequence explicit
//! ([`run_with`]) and owns the four-line failure diagnosis #6372 added.
//!
//! # What moved, and what did not
//!
//! Moved: the three `git` invocations, their ordering, and the two success /
//! four-warning message texts. Stayed in the shell: everything that is not the
//! removal — the `cargo-target-dir resolve` that must run first, the
//! `Removing worktree:` announcement, and on success the removal ledger
//! (`loom_record_worktree_removal`, a sourced shell library), the
//! "your shell was inside it" hint, the `--worktree-path` branch delete and the
//! `cargo-target-dir reclaim`. Those read this verb's verdict line.
//!
//! # Fidelity to the retired shell
//!
//! Each `git` call's stdout and stderr share ONE pipe, as `$(… 2>&1)` did, so
//! git's own interleaving survives; every trailing newline is stripped, as
//! command substitution does. `prune`'s output is discarded (`>/dev/null
//! 2>&1`). The failure report's `remove_err` is whichever `remove` ran last.
//!
//! One rendering difference, the same one `merge-pr delete-branch`'s replay
//! (`cli::merge_pr_delete_branch`) already made for the same reason: a MULTI-LINE git
//! error (a locked worktree's refusal is two lines) is emitted as one
//! `WARNING` record per line, because the shell replays records line by line.
//! The visible text is unchanged; each line gets its own colour codes rather
//! than one pair spanning them.
//!
//! # Fail direction
//!
//! The verb always exits 0 with a [`REMOVED`] or [`FAILED`] verdict, so a
//! non-zero exit or any other first line can only mean "did not run" (a
//! missing or older binary). The shell treats that as FAILED: it warns and
//! removes nothing — the same direction as every guard in front of it. A
//! skipped removal is always recoverable (`loom-clean`, the daemon's reaper,
//! `worktree.sh remove`); the merge itself has already succeeded.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

/// First stdout line: the worktree is gone.
pub const REMOVED: &str = "LOOM-WORKTREE-TEARDOWN REMOVED";
/// First stdout line: both attempts failed (or the prune did); it is still there.
pub const FAILED: &str = "LOOM-WORKTREE-TEARDOWN FAILED";

/// How a record is replayed by the shell wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Through the shell's `success`.
    Success,
    /// Through the shell's `warning`.
    Warning,
}

impl Level {
    /// The protocol token, as the wrapper's `case` reads it.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Level::Success => "SUCCESS",
            Level::Warning => "WARNING",
        }
    }
}

/// One `git worktree remove --force` attempt: whether it exited 0, and its
/// combined stdout+stderr with trailing newlines stripped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    /// The command exited 0.
    pub ok: bool,
    /// What `$(… 2>&1)` would have captured.
    pub output: String,
}

/// What the remove / prune / retry sequence came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The first attempt removed it.
    Removed,
    /// The first attempt failed, `prune` succeeded, and the retry removed it.
    RemovedAfterPrune,
    /// Not removed. `error` is the last attempt's captured output — the
    /// retry's when the prune succeeded, the first attempt's when it did not.
    Failed {
        /// The text the retired shell held in `remove_err`.
        error: String,
    },
}

/// The retired ladder, as a function of its two effects:
///
/// ```text
/// if remove_err="$(remove)"; then removed=true
/// elif prune; then pruned=true
///   if remove_err="$(remove)"; then removed=true; fi
/// fi
/// ```
///
/// `prune` runs only after a failed first attempt, and the retry only after a
/// successful prune — at most three `git` calls, never a second prune.
pub fn run_with<R, P>(mut remove: R, mut prune: P) -> Outcome
where
    R: FnMut() -> Attempt,
    P: FnMut() -> bool,
{
    let first = remove();
    if first.ok {
        return Outcome::Removed;
    }
    if !prune() {
        return Outcome::Failed {
            error: first.output,
        };
    }
    let second = remove();
    if second.ok {
        Outcome::RemovedAfterPrune
    } else {
        Outcome::Failed {
            error: second.output,
        }
    }
}

/// The verdict line plus the records to replay, in order.
#[must_use]
pub fn report(
    outcome: &Outcome,
    repo_root: &str,
    path: &str,
) -> (&'static str, Vec<(Level, String)>) {
    match outcome {
        Outcome::Removed => (REMOVED, vec![(Level::Success, "Worktree removed".to_string())]),
        Outcome::RemovedAfterPrune => (
            REMOVED,
            vec![(
                Level::Success,
                "Worktree removed (after pruning a stale worktree registration)".to_string(),
            )],
        ),
        Outcome::Failed { error } => {
            let mut lines = vec![(
                Level::Warning,
                format!(
                    "Could not remove worktree at {path} (best-effort cleanup — the merge itself already succeeded and is unaffected):"
                ),
            )];
            // `split`, not `lines`: a `\r` is git's byte, not a line ending,
            // and an EMPTY error still produced one (empty) `warning` call.
            lines.extend(error.split('\n').map(|l| (Level::Warning, l.to_string())));
            lines.push((
                Level::Warning,
                format!(
                    "Remediation: git worktree prune && git -C \"{repo_root}\" worktree remove \"{path}\" --force"
                ),
            ));
            lines.push((
                Level::Warning,
                format!(
                    "If that still fails: rm -rf \"{path}\" && git -C \"{repo_root}\" worktree prune"
                ),
            ));
            (FAILED, lines)
        }
    }
}

/// Render the wrapper's protocol: the verdict line, then one
/// `LEVEL<TAB>message` line per record.
#[must_use]
pub fn render(verdict: &str, lines: &[(Level, String)]) -> String {
    let mut out = format!("{verdict}\n");
    for (level, message) in lines {
        out.push_str(level.token());
        out.push('\t');
        out.push_str(message);
        out.push('\n');
    }
    out
}

/// `git -C <repo_root> worktree remove <path> --force` with stdout and stderr
/// on one pipe. A `git` that cannot be spawned at all is a failed attempt
/// carrying the spawn error, as bash's `command not found` would have been.
fn remove_once(repo_root: &Path, path: &str) -> Attempt {
    let failed = |output: String| Attempt { ok: false, output };
    let Ok((mut reader, writer)) = std::io::pipe() else {
        return failed("could not create a pipe for git".to_string());
    };
    let Ok(writer_err) = writer.try_clone() else {
        return failed("could not create a pipe for git".to_string());
    };
    let spawned = {
        // Scoped so the Command, which holds both write ends, is dropped
        // before the read below; otherwise the read never sees EOF.
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(repo_root)
            .args(["worktree", "remove", path, "--force"])
            .stdin(Stdio::null())
            .stdout(writer)
            .stderr(writer_err);
        cmd.spawn()
    };
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return failed(format!("git: {e}")),
    };
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf);
    let ok = child.wait().is_ok_and(|s| s.success());
    let output = String::from_utf8_lossy(&buf)
        .trim_end_matches('\n')
        .to_string();
    Attempt { ok, output }
}

/// `git -C <repo_root> worktree prune >/dev/null 2>&1`.
fn prune(repo_root: &Path) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["worktree", "prune"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Run the sequence against a real repository and return the protocol text.
#[must_use]
pub fn teardown(repo_root: &str, path: &str) -> String {
    let root = Path::new(repo_root);
    let outcome = run_with(|| remove_once(root, path), || prune(root));
    let (verdict, lines) = report(&outcome, repo_root, path);
    render(verdict, &lines)
}

#[cfg(test)]
mod tests;
