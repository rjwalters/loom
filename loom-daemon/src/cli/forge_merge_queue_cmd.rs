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
    };
    handle(&cmd)
}
