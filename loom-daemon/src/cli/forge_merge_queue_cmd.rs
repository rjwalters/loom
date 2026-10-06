//! `loom-daemon forge merge-queue …` (#10255) — clap surface only. All
//! decisions live in [`loom_daemon::forge_merge_queue`].

use clap::Subcommand;
use loom_daemon::forge_merge_queue::{handle, MergeQueueCmd};

/// Dormant merge-queue controls (#9978 Phase A). `mode`, `preflight` and
/// `status` are read-only; `enqueue`/`dequeue` refuse (exit 4) under
/// `champion.mergeMode=direct` (the default) and while queue execution is
/// dormant in this build. Exit codes: 0 ok, 1 failed / not capable, 2 invalid
/// config or usage, 3 could not determine, 4 refused before any forge call.
#[derive(Subcommand)]
pub(crate) enum MergeQueueAction {
    /// Print the resolved `champion.mergeMode` (env `LOOM_MERGE_MODE` >
    /// config > default `direct`), its source, and the execution gate.
    Mode,
    /// Capability preflight: is there an active `merge_queue` rule with
    /// required status checks on the branch? Never writes; never falls back.
    Preflight {
        /// Repository, `owner/repo`. Default: the repository of the CWD.
        #[arg(long, value_name = "NWO")]
        repo: Option<String>,
        /// Branch to check. Default: the repository's default branch.
        #[arg(long, value_name = "BRANCH")]
        branch: Option<String>,
    },
    /// Read a PR's head and merge-queue entry.
    Status {
        #[arg(value_name = "PR")]
        pr: u32,
        #[arg(long, value_name = "NWO")]
        repo: Option<String>,
    },
    /// Enqueue a PR pinned to the approved head (`expectedHeadOid`).
    /// Idempotent. Dormant in this build.
    Enqueue {
        #[arg(value_name = "PR")]
        pr: u32,
        /// The full 40-character head SHA the Judge approved.
        #[arg(long, value_name = "SHA")]
        approved_sha: String,
        #[arg(long, value_name = "NWO")]
        repo: Option<String>,
    },
    /// Remove a PR from the merge queue. Idempotent. Dormant in this build.
    Dequeue {
        #[arg(value_name = "PR")]
        pr: u32,
        #[arg(long, value_name = "NWO")]
        repo: Option<String>,
    },
    /// #10256: reconcile a PR with the queue (`merge-pr.sh` calls this with
    /// `--pr`), or every pending queued PR (no `--pr`). Confirms merges,
    /// comments and routes drops with GitHub's verified reason, revokes and
    /// dequeues a queued PR that lost its authorization. Prints
    /// `LOOM-MERGE-QUEUE-DIRECT` and exits 0 in direct mode without any forge
    /// call. Exit 7 = queued/dropped (not merged, nothing wrong), 3 = could
    /// not determine (do not merge).
    Reconcile {
        #[arg(long, value_name = "PR")]
        pr: Option<u32>,
        #[arg(long, value_name = "NWO")]
        repo: Option<String>,
        /// Only report the mode; read and write nothing (dry runs).
        #[arg(long)]
        mode_only: bool,
    },
    /// #10256: authorize and enqueue an approved PR after every direct merge
    /// guard passed. Refuses (exit 4) in direct mode and while dormant;
    /// refuses unless the ruleset requires `loom/merge-authorization`. Exit 7
    /// = handed off (NOT merged). Never falls back to a direct merge.
    Handoff {
        #[arg(value_name = "PR")]
        pr: u32,
        #[arg(long, value_name = "SHA")]
        approved_sha: String,
        #[arg(long, value_name = "NWO")]
        repo: Option<String>,
    },
    /// #10256: the Champion's one call at the merge point: `reconcile`, then
    /// `handoff` only when reconcile says continue. Direct mode prints
    /// `LOOM-MERGE-QUEUE-DIRECT` and exits 0 (run `merge-pr.sh` as before);
    /// any other first line means do NOT run `merge-pr.sh`.
    Step {
        #[arg(value_name = "PR")]
        pr: u32,
        #[arg(long, value_name = "SHA")]
        approved_sha: String,
        #[arg(long, value_name = "NWO")]
        repo: Option<String>,
    },
    /// #10256: revoke the queue authorization and dequeue BEFORE a Loom-owned
    /// transition (verdict invalidation, review claim, operator hold). Exit 0
    /// when the transition may proceed (and always in direct mode).
    Revoke {
        #[arg(value_name = "PR")]
        pr: u32,
        /// Short token recorded in the revoke marker, e.g. `stale-verdict`.
        #[arg(long, default_value = "transition")]
        reason: String,
        #[arg(long, value_name = "NWO")]
        repo: Option<String>,
    },
    /// #10256: the required `loom/merge-authorization` check body for the PR
    /// head a merge group was built from. Exit 0 = success, 1 = failure
    /// (any unknown fact, outage or forge error fails).
    AuthorizeCheck {
        #[arg(value_name = "PR")]
        pr: u32,
        #[arg(long, value_name = "SHA")]
        pr_head: String,
        #[arg(long, value_name = "NWO")]
        repo: Option<String>,
    },
}

