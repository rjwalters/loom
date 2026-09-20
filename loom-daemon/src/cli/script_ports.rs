//! Epic #7810's subcommands: those that back a retired shell script, plus the
//! epic's own instrumentation (`shell-budget`).
//!
//! Every variant here is the implementation behind a `defaults/scripts/*.sh`
//! entry point that is now a thin stub. The stubs' names, flags, stdout and
//! exit codes are contract — role prompts invoke them by path and parse or
//! `eval` their output — so these subcommands inherit that contract.
//!
//! They are gathered into one flattened enum for two reasons. They are one
//! family, added and reviewed together as the epic lands each port; and
//! `main.rs` is over `.loom/docs/file-size-policy.md`'s threshold and frozen,
//! so each new port must cost it nothing. Flattening keeps every subcommand
//! top-level on the CLI (`loom-daemon classify-dependency-block`, not
//! `loom-daemon script-ports classify-dependency-block`) while `main.rs` holds
//! a single variant and a single dispatch arm for all of them.

use anyhow::Result;

#[derive(clap::Subcommand)]
pub(crate) enum ScriptPortCommand {
    /// Champion's dependency-classification family (PR 3):
    /// `classify-dependency-block`, `detect-dependency-cycle`,
    /// `detect-startable-subset`.
    #[command(flatten)]
    DepClassify(super::dep_classify::DepClassifyCommand),

    /// Curator's re-check fingerprints (PR 4), backing
    /// `dep-recheck-fingerprint.sh`.
    #[command(subcommand)]
    DepRecheckFingerprint(super::dep_recheck::DepRecheckCommand),

    /// Download + verify one release artifact (PR 6a). Backs
    /// `loom-daemon-update.sh`'s `fetch_and_verify_artifact`. Exit 0 verified,
    /// 1 verification failed (tamper evidence), 2 could not even download —
    /// see `cli/release_fetch.rs` for the full contract.
    ReleaseFetch(super::release_fetch::ReleaseFetchArgs),

    /// Resolve the latest release artifact for this host, read-only (PR 5).
    /// Backs `loom-daemon-update.sh --resolve-json`. Exit 0 when one resolved,
    /// 1 when none did — data, not an error.
    ReleaseResolve(super::release_resolve::ReleaseResolveArgs),

    /// `merge-pr.sh`'s verdict-label mutual-exclusion guard (#8112), the
    /// second slice of the merge-pr port (#8191). Exit 1 = contradictory,
    /// 0 = clean, 2 = the guard could not run — and 2 must refuse the merge.
    #[command(subcommand)]
    MergePr(MergePrCommand),

    /// How far the epic actually is: portable shell remaining, the permanent
    /// floor, and the net change since the first port. Not a port itself — it
    /// lives here because `main.rs` is frozen by the file-size ratchet and this
    /// flattened enum is what keeps a new top-level subcommand free.
    ShellBudget(super::shell_budget::ShellBudgetArgs),

    /// `merge-pr.sh`'s closing-reference / partial-increment analysis (#8191,
    /// slice 1). Reads the PR body on stdin — it is untrusted external content
    /// and routinely tens of kilobytes, so it does not belong in argv.
    #[command(subcommand)]
    MergePrRefs(super::merge_pr_refs::MergePrRefsCommand),

    /// `worktree.sh`'s repo-global worktree-add lock (#8195, slice 1) — the
    /// lock every destructive path in that script stands behind, and which
    /// #6014/#6017 showed could be released by a holder that no longer owned
    /// it.
    #[command(subcommand)]
    WorktreeLock(super::worktree_lock::WorktreeLockCommand),

    /// `worktree.sh`'s WIP-shelving verbs (#8195, slice 2): `snapshot`,
    /// `stash-push`, `stash-pop`. The part of that script whose entire purpose
    /// is not losing somebody's uncommitted work — and whose `stash-push` runs
    /// `git reset --hard` once a capture has succeeded.
    #[command(subcommand)]
    WorktreeWip(super::worktree_wip::WorktreeWipCommand),

    /// `claude-wrapper.sh`'s retry/rotation classifiers (#8037): retry vs give
    /// up, rotate, mark a credential dead, and the backoff curve. Exit 0 when
    /// the predicate holds, 1 when it does not — an answer, not an error.
    #[command(subcommand)]
    RetryClassify(super::retry_classify::RetryClassifyCommand),
    /// Host-side autonomy-loss detector (#8086), backing
    /// `loom-daemon-watchdog.sh`. Run by a launchd/systemd timer on a
    /// `StartInterval` cadence, so it owns no long-lived process.
    DaemonWatchdog(super::watchdog::WatchdogArgs),

