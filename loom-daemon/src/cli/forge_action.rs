//! `loom-daemon forge …`: the subcommand enum and its dispatcher.
//!
//! Moved verbatim out of `main.rs` (over `.loom/docs/file-size-policy.md`'s
//! threshold and frozen) and `cli/tokens.rs` (where the dispatcher had
//! landed for no reason but proximity), so new `forge` verbs cost `main.rs`
//! nothing: they are added here, beside the verbs they sit with.

use anyhow::Result;
use clap::Subcommand;

/// Sub-actions for `loom-daemon forge`.
///
/// The `issue`/`pr`/`auth` variants capture their trailing args verbatim and
/// (on GitHub) exec `gh <entity> <args…>`, so the surface stays byte-identical
/// to the `FORGE=gh` shell fallback the four scripts already understand.
#[derive(Subcommand)]
pub(crate) enum ForgeAction {
    /// `forge issue <args…>` — e.g. `issue view 42 --json labels --jq
    /// '.labels[].name'`. GitHub: exec `gh issue <args…>`.
    ///
    /// This is a byte-identical passthrough, so `forge issue create` is
    /// GraphQL-backed and has **no REST fallback** — it dies on GraphQL-quota
    /// exhaustion exactly like `gh issue create`, and is not an escape hatch
    /// from it (#5047). To file an issue that survives an exhausted GraphQL
    /// pool, use `.loom/scripts/create-issue.sh`.
    Issue {
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
    },
    /// `forge pr <args…>` — e.g. `pr list --state=merged --limit 20 --json
    /// number,title,body`. GitHub: exec `gh pr <args…>`.
    Pr {
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
    },
    /// `forge auth <args…>` — e.g. `auth status`. GitHub: exec `gh auth
    /// <args…>`.
    Auth {
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "ARGS"
        )]
        args: Vec<String>,
    },
    /// `forge check-open-pr <issue>` — the #4123 open-linked-PR guard as a
    /// **pre-claim** check for the manual/in-session Builder path (#8551).
    ///
    /// Exits `0` and prints the PR number when an open linked PR already
    /// exists (**do not claim**), `1` on a verified absence (safe to claim),
    /// and `5` when the probe could not answer (fail closed — NOT an
    /// absence). Reuses the same closes-graph ∪ timeline probe the daemon's
    /// own dispatch guard uses.
    #[command(name = "check-open-pr")]
    CheckOpenPr {
        /// Issue number you are about to claim.
        #[arg(value_name = "ISSUE")]
        issue: u32,
    },

    /// OPERATOR-ONLY: arm GitHub's server-side auto-merge for a PR. Not a
    /// Loom merge path — use `merge-pr.sh` instead.
    ///
    /// SAFETY CAVEAT (#8410, #8427): once armed, GitHub merges the PR as soon
    /// as the branch ruleset's REQUIRED checks pass. It does NOT re-read the
    /// `loom:pr` label, a later `loom:verdict-stale` / `loom:changes-requested`
    /// revocation, or any non-required test suite — PR #8220 merged exactly
    /// that way over a revoked verdict with five suites still running. No Loom
    /// merge path arms a server-side merge: `merge-pr.sh --auto` waits for the
    /// head's check-runs to settle, re-validates, and merges in-process. Use
    /// this verb only when a human deliberately wants a queued merge and
    /// accepts that it bypasses Loom's merge-time gates. Kept (rather than
    /// deleted) as a CLI compatibility surface for installed pre-#8410
    /// `merge-pr.sh` copies.
    ///
    /// `forge auto-merge <pr> [--method M] [--expected-head-sha SHA]`
    /// (formerly `loom-auto-merge`). GitHub: `enablePullRequestAutoMerge`
    /// GraphQL mutation. Gitea: declines (exit 3) — there is no Gitea arm.
    /// `--poll-interval` / `--timeout` are accepted for CLI compatibility and
    /// ignored (the server queues the merge).
    #[command(name = "auto-merge")]
    AutoMerge {
        /// Pull request number.
        #[arg(value_name = "PR")]
        pr_number: u32,

        /// Merge method (merge | squash | rebase). Default merge (#9105: merge
        /// commits preserve the branch's full history).
        #[arg(long, default_value = "merge")]
        method: String,

        /// Optimistic-concurrency precondition (#5589, mirrors #5579's shell
        /// `EXPECTED_HEAD_SHA`): the SHA the PR's head branch must currently
        /// match. Threaded into the GitHub `expectedHeadOid` GraphQL mutation
        /// input; a mismatch exits `EX_FORGE_HEAD_MISMATCH` (4) instead of
        /// the generic failure exit `1`, distinguishable from a Gitea decline
        /// (exit 3). Omit to preserve prior (unguarded) behavior.
        #[arg(long, value_name = "SHA")]
        expected_head_sha: Option<String>,

        /// Seconds between CI polls. Accepted for CLI compatibility only;
        /// ignored (the retired Gitea shell poller was its only reader).
        #[arg(long, value_name = "SECONDS")]
        poll_interval: Option<u64>,

        /// Max seconds to wait for CI. Accepted for CLI compatibility only;
        /// ignored (the retired Gitea shell poller was its only reader).
        #[arg(long, value_name = "SECONDS")]
        timeout: Option<u64>,
    },

    /// Disarm GitHub's server-side auto-merge on a PR. Safe to call anywhere a
    /// review verdict is invalidated.
    ///
    /// The inverse of `auto-merge`, and deliberately NOT operator-only: it can
    /// only turn a queued merge OFF, never on, so there is no state in which
    /// calling it makes an unreviewed merge more likely.
    ///
    /// WHY IT EXISTS (#8900): an armed auto-merge is gated only by the branch
    /// ruleset's REQUIRED checks. Clearing `loom:pr` for a head move does not
    /// disarm it, so the queued merge fires as soon as required checks pass on
    /// the NEW, unreviewed head — bypassing `loom:pr`, the non-required suites,
    /// and the #8248 required-check-freshness guard (which lives inside
    /// `merge-pr.sh`). #8694 merged that way on 2026-09-25, three minutes after
    /// a rebase force-push, still labeled `loom:review-requested`.
    ///
    /// Prints `DISARMED=1` (an arm was disabled) or `DISARMED=0` (nothing was
    /// armed — no mutation sent) and exits 0 for both; exits 1 when the arm
    /// state could not be read or the mutation failed (treat as possibly still
    /// armed, never as an all-clear); exits 3 on Gitea, which has no
    /// server-side arm to disable.
    ///
    /// `--audit-comment` also records what the disarm did as a PR comment, so a
    /// caller does not have to compose (and duplicate) that prose itself. It is
    /// silent when nothing was armed, which is the common case — no PR ever
    /// collects a comment saying nothing happened. This is the flag
    /// `verdict-staleness-guard.sh --clear` uses; `--hold` tells it the PR is
    /// parked so the comment explains why a held PR was written to at all.
    #[command(name = "disable-auto-merge")]
    DisableAutoMerge {
        /// Pull request number.
        #[arg(value_name = "PR")]
        pr_number: u32,

        /// Record what the disarm did as a comment on the PR. Silent when
        /// nothing was armed.
        #[arg(long)]
        audit_comment: bool,

        /// The explicit-hold label found on the PR (`loom:operator`,
        /// `loom:blocked`, `loom:operator-only`), if any. Shapes the audit
        /// comment's wording only — it never suppresses the disarm, which can
        /// only prevent a merge and therefore enforces a hold rather than
        /// undoing it. An empty value means "not held".
        #[arg(long, value_name = "LABEL")]
        hold: Option<String>,
    },

    /// `forge merge-method --repo <nwo> [--requested squash|merge|rebase]`
    /// (#8845) — resolve/validate the merge method `merge-pr.sh` should use,
    /// replacing its old unconditional `forge_detect_merge_method` call.
    /// With no `--requested`, preserves today's squash > merge > rebase
    /// auto-detect. With `--requested`, validates it against the repo's
    /// actual allowed strategies: prints the method and exits 0 when
    /// allowed, or names the allowed methods on stderr and exits 1 when not
    /// — never a silent fallback to squash. GitHub only; Gitea declines
    /// (exit 3, `EX_FORGE_DECLINED`) so `merge-pr.sh` falls back to its
    /// shell auto-detect.
    #[command(name = "merge-method")]
    MergeMethod {
        /// Repository, `owner/repo`.
        #[arg(long, value_name = "NWO")]
        repo: String,

        /// Explicitly requested merge method. Omit to auto-detect (today's
        /// unchanged behavior).
        #[arg(long, value_name = "METHOD")]
        requested: Option<String>,
    },
}

