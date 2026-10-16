//! `loom-daemon merge-pr post-merge-cleanup` (#8191, slice 1 of
//! `docs/migration/merge-pr-orchestrator-port.md`): the whole worktree/branch
//! cleanup block that follows a successful merge, in one process.
//!
//! # Protocol
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | cleanup ran (or was correctly skipped) | `LEVEL<TAB>message` records, `LEVEL` one of `INFO` `WARNING` `SUCCESS` `PLAIN`; optionally one `CD<TAB>dir`; then the terminator [`DONE`] | 0 |
//! | a forge fact is needed first | one `LOOM-NEED <close-targets\|issue-state N\|landed>` line, NOTHING else, nothing done | 3 |
//!
//! The shell replays the records through its own `info`/`warning`/`success`/
//! `echo`, fetches a needed fact and calls again with it. See
//! [`loom_daemon::merge_pr::post_merge_cleanup`] for why the facts are asked
//! for rather than fetched here, and for the fail direction (toward removing
//! nothing).
//!
//! Any other exit means the verb did not run (a missing or older binary). The
//! shell then removes nothing and says so: the merge already succeeded and a
//! skipped cleanup is recoverable (`loom-clean`, the daemon's reaper).

use anyhow::Result;

use loom_daemon::merge_pr::post_merge_cleanup::{
    run, Facts, Inputs, Landed, Outcome, SystemHost, DONE, NEED_EXIT,
};

#[derive(clap::Args)]
pub(crate) struct PostMergeCleanupArgs {
    /// The main repository root (`$REPO_ROOT`).
    #[arg(long, value_name = "PATH")]
    repo_root: String,

    /// The merged PR's number.
    #[arg(long, value_name = "N")]
    pr: String,

    /// The merged PR's head branch.
    #[arg(long, value_name = "BRANCH")]
    branch: String,

    /// The merged PR's `head.sha` (`$PR_HEAD_SHA`); a tip matching it is landed
    /// with no forge round-trip.
    #[arg(long, value_name = "SHA", default_value = "")]
    head_sha: String,

    /// The repo's default branch, when it resolved.
    #[arg(long, value_name = "BRANCH")]
    default_branch: Option<String>,

    /// `--no-cleanup-worktree`: skip everything.
    #[arg(long)]
    no_cleanup_worktree: bool,

    /// `--no-cleanup-primary` (#5015 opt-out).
    #[arg(long)]
    no_cleanup_primary: bool,

    /// `LOOM_PRESERVE_WORKTREE=1`.
    #[arg(long)]
    preserve_env: bool,

    /// `--worktree-path`, already validated by the shell.
    #[arg(long, value_name = "PATH", default_value = "")]
    worktree_path: String,

    /// `forge_pr_close_targets`' output. Pass it (empty included) once fetched.
    #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
    close_targets: Option<String>,

    /// `forge_get_issue_state`' output (`OPEN`/`CLOSED`, empty on a failed
    /// lookup). Pass it (empty included) once fetched.
    #[arg(long, value_name = "STATE")]
    issue_state: Option<String>,

    /// `branch_has_landed`'s exit: `landed` or `not-landed`. Pass it once
    /// fetched, with the two globals below.
    #[arg(long, value_enum)]
    landed_result: Option<LandedResult>,

    /// `$BRANCH_LANDED_VERDICT`.
    #[arg(long, value_name = "VERDICT", default_value = "")]
    landed_verdict: String,

    /// `$BRANCH_LANDED_EVIDENCE`.
    #[arg(long, value_name = "EVIDENCE", default_value = "")]
    landed_evidence: String,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum LandedResult {
    Landed,
    NotLanded,
}

impl PostMergeCleanupArgs {
    pub(crate) fn run(self) -> Result<()> {
        let inputs = Inputs {
            repo_root: self.repo_root,
            pr_number: self.pr,
            pr_branch: self.branch,
            pr_head_sha: self.head_sha,
            default_branch: self.default_branch.filter(|b| !b.is_empty()),
            cleanup_worktree: !self.no_cleanup_worktree,
            cleanup_primary: !self.no_cleanup_primary,
            preserve_env: self.preserve_env,
            worktree_path_override: self.worktree_path,
        };
        let facts = Facts {
            close_targets: self.close_targets,
            issue_state: self.issue_state,
            landed: self.landed_result.map(|r| Landed {
                landed: matches!(r, LandedResult::Landed),
                verdict: self.landed_verdict,
                evidence: self.landed_evidence,
            }),
        };
        match run(&inputs, &facts, &SystemHost) {
            Outcome::Need(need) => {
                println!("LOOM-NEED {}", need.token());
                std::process::exit(NEED_EXIT);
            }
            Outcome::Done { records, cd } => {
                for (level, text) in records {
                    // A message never carries a newline here (every producer
                    // splits per line), but a stray one must not forge a record.
                    for line in text.split('\n') {
                        println!("{level}\t{line}");
                    }
                }
                if let Some(dir) = cd {
                    println!("CD\t{dir}");
                }
                // A positive terminator, like `dirty-guard`'s CLEAN: a binary
                // that exits 0 having printed nothing (stale, or stdout lost)
                // must not read as "cleanup ran and had nothing to say".
                println!("{DONE}");
                Ok(())
            }
        }
    }
}