/// `forge merge-queue`'s subcommand slot (#10256), built lazily.
///
/// A debug build gives each clap-derived `augment_subcommands` one stack frame
/// sized by every arg it declares, and the derive nests them: `Commands`
/// (~1.2 MB) -> `ForgeAction` (~0.7 MB) -> `MergeQueueAction` (~0.2 MB). Adding
/// `Step` pushed that chain past the 2 MB test-thread stack (the
/// `cli::sweep_outcomes_cli` parse tests aborted with a stack overflow). Boxing
/// the field does not help: `Box<T>`'s `augment_subcommands` is `T`'s. Here
/// [`clap::Command::defer`] adds the merge-queue verbs only when clap builds the
/// `merge-queue` command itself (it is selected, or `--help` / `build()` walks
/// it), in a frame of its own after `Commands`' has returned.
pub(crate) struct DeferredMergeQueue(pub(crate) MergeQueueAction);

impl clap::FromArgMatches for DeferredMergeQueue {
    fn from_arg_matches(m: &clap::ArgMatches) -> Result<Self, clap::Error> {
        MergeQueueAction::from_arg_matches(m).map(Self)
    }

    fn from_arg_matches_mut(m: &mut clap::ArgMatches) -> Result<Self, clap::Error> {
        MergeQueueAction::from_arg_matches_mut(m).map(Self)
    }

    fn update_from_arg_matches(&mut self, m: &clap::ArgMatches) -> Result<(), clap::Error> {
        self.0.update_from_arg_matches(m)
    }

    fn update_from_arg_matches_mut(&mut self, m: &mut clap::ArgMatches) -> Result<(), clap::Error> {
        self.0.update_from_arg_matches_mut(m)
    }
}

impl Subcommand for DeferredMergeQueue {
    fn augment_subcommands(cmd: clap::Command) -> clap::Command {
        cmd.defer(MergeQueueAction::augment_subcommands)
    }

    fn augment_subcommands_for_update(cmd: clap::Command) -> clap::Command {
        cmd.defer(MergeQueueAction::augment_subcommands_for_update)
    }

    fn has_subcommand(name: &str) -> bool {
        MergeQueueAction::has_subcommand(name)
    }
}

pub(crate) fn run(action: MergeQueueAction) -> ! {
    let cmd = match action {
        MergeQueueAction::Mode => MergeQueueCmd::Mode,
        MergeQueueAction::Preflight { repo, branch } => MergeQueueCmd::Preflight { repo, branch },
        MergeQueueAction::Status { pr, repo } => MergeQueueCmd::Status { pr, repo },
        MergeQueueAction::Enqueue {
            pr,
            approved_sha,
            repo,
        } => MergeQueueCmd::Enqueue {
            pr,
            approved_sha,
            repo,
        },
        MergeQueueAction::Dequeue { pr, repo } => MergeQueueCmd::Dequeue { pr, repo },
        MergeQueueAction::Reconcile {
            pr,
            repo,
            mode_only,
        } => MergeQueueCmd::Reconcile {
            pr,
            repo,
            mode_only,
        },
        MergeQueueAction::Handoff {
            pr,
            approved_sha,
            repo,
        } => MergeQueueCmd::Handoff {
            pr,
            approved_sha,
            repo,
        },
        MergeQueueAction::Step {
            pr,
            approved_sha,
            repo,
        } => MergeQueueCmd::Step {
            pr,
            approved_sha,
            repo,
        },
        MergeQueueAction::Revoke { pr, reason, repo } => MergeQueueCmd::Revoke { pr, reason, repo },
        MergeQueueAction::AuthorizeCheck { pr, pr_head, repo } => {
            MergeQueueCmd::AuthorizeCheck { pr, pr_head, repo }
        }
    };
    handle(&cmd)
}
