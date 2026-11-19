//! The `forge` half of `loom-tools`, shared with `loom-daemon forge` (#14395,
//! epic #12708; design `docs/design/loom-tools-cli-boundary.md` §3.1, §13).
//!
//! `loom-tools forge <verb>` parses the very same [`ForgeAction`] tree
//! `loom-daemon forge` does and runs the very same
//! [`handle_forge_command`](super::forge_action::handle_forge_command); the
//! only thing this module adds is the answer to "is this verb served?". A verb
//! that is not served is refused with [`EX_WRITE_VERB_NOT_MIGRATED`] before
//! any handler runs, so no second implementation exists and no write can slip
//! onto the tools path ahead of its own slice. Since #14519 every verb is
//! served except the merge-queue writes, which stay in `loom-daemon` until
//! their own slice (#14520) — so the refusal is now about *which* slice owns
//! the verb, not about reads versus writes.
//!
//! [`ForgeAction::is_served`] is an exhaustive `match` with no wildcard:
//! adding a verb to `ForgeAction` does not compile until its author has said
//! whether the tools binary serves it. [`ForgeAction::reaches_forge`] is the
//! same decision for the mode gate (§10.3), so a served verb that can reach
//! the forge still resolves `mode` before it runs.

use anyhow::Result;
use clap::Args;

use super::forge_action::{handle_forge_command, ForgeAction};
use super::forge_merge_queue_cmd::MergeQueueAction;

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
    /// [`ForgeAction::is_served`]).
    #[must_use]
    pub fn is_served(&self) -> bool {
        self.action.is_served()
    }

    /// Whether the selected verb may reach the forge, and so must pass the
    /// mode gate (design §10.3) before it runs. `false` only for verbs that
    /// touch nothing but stdin, argv, local git objects, a local lock
    /// directory or a local journal.
    #[must_use]
    pub fn reaches_forge(&self) -> bool {
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
                // Local-only by trace: a per-PR directory under
                // `LOOM_VERDICT_LOCK_DIR` (or `$HOME/.loom/locks/post-verdict`)
                // held across the gate read and the writes of another verb; it
                // makes no forge call itself (`forge_verdict_cmd::verdict_lock`).
                | ForgeAction::VerdictLock { .. }
                // Appends a marker line to the `LOOM_PICK_JOURNAL` file and
                // is a no-op outside a journaled run or on an unknown class;
                // no forge call (`pick_journal::record_ci_refusal`).
                | ForgeAction::JournalCiRefusal { .. }
        )
    }

    /// The verb as typed, for the refusal message.
    #[must_use]
    pub fn verb(&self) -> &'static str {
        self.action.verb()
    }
}

/// Run the verb through the one shared handler. Refuses (exit
/// [`EX_WRITE_VERB_NOT_MIGRATED`]) a verb the tools binary does not serve.
///
/// # Errors
/// Whatever [`handle_forge_command`] returns; most handlers exit the process
/// themselves, with their documented exit codes.
pub fn handle_forge_read_command(cli: ForgeReadCli) -> Result<()> {
    if !cli.is_served() {
        eprintln!(
            "loom-tools forge: `{}` is a merge-queue write and is not served by loom-tools yet \
             (epic #12708 Phase 3b, #14520); use `loom-daemon forge {}`.",
            cli.verb(),
            cli.verb()
        );
        std::process::exit(EX_WRITE_VERB_NOT_MIGRATED);
    }
    handle_forge_command(cli.action)
}

impl ForgeAction {
    /// `true` for a verb the tools binary serves, `false` for one still kept
    /// in `loom-daemon`. Since #14519 the only unserved verbs are the
    /// merge-queue writes, owned by the sibling slice #14520: everything else
    /// — reads and the write verbs of #14395/#14464/#14519 — runs through the
    /// one shared [`handle_forge_command`](super::forge_action::handle_forge_command),
    /// whose `write_target` gate vets a write exactly as `loom-daemon forge`
    /// does, and whose mode gate the binary resolves before any forge call.
    ///
    /// The `issue` and `pr` passthroughs exec the person's own `gh`;
    /// [`handle_forge_command`] vets a write op (`pr merge`, `issue
    /// comment`, ...) with `write_target` exactly as `loom-daemon forge` does.
    #[must_use]
    pub(crate) fn is_served(&self) -> bool {
        match self {
            Self::Issue { .. }
            | Self::Pr { .. }
            | Self::Auth { .. }
            | Self::CheckOpenPr { .. }
            | Self::PriorityLabels(_)
            | Self::RebaseGuard(_)
            | Self::PrCongestion { .. }
            | Self::JournalCiRefusal { .. }
            | Self::WaitChecks { .. }
            | Self::Rerun { .. }
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
            | Self::AutoMerge { .. }
            | Self::DisableAutoMerge { .. }
            // Writes served since #14519: the comment chokepoint, REST issue
            // filing, CI rerun, the auto-merge compat verbs, the verdict
            // labels/reconcile/lock family, the Doctor's handback and the
            // parent sub-issue link. All run the shared handler's `write_target`
            // gate (or, for `verdict-lock` and `journal-ci-refusal`, touch only
            // local state — see [`Self::reaches_forge`]).
            | Self::Comment { .. }
            | Self::CreateIssue(_)
            | Self::Parent { .. }
            | Self::DashboardLink { .. }
            | Self::VerdictLabels { .. }
            | Self::DoctorHandback { .. }
            | Self::VerdictReconcile { .. }
            | Self::VerdictLock { .. } => true,
            // The merge-queue reads only; its writes stay in `loom-daemon`
            // for #14520.
            Self::MergeQueue { action } => matches!(
                action.0,
                MergeQueueAction::Mode
                    | MergeQueueAction::Preflight { .. }
                    | MergeQueueAction::Status { .. }
            ),
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
