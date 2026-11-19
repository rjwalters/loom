//! The read-only half of `forge`, shared by `loom-daemon forge` and
//! `loom-tools forge` (#14395, epic #12708 Phase 3a; design
//! `docs/design/loom-tools-cli-boundary.md` §3.1).
//!
//! `loom-tools forge <verb>` parses the very same [`ForgeAction`] tree
//! `loom-daemon forge` does and runs the very same
//! [`handle_forge_command`](super::forge_action::handle_forge_command); the only
//! thing this module adds is the answer to "is this verb a read?". A verb that
//! is not a read is refused with [`EX_WRITE_VERB_NOT_MIGRATED`] before any
//! handler runs, so no second implementation exists and no write can slip
//! onto the tools path ahead of its own slice (#14396).
//!
//! [`ForgeAction::is_read`] is an exhaustive `match` with no wildcard: adding a
//! verb to `ForgeAction` does not compile until its author has said which side
//! of the line it is on.

use anyhow::Result;
use clap::Args;

use super::forge_action::{handle_forge_command, ForgeAction};
use super::forge_merge_queue_cmd::MergeQueueAction;
use super::forge_parent_cmd::ParentAction;
use crate::forge_merge_queue::mode::{resolve_merge_mode, MergeModeError, ResolvedMergeMode};

/// Exit status for a `forge` verb `loom-tools` does not serve (BSD
/// `EX_USAGE`). Not `1`: `check-claim`, `check-open-pr` and friends use `1` as
/// the data answer "safe", and a refusal must never read as one.
pub const EX_WRITE_VERB_NOT_MIGRATED: i32 = 64;

/// `loom-tools forge <verb>`: the [`ForgeAction`] subcommand tree, unchanged.
///
/// The field is private so the crate-private `ForgeAction` type does not leak
/// into the public API; the binary only parses and hands the value back to
/// [`handle_forge_read_command`].
#[derive(Args)]
pub struct ForgeReadCli {
    #[command(subcommand)]
    action: ForgeAction,
}

impl ForgeReadCli {
    /// Whether the selected verb is served by `loom-tools` (see
    /// [`ForgeAction::is_read`]).
    #[must_use]
    pub fn is_read(&self) -> bool {
        self.action.is_read()
    }

    /// Whether the selected verb may reach the forge, and so must pass the
    /// mode gate (design §10.3) before it runs. `false` only for verbs that
    /// touch nothing but stdin, argv, local git objects or local files.
    #[must_use]
    pub fn reaches_forge(&self) -> bool {
        // #14520: a merge-queue verb is classified by what it does in the
        // resolved `champion.mergeMode`, not by its name.
        if let ForgeAction::MergeQueue { action } = &self.action {
            return action.0.reaches_forge(&current_merge_mode());
        }
        !matches!(
            self.action,
            // Reads local git objects only (its own doc comment).
            ForgeAction::RebaseGuard(_)
                // stdin in, one line out.
                | ForgeAction::VerdictBodyCheck
                // Pure formatting of the dashboard footer.
                | ForgeAction::DashboardLink { .. }
                // This host's forge-call sink: local files only.
                | ForgeAction::Calls(_)
        )
    }

    /// The verb as typed, for the refusal message.
    #[must_use]
    pub fn verb(&self) -> &'static str {
        self.action.verb()
    }
}

/// `champion.mergeMode` for the current checkout, resolved exactly as
/// `forge_merge_queue::Env::current` does (without detecting the forge).
fn current_merge_mode() -> Result<ResolvedMergeMode, MergeModeError> {
    let root = crate::repo_root::find_repo_root_from_cwd()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    resolve_merge_mode(&root)
}

/// Run the verb through the one shared handler. Refuses (exit
/// [`EX_WRITE_VERB_NOT_MIGRATED`]) a verb that is not a read.
///
/// # Errors
/// Whatever [`handle_forge_command`] returns; most handlers exit the process
/// themselves, with their documented exit codes.
pub fn handle_forge_read_command(cli: ForgeReadCli) -> Result<()> {
    if !cli.is_read() {
        eprintln!(
            "loom-tools forge: `{}` writes to the forge and is not served by loom-tools yet \
             (epic #12708 Phase 3b, #14396); use `loom-daemon forge {}`.",
            cli.verb(),
            cli.verb()
        );
        std::process::exit(EX_WRITE_VERB_NOT_MIGRATED);
    }
    handle_forge_command(cli.action)
}

