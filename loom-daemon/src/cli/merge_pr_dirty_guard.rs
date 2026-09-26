//! `loom-daemon merge-pr dirty-guard` (#5031/#5658, a slice of #8191).
//!
//! # Exit codes, and why this one is a gate
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | no user work in the worktree | [`CLEAN`] | 0 |
//! | user work present — do not remove | the refusal, as `LEVEL<TAB>message` | 1 |
//! | stdin unreadable | (nothing) | 2 |
//!
//! Every other step of `merge-pr.sh`'s post-merge cleanup is best-effort: the
//! merge already succeeded, so a cleanup that cannot run is a warning. This one
//! is different, and its wrapper treats an unrecognised outcome as a refusal
//! rather than a pass — because the action it gates is `git worktree remove
//! --force`, and a sibling builder's uncommitted edits destroyed by it are gone
//! for good (#5031). A skipped removal is recoverable by `loom-clean`, the
//! daemon's reaper, or the next merge. Refusing on exit 2 costs a cleanup;
//! passing on exit 2 costs somebody's afternoon.
//!
//! # The protocol
//!
//! One `LEVEL<TAB>message` line per output, `WARNING` or `PLAIN`, replayed by
//! the shell through its own `warning` / bare `echo` — the same shape
//! [`super::merge_pr_delete_branch`] established, for the same reason: this
//! guard's messages are interleaved with dozens of others in
//! `merge-pr.sh`'s output, and `test-merge-pr-dirty-worktree-guard.sh` stubs
//! those shell functions and asserts on the text passed to them.
//!
//! `PLAIN` exists because the retired code's last line was a bare, uncolored
//! `echo` of the remediation command. An operator copy-pastes that line, so it
//! must not arrive wrapped in ANSI codes.
//!
//! The porcelain arrives on stdin rather than being read here. That is
//! deliberate and is explained in [`loom_daemon::merge_pr::dirty_guard`]'s
//! module docs: `git status` FAILING has always meant "no dirt, proceed" on
//! this path (the #5177 orphaned-directory case cleanup exists to handle), and
//! the `|| true` that encodes it stays where it already lives.

use anyhow::Result;
use std::io::Read;

use loom_daemon::merge_pr::dirty_guard::{assess, Context, CLEAN};

#[derive(clap::Args)]
pub(crate) struct DirtyGuardArgs {
    /// The worktree being considered for removal (`$worktree_path`).
    #[arg(long, value_name = "PATH")]
    worktree_path: String,

    /// The repository root, for the remediation command (`$REPO_ROOT`).
    #[arg(long, value_name = "PATH", default_value = "")]
    repo_root: String,

    /// The branch checked out in that worktree. Omit (or pass empty) when it
    /// could not be resolved — the refusal then omits the `on branch '…'`
    /// clause, exactly as the retired `${live_branch:+…}` did.
    #[arg(long, value_name = "BRANCH", default_value = "")]
    branch: String,

    /// `git status --porcelain` output. Omit to read it from stdin, which is
    /// what the caller does.
    #[arg(long, value_name = "TEXT")]
    porcelain: Option<String>,
}

impl DirtyGuardArgs {
    pub(crate) fn run(self) -> Result<()> {
        let porcelain = match self.porcelain {
            Some(p) => p,
            None => {
                let mut buf = String::new();
                if std::io::stdin().read_to_string(&mut buf).is_err() {
                    // An unreadable stream is an UNKNOWN worktree state, which
                    // is not the same as a clean one. Printing the clean
                    // sentinel here would authorize a `--force` on evidence
                    // nobody has.
                    eprintln!(
                        "merge-pr dirty-guard: could not read `git status --porcelain` from stdin"
                    );
                    std::process::exit(2);
                }
                buf
            }
        };

        let ctx = Context {
            worktree_path: &self.worktree_path,
            repo_root: &self.repo_root,
            branch: &self.branch,
        };

        match assess(&ctx, &porcelain) {
            None => {
                println!("{CLEAN}");
                Ok(())
            }
            Some(records) => {
                for (level, message) in records {
                    println!("{}\t{message}", level.token());
                }
                std::process::exit(1);
            }
        }
    }
}