    /// The combined "not a work item" label list for a role prompt's
    /// unfiltered fallback query (#8255): the fleet-wide hard exclusions
    /// (`hard-exclusion-labels.sh`'s list) plus this workspace's configured
    /// `autonomous.workFinder.extraSkipLabels` (#6685). Backs
    /// `skip-labels.sh`. See `cli/skip_labels.rs` for why the two knobs had
    /// drifted apart (2AMLogic/2am's `journal` label).
    SkipLabels(super::skip_labels::SkipLabelsArgs),

    /// What a dispatched agent actually left behind (#8267): commits on the
    /// branch vs. deliverables still sitting uncommitted in the worktree, plus
    /// the `Stop`/`SubagentStop` hook that refuses a clean completion when the
    /// two disagree. Not a port either — same frozen-`main.rs` reason as
    /// `shell-budget` above.
    #[command(subcommand)]
    WorktreeState(super::worktree_state::WorktreeStateCommand),

    /// `check-duplicate.sh`'s similarity scan (#8360): keyword extraction,
    /// true-Jaccard scoring (#4409), threshold banding, the #8289 near-match
    /// band and the degenerate-result detector, ported out of the
    /// `contract`-category script per the shell language policy. Reads the
    /// candidate pool on stdin; stdout and exit codes are the lines and codes
    /// the script's own aggregation always consumed.
    DuplicateScan(super::duplicate_scan::DuplicateScanArgs),
}

impl ScriptPortCommand {
    /// Never returns: every arm exits with its subcommand's own code, which
    /// the stubs' callers branch on.
    pub(crate) fn run(self) -> Result<()> {
        match self {
            ScriptPortCommand::DepClassify(cmd) => cmd.run(),
            ScriptPortCommand::DepRecheckFingerprint(cmd) => cmd.run(),
            ScriptPortCommand::ReleaseFetch(args) => args.run(),
            ScriptPortCommand::ReleaseResolve(args) => args.run(),
            ScriptPortCommand::MergePr(cmd) => cmd.run(),
            ScriptPortCommand::ShellBudget(args) => args.run(),
            ScriptPortCommand::MergePrRefs(cmd) => cmd.run(),
            ScriptPortCommand::WorktreeLock(cmd) => cmd.run(),
            ScriptPortCommand::WorktreeWip(cmd) => cmd.run(),
            ScriptPortCommand::RetryClassify(cmd) => cmd.run(),
            ScriptPortCommand::DaemonWatchdog(args) => args.run(),
            ScriptPortCommand::SkipLabels(args) => args.run(),
            ScriptPortCommand::WorktreeState(cmd) => cmd.run(),
            ScriptPortCommand::DuplicateScan(args) => args.run(),
        }
    }
}

/// `merge-pr.sh`'s ported decisions, grouped under one subcommand so the
/// script's slices stay legible as a family rather than scattering across the
/// top level.
#[derive(clap::Subcommand)]
pub(crate) enum MergePrCommand {
    /// Refuse a PR carrying `loom:pr` alongside a contradicting label.
    VerdictContradiction(super::merge_pr_labels::VerdictContradictionArgs),

    /// Refuse a merge whose required checks ran before the base branch's
    /// current tip (#8248): a stale green ratchet result is evidence about a
    /// tree that no longer exists. Exit 0+CLEAN = fresh, 1 = stale, 2 =
    /// could not determine (must also refuse).
    StaleChecks(super::merge_pr_stale_checks::StaleChecksArgs),

    /// Decide whether a head-SHA-mismatch refusal was caused by THIS merge
    /// run's own base-sync push (#8164) and may be retried once against a
    /// freshly-read head. Exit 0 + sentinel = retry authorized, 1 = foreign
    /// head move (re-queue), 2 = attribution undeterminable (also re-queue).
    HeadSyncRetry(super::merge_pr_head_sync::HeadSyncRetryArgs),
}

impl MergePrCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            MergePrCommand::VerdictContradiction(args) => args.run(),
            MergePrCommand::StaleChecks(args) => args.run(),
            MergePrCommand::HeadSyncRetry(args) => args.run(),
        }
    }
}