impl ForgeAction {
    /// `true` for a verb that only reads (migrated by #14395), `false` for one
    /// that writes to the forge, to a local journal or lock, or whose read/write
    /// split was not established (kept in `loom-daemon` for #14396).
    ///
    /// The `issue` and `pr` passthroughs are reads here by the issue's own
    /// ruling: they exec the person's own `gh`, and
    /// [`handle_forge_command`] still vets a write op (`pr merge`, `issue
    /// comment`, ...) with `write_scope` exactly as `loom-daemon forge` does.
    #[must_use]
    pub(crate) fn is_read(&self) -> bool {
        match self {
            Self::Issue { .. }
            | Self::Pr { .. }
            | Self::Auth { .. }
            | Self::CheckOpenPr { .. }
            | Self::PriorityLabels(_)
            | Self::RebaseGuard(_)
            | Self::PrCongestion { .. }
            | Self::WaitChecks { .. }
            | Self::CheckClaim { .. }
            | Self::CheckBranch { .. }
            | Self::TreeUnchanged { .. }
            | Self::VerdictEquivalent { .. }
            | Self::VersionOnlyDiff { .. }
            | Self::AdditiveMigrations { .. }
            | Self::LockedDepsOnly { .. }
            | Self::ResyncWaiver { .. }
            | Self::SuspectCriticalDiff { .. }
            | Self::MergeMethod { .. }
            | Self::MergeConfig { .. }
            | Self::Token { .. }
            | Self::IsFleet { .. }
            | Self::TrustedComments { .. }
            | Self::PromotionAuthor { .. }
            | Self::ClaimLiveness { .. }
            | Self::VerdictStaleNotice { .. }
            | Self::VerdictBodyCheck
            | Self::VerdictGate { .. }
            | Self::MayWrite { .. }
            | Self::Calls(_)
            | Self::Identities { .. }
            | Self::DashboardLink { .. } => true,
            // #14520: every queue verb. The mutating ones keep their own
            // vetting: `write_target` (enqueue, dequeue, handoff, revoke) and
            // the in-handler `may_write_from` (reconcile's queue branch,
            // group-check; `step` runs reconcile first) run on this path too.
            Self::MergeQueue { action } => match action.0 {
                MergeQueueAction::Mode
                | MergeQueueAction::Preflight { .. }
                | MergeQueueAction::Status { .. }
                | MergeQueueAction::Enqueue { .. }
                | MergeQueueAction::Dequeue { .. }
                | MergeQueueAction::Reconcile { .. }
                | MergeQueueAction::Handoff { .. }
                | MergeQueueAction::Step { .. }
                | MergeQueueAction::Revoke { .. }
                | MergeQueueAction::GroupCheck { .. }
                | MergeQueueAction::AuthorizeCheck { .. } => true,
            },
            Self::Parent { action } => {
                matches!(action, ParentAction::Body { .. } | ParentAction::SplitState { .. })
            }
            // Writes: #14396.
            Self::JournalCiRefusal { .. }
            | Self::Rerun { .. }
            | Self::AutoMerge { .. }
            | Self::DisableAutoMerge { .. }
            | Self::VerdictLabels { .. }
            | Self::DoctorHandback { .. }
            | Self::VerdictReconcile { .. }
            | Self::VerdictLock { .. }
            | Self::Comment { .. }
            | Self::CreateIssue(_) => false,
        }
    }

    /// The verb's command name, for diagnostics.
    pub(crate) fn verb(&self) -> &'static str {
        match self {
            Self::Issue { .. } => "issue",
            Self::Pr { .. } => "pr",
            Self::Auth { .. } => "auth",
            Self::CheckOpenPr { .. } => "check-open-pr",
            Self::PriorityLabels(_) => "priority-labels",
            Self::RebaseGuard(_) => "rebase-guard",
            Self::PrCongestion { .. } => "pr-congestion",
            Self::JournalCiRefusal { .. } => "journal-ci-refusal",
            Self::WaitChecks { .. } => "wait-checks",
            Self::Rerun { .. } => "rerun",
            Self::CheckClaim { .. } => "check-claim",
            Self::CheckBranch { .. } => "check-branch",
            Self::AutoMerge { .. } => "auto-merge",
            Self::DisableAutoMerge { .. } => "disable-auto-merge",
            Self::TreeUnchanged { .. } => "tree-unchanged",
            Self::VerdictEquivalent { .. } => "verdict-equivalent",
            Self::VersionOnlyDiff { .. } => "version-only-diff",
            Self::AdditiveMigrations { .. } => "additive-migrations",
            Self::LockedDepsOnly { .. } => "locked-deps-only",
            Self::ResyncWaiver { .. } => "resync-waiver",
            Self::SuspectCriticalDiff { .. } => "suspect-critical-diff",
            Self::MergeMethod { .. } => "merge-method",
            Self::MergeConfig { .. } => "merge-config",
            Self::MergeQueue { .. } => "merge-queue",
            Self::Token { .. } => "token",
            Self::IsFleet { .. } => "is-fleet",
            Self::TrustedComments { .. } => "trusted-comments",
            Self::PromotionAuthor { .. } => "promotion-author",
            Self::ClaimLiveness { .. } => "claim-liveness",
            Self::VerdictStaleNotice { .. } => "verdict-stale-notice",
            Self::VerdictBodyCheck => "verdict-body-check",
            Self::VerdictGate { .. } => "verdict-gate",
            Self::VerdictLabels { .. } => "verdict-labels",
            Self::DoctorHandback { .. } => "doctor-handback",
            Self::VerdictReconcile { .. } => "verdict-reconcile",
            Self::VerdictLock { .. } => "verdict-lock",
            Self::MayWrite { .. } => "may-write",
            Self::Calls(_) => "calls",
            Self::Identities { .. } => "identities",
            Self::Comment { .. } => "comment",
            Self::CreateIssue(_) => "create-issue",
            Self::Parent { .. } => "parent",
            Self::DashboardLink { .. } => "dashboard-link",
        }
    }
}

#[cfg(test)]
mod tests;