/// Handle `loom-daemon forge <issue|pr|auth|auto-merge>` (epic #4081 Phase 3,
/// family 3 — the native port of `loom-forge` / `loom-auto-merge`). Handlers
/// exec `gh` / exit the process directly, so this only returns `Err` when a
/// child process cannot be spawned. See `loom-daemon/src/forge_cmd.rs`.
pub(crate) fn handle_forge_command(action: ForgeAction) -> Result<()> {
    use loom_daemon::forge_cmd::{dispatch, ForgeCmd};
    let cmd = match action {
        ForgeAction::Issue { args } => ForgeCmd::Issue(args),
        ForgeAction::Pr { args } => ForgeCmd::Pr(args),
        ForgeAction::Auth { args } => ForgeCmd::Auth(args),
        ForgeAction::CheckOpenPr { issue } => ForgeCmd::CheckOpenPr { issue },
        ForgeAction::AutoMerge {
            pr_number,
            method,
            expected_head_sha,
            ..
        } => ForgeCmd::AutoMerge {
            pr: pr_number,
            method,
            expected_head_sha,
        },
        ForgeAction::DisableAutoMerge {
            pr_number,
            audit_comment,
            hold,
        } => ForgeCmd::DisableAutoMerge {
            pr: pr_number,
            audit_comment,
            hold,
        },
        ForgeAction::MergeMethod { repo, requested } => ForgeCmd::MergeMethod { repo, requested },
    };
    dispatch(cmd)
}
