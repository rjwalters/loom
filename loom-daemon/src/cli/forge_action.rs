//! `loom-daemon forge …`: the subcommand enum and its dispatcher.
//!
//! Moved verbatim out of `main.rs` (over `.loom/docs/file-size-policy.md`'s
//! threshold and frozen) and `cli/tokens.rs` (where the dispatcher had
//! landed for no reason but proximity), so new `forge` verbs cost `main.rs`
//! nothing: they are added here, beside the verbs they sit with.

use anyhow::Result;
use clap::Subcommand;
use std::path::PathBuf;

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

    /// `forge pr-congestion [--json] [--max-open N] [--max-points N]` — the
    /// #9063 **Phase 1** congestion signal, report-only: approved-queue
    /// depth (`loom:pr`), story points awaiting merge, and a path-disjoint
    /// bundle estimate over that queue.
    ///
    /// Exits `0` with the report whether or not it reads congested —
    /// congestion is information, not failure — `3` on Gitea, and `5`
    /// fail-closed when the fetch could not answer (never an empty-queue
    /// measurement). It never merges, creates, or closes anything; bundle
    /// execution is #9063 Phases 2–3 and stays operator-gated.
    #[command(name = "pr-congestion")]
    PrCongestion {
        /// Emit machine-readable JSON instead of the human report.
        #[arg(long)]
        json: bool,

        /// Congestion threshold: trip when the approved queue is strictly
        /// deeper than this (the issue's ">5 open PRs" example).
        #[arg(long, default_value_t = loom_daemon::forge_pr_congestion::DEFAULT_MAX_OPEN)]
        max_open: usize,

        /// Congestion threshold: trip when known story points awaiting merge
        /// strictly exceed this.
        #[arg(long, default_value_t = loom_daemon::forge_pr_congestion::DEFAULT_MAX_POINTS)]
        max_points: u32,
    },

    /// `forge wait-checks <PR|SHA> [--timeout SECS] [--required-only]`
    /// (#10330) — wait for a PR's (or commit's) CI with ETag'd REST reads
    /// (an unchanged poll is a free `304`) backing off 30s → 120s.
    ///
    /// Prints exactly one sentinel on stdout — `LOOM-CHECKS-GREEN <sha>`,
    /// `-NONE <sha>`, `-RED <sha> <names>`, `-TIMEOUT <sha> <pending>`,
    /// `-ERROR <reason>`, `-HEAD-MOVED <old> <new>` — and, on RED, one
    /// `<name>\t<url>\t<run_id>` line per failing check on stderr. Branch
    /// on the sentinel, not the exit code (0/1/2/3/4). `--timeout 0` takes
    /// one snapshot. See `loom_daemon::forge_wait_checks`.
    #[command(name = "wait-checks")]
    WaitChecks {
        /// A PR number, or a commit SHA (7-40 hex).
        #[arg(value_name = "PR|SHA")]
        selector: String,

        /// `owner/repo` (default: `LOOM_REPO`, else the `origin` remote).
        #[arg(long)]
        repo: Option<String>,

        /// Base branch for the required-context lookup in SHA mode
        /// (default: the repository's default branch).
        #[arg(long)]
        base: Option<String>,

        /// Seconds to wait before `LOOM-CHECKS-TIMEOUT`; `0` = one poll.
        #[arg(long, default_value_t = loom_daemon::forge_wait_checks::DEFAULT_TIMEOUT)]
        timeout: u64,

        /// Settle on the base branch's required contexts only.
        #[arg(long)]
        required_only: bool,

        /// First poll interval, seconds (env `LOOM_WAIT_CHECKS_MIN`, default 30).
        #[arg(long, value_name = "SECS")]
        min_interval: Option<u64>,

        /// Poll interval cap, seconds (env `LOOM_WAIT_CHECKS_MAX`, default 120).
        #[arg(long, value_name = "SECS")]
        max_interval: Option<u64>,
    },

    /// `forge check-claim <issue> [--force-claim]` — the aggregated
    /// pre-flight claim-CAS probe (#9453 Phase 1): "may I claim issue N
    /// **right now**?" Four legs, cheapest-first, short-circuiting on the
    /// first blocker — open linked PR (#4123), claim label
    /// (`loom:building`/`loom:reviewing`/`loom:treating`), fresh foreign
    /// lease (`LOOM_LEASE_TTL_MINUTES`), remote `feature/issue-N` branch.
    ///
    /// Exits `0` BLOCKED with the reason token on stdout (`OPEN_PR #X` /
    /// `BUILDING` / `LEASE_ALREADY_HELD <host> <sweep-id>` /
    /// `BRANCH_EXISTS feature/issue-N`), `1` verified safe to claim, `5`
    /// fail closed (a leg could not answer — NOT an all-clear), `3` Gitea
    /// decline. `--force-claim` overrides the label/lease/branch legs only —
    /// it never overrides the `OPEN_PR` leg.
    #[command(name = "check-claim")]
    CheckClaim {
        /// Issue number you are about to claim.
        #[arg(value_name = "ISSUE")]
        issue: u32,

        /// Override the claim-label, fresh-lease, and remote-branch legs.
        /// NEVER overrides the open-linked-PR leg (someone's submitted work
        /// is not a claim race) — and an unreadable open-PR leg still fails
        /// closed.
        #[arg(long)]
        force_claim: bool,
    },

    /// `forge check-branch <issue>` — the #9447 branch-collision hard-stop
    /// probe (#9453 Phase 4): does `feature/issue-N` already exist on
    /// `origin`? Wired into the pre-push fence immediately before a
    /// Builder's first `git push -u origin feature/issue-N` — a `0` means
    /// **hard-abort with `BRANCH_COLLISION`**, never create a suffix branch
    /// past it (the #9447 incident's exact failure mode).
    ///
    /// Exits `0` and prints the branch's last-commit timestamp (or its tip
    /// SHA when the commit is not locally reachable) when the branch already
    /// exists, `1` on a verified absence (safe to push), and `5` when the
    /// probe could not answer (fail closed — NOT an absence). Zero
    /// forge-API calls: `git ls-remote` is the git wire protocol, so this
    /// works identically on GitHub and Gitea.
    ///
    /// `--branch NAME` probes NAME instead of `feature/issue-N` (#10027).
    /// `--closed-pr-head` adds one forge read on an existing branch and exits
    /// `6` (closed PR number on stdout) when its tip is the head of a PR
    /// closed without merging, no open PR heads it, and the issue has no open
    /// linked PR — a preserved closed head, not a competing PR. Without it,
    /// zero forge-API calls.
    #[command(name = "check-branch")]
    CheckBranch {
        /// Issue number whose `feature/issue-N` branch you are about to push.
        #[arg(value_name = "ISSUE")]
        issue: u32,
        /// Branch to probe instead of the default `feature/issue-N`.
        #[arg(long, value_name = "BRANCH")]
        branch: Option<String>,
        /// Exit 6 instead of 0 when the existing branch is a closed-unmerged
        /// PR's preserved head and the issue has no open linked PR.
        #[arg(long)]
        closed_pr_head: bool,
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

    /// `forge tree-unchanged <base> <head>` (#9576) — did the tree change at
    /// all between two commits? Backed by GitHub's own
    /// `compare/{base}...{head}` reporting `files: []`, i.e. evidence, never a
    /// "shaped like a rebase" heuristic.
    ///
    /// Prints `TREE_UNCHANGED=1` (byte-identical trees) or `TREE_UNCHANGED=0`
    /// (a real content change) and exits 0 for both; exits 1 with nothing on
    /// stdout when the comparison could not be made. So a caller keys on the
    /// stdout line, and every failure mode — absent binary, a daemon predating
    /// this verb, a `gh` outage, a non-GitHub forge — collapses into the same
    /// fail-closed "assume the tree changed" arm.
    ///
    /// WHY IT EXISTS: this is the test #9124 added to the daemon's periodic
    /// verdict-invalidation pass so a tree-identical head move (the #8248
    /// guard's automated re-date commit, #8508) re-anchors the verdict instead
    /// of clearing it. `verdict-staleness-guard.sh`, the agent-side fast path,
    /// had no tree comparison at all and kept clearing those verdicts anyway
    /// (#9541, #9483). This verb is how the shell guard asks the *same*
    /// implementation ([`crate::forge_tree_unchanged`]) rather than growing a
    /// second copy of it.
    #[command(name = "tree-unchanged")]
    TreeUnchanged {
        /// The commit the verdict was rendered against (7-40 lowercase hex).
        #[arg(value_name = "BASE")]
        base: String,

        /// The commit to compare it with, normally the PR's current head.
        #[arg(value_name = "HEAD")]
        head: String,
    },

    /// `forge verdict-equivalent <pr> <reviewed> <head>` (#9416) — does a
    /// verdict rendered against `<reviewed>` still describe `<head>`, and by
    /// which equivalence? The superset of `tree-unchanged`: it asks that same
    /// tree-identical test first (#9124/#9576), then the clean-merge-of-base and
    /// rebase-patch-identical kinds #9416 adds. All three are recomputed from
    /// the repository — git objects and the forge's own compare endpoint — never
    /// from a comment or marker, and never from a commit message or the shape of
    /// a ref update.
    ///
    /// Prints `VERDICT_EQUIVALENT=1` plus `EQUIVALENCE_KIND=tree|clean-merge|
    /// rebase-patch-identical` when the verdict carries, or
    /// `VERDICT_EQUIVALENT=0` when it provably does not, exiting 0 for both;
    /// exits 1 with nothing on stdout when it could not be decided. So a caller
    /// keys on the `EQUIVALENCE_KIND=` line, and every failure mode — absent
    /// binary, a daemon predating this verb, a `gh` outage, a shallow clone, a
    /// `merge-tree` conflict, a non-GitHub forge — collapses into the same
    /// fail-closed "re-review" arm.
    ///
    /// Only the REVIEW is ever carried forward. CI re-runs against the new head
    /// regardless of which kind applied.
    #[command(name = "verdict-equivalent")]
    VerdictEquivalent {
        /// The PR whose base branch the two heads are compared against.
        #[arg(value_name = "PR")]
        pr_number: u32,

        /// The commit the verdict was rendered against (7-40 lowercase hex).
        #[arg(value_name = "REVIEWED")]
        reviewed: String,

        /// The PR's current head (7-40 lowercase hex).
        #[arg(value_name = "HEAD")]
        head: String,
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

    /// `forge merge-config` (#9287) — advisory, READ-ONLY check that
    /// `merge-pr.sh` can merge on the default branch at all. Computes the
    /// effective merge-method set (the repository's `allow_*` flags
    /// intersected with `allowed_merge_methods` of every ACTIVE ruleset on the
    /// branch) and warns when it is empty, when `required_linear_history`
    /// leaves only `merge`, or when it excludes the method `merge-pr.sh` will
    /// use. Silent when there is nothing to report (`--verbose` prints an OK
    /// line). A probe it cannot answer (403, no auth) prints "could not
    /// determine", never a finding. ALWAYS exits 0 and never writes a ruleset
    /// or repository setting. GitHub only; Gitea is skipped.
    #[command(name = "merge-config")]
    MergeConfig {
        /// Repository, `owner/repo`. Default: the repository of the CWD.
        #[arg(long, value_name = "NWO")]
        repo: Option<String>,

        /// Branch to check. Default: the repository's default branch.
        #[arg(long, value_name = "BRANCH")]
        branch: Option<String>,

        /// Check this merge method instead of `merge-pr.sh`'s auto-detect.
        #[arg(long, value_name = "METHOD", value_parser = ["merge", "squash", "rebase"])]
        method: Option<String>,

        /// Also print a one-line OK summary when there is nothing to report.
        #[arg(long)]
        verbose: bool,
    },

    /// `forge merge-queue <mode|preflight|status|enqueue|dequeue>` (#10255)
    /// — dormant merge-queue controls (`champion.mergeMode`, default
    /// `direct`). Exit: 0 ok, 1 failed, 2 invalid config, 3 undetermined,
    /// 4 refused before any forge call.
    #[command(name = "merge-queue")]
    MergeQueue {
        #[command(subcommand)]
        action: super::forge_merge_queue_cmd::MergeQueueAction,
    },

    /// `forge token --repo <nwo> [--access read|write] [--force]` (#9537) —
    /// the single entry point for "which App's token should this call use".
    /// `read` returns the repo's reader App token (deterministic per repo),
    /// falling back to the writer when there is no usable reader; `write`
    /// always returns the writer's. Prints one JSON line in the same shape as
    /// `github-app-token.sh get-token` plus `access`, `slug` and, on a
    /// fallback, `fallback_reason`; always exits 0 (read `status`).
    Token {
        /// Repository, `owner/repo` (selects the installation and the reader).
        #[arg(long, value_name = "NWO")]
        repo: String,
        /// `read` or `write` (default `write`: the attributed identity).
        #[arg(long, value_name = "ACCESS", default_value = "write")]
        access: String,
        /// Bypass the minter's token cache.
        #[arg(long)]
        force: bool,
    },

    /// `forge is-fleet <login>` (#9537) — whether `login` (any spelling:
    /// `x`, `x[bot]`, `app/x`) is one of this fleet's App identities. Prints
    /// its role (`writer`, `reader`, `legacy`, `default`) and exits 0; exits
    /// 1 silently when it is not. Replaces every hardcoded fleet login in
    /// scripts.
    #[command(name = "is-fleet")]
    IsFleet {
        /// The login to test.
        #[arg(value_name = "LOGIN")]
        login: String,
    },

    /// `forge trusted-comments [--self-login L]` (#9548) — filter a comment
    /// listing on stdin (REST `user.login`/`author_association`, or `gh
    /// --json` `author.login`/`authorAssociation`) down to the comments whose
    /// author Loom trusts as a control-signal source: a repo insider by
    /// association, one of THIS fleet's Apps (App-spelled, exact match), this
    /// daemon's own identity, or `forge.trustedCommenters`. Prints the same
    /// JSON shape; exits 1 with nothing on stdout on unparseable input. `gh
    /// --json` spells an App as a bare login, so fleet-authored markers need
    /// the REST listing. See `.loom/docs/comment-trust.md`.
    ///
    /// `--fetch N` reads issue/PR N's REST listing itself (one call for role
    /// prompts and scripts; exits 1 when it cannot be read). `--with-body`
    /// puts the issue/PR itself first, so its body survives only when its
    /// author is trusted; `--gh-shape` prints `gh --json comments` fields.
    #[command(name = "trusted-comments")]
    TrustedComments {
        /// This caller's own login, trusted with the same account kind
        /// (default: the configured writer App, `<slug>[bot]`).
        #[arg(long, value_name = "LOGIN")]
        self_login: Option<String>,
        /// Fetch issue/PR N's comments (REST) instead of reading stdin.
        #[arg(long, value_name = "N")]
        fetch: Option<u64>,
        /// With `--fetch`: `owner/name` (default: the cwd's repo).
        #[arg(long, value_name = "NWO", requires = "fetch")]
        repo: Option<String>,
        /// With `--fetch`: the issue/PR body first, kept only if trusted.
        #[arg(long, requires = "fetch")]
        with_body: bool,
        /// Print `{author:{login},authorAssociation,body,createdAt}` items.
        #[arg(long)]
        gh_shape: bool,
    },

    /// `forge verdict-stale-notice --label L --marker-sha M --head-sha H
    /// [--source S]` (#9709) — print the stale-verdict audit comment for a
    /// `M -> H` invalidation, rendered by the SAME template the daemon's pass
    /// posts. Reads the PR's RAW (unfiltered) REST comment listing on stdin:
    /// when a marker newer than the newest trusted one was dropped as
    /// untrusted, the notice names its login and `author_association` and
    /// points at `forge.trustedCommenters` instead of asserting a head move.
    /// Unreadable stdin yields the plain wording. Exits 1 on an unknown label.
    #[command(name = "verdict-stale-notice")]
    VerdictStaleNotice {
        /// The verdict label being cleared (`loom:pr` / `loom:changes-requested`).
        #[arg(long)]
        label: String,
        /// The SHA the newest trusted marker records.
        #[arg(long)]
        marker_sha: String,
        /// The PR's current head SHA.
        #[arg(long)]
        head_sha: String,
        /// The attribution footer's source.
        #[arg(long, default_value = "verdict-staleness-guard.sh")]
        source: String,
    },

    /// `forge may-write [--repo OWNER/REPO]` (#9548) — may this installation
    /// write (comment, label, merge, lease) to the repository? Yes only when
    /// it is managed here (origin of a registered workspace or of this Loom
    /// checkout) and this process's credential has WRITE (cached probe). With
    /// no `--repo`, the target is what `gh` resolves from the checkout, which
    /// must be its `origin` (gh prefers an `upstream` remote). Prints the
    /// OWNER/REPO to name on the write and exits 0; exits 1 with the reason on
    /// stderr. See `.loom/docs/comment-trust.md`.
    #[command(name = "may-write")]
    MayWrite {
        /// Repository to vet (default: this checkout's gh target).
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,
    },

    /// `forge calls [--since 1h|3h] [--by bucket|caller|role|repo]` (W1) —
    /// this host's forge-call sink per billed GitHub bucket (or caller, role,
    /// repo) beside the bucket book's readings. Reads local files only.
    Calls(super::forge_calls_cmd::CallsArgs),

    /// `forge identities [--json]` (#9537) — the resolved roster (writer,
    /// readers, legacy logins) and, per reader, each published token's owner
    /// and expiry.
    Identities {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },

    /// `forge comment <number> (--body TEXT | --body-file PATH)
    /// [--repo OWNER/REPO] [--pr]` — the #9772 comment chokepoint as a verb:
    /// appends the dashboard link (`loom:dashboard-link`, #9772) and POSTs to
    /// `repos/<owner>/<repo>/issues/<number>/comments`. A PR IS an issue for
    /// comments; `--pr` only picks `/pull/N` over `/issues/N` in the link.
    /// `--repo` defaults to the current checkout's `origin` remote. GitHub
    /// only, like every daemon comment path (`gh` REST).
    #[command(name = "comment")]
    Comment {
        /// Issue or PR number to comment on (post path; omit when
        /// `--patch-created` is given).
        #[arg(value_name = "NUMBER")]
        number: Option<u64>,

        /// Target `owner/repo`; omitted resolves from the origin remote.
        #[arg(long, value_name = "OWNER/REPO")]
        repo: Option<String>,

        /// Comment body as literal text.
        #[arg(long, value_name = "TEXT")]
        body: Option<String>,

        /// Read the body from PATH ("-" = stdin). Mutually exclusive with
        /// `--body` (`--body @path` does NOT expand — see the
        /// comment-body-literal-path rule).
        #[arg(long, value_name = "PATH")]
        body_file: Option<PathBuf>,

        /// The number names a pull request (link says `/pull/N`).
        #[arg(long)]
        pr: bool,

        /// Don't post: append the footer to this CREATED object's existing
        /// body instead (idempotent) — the post-create step
        /// create-issue.sh / create-pr.sh run right after a successful
        /// create, when the number exists only inside the URL. Mutually
        /// exclusive with NUMBER.
        #[arg(long, value_name = "URL|OWNER/REPO#N", conflicts_with = "number")]
        patch_created: Option<String>,
    },

    /// `forge egress assert|doctor|policy` (#9984) — will this process's `gh`
    /// reach the mandated API origin? Exit 0 aligned / 1 findings / 2
    /// verification incomplete; no policy configured ⇒ 0. Never a `gh`
    /// passthrough. See `defaults/docs/forge-egress.md`.
    Egress {
        #[command(subcommand)]
        action: super::forge_egress_cmd::EgressAction,
    },

    /// `forge dashboard-link <owner/repo> <number> [--pr]` — print the exact
    /// dashboard footer (#9772) for `number` in `owner/repo`, byte-for-byte
    /// as `forge comment` would append it. The shell twin's format-pinning
    /// test (#9774) asserts its bash implementation against this output, so
    /// the two implementations cannot drift.
    #[command(name = "dashboard-link")]
    DashboardLink {
        /// Target `owner/repo`.
        #[arg(value_name = "OWNER/REPO")]
        repo: String,

        /// Issue or PR number.
        #[arg(value_name = "NUMBER")]
        number: u64,

        /// The number names a pull request (link says `/pull/N`).
        #[arg(long)]
        pr: bool,

        /// Optional body to prepend, so the pinning test can compare a full
        /// `body + footer` document byte-for-byte.
        #[arg(long, value_name = "TEXT")]
        body: Option<String>,
    },
}

/// Handle `loom-daemon forge <issue|pr|auth|auto-merge>` (epic #4081 Phase 3,
/// family 3 — the native port of `loom-forge` / `loom-auto-merge`). Handlers
/// exec `gh` / exit the process directly, so this only returns `Err` when a
/// child process cannot be spawned. See `loom-daemon/src/forge_cmd.rs`.
pub(crate) fn handle_forge_command(action: ForgeAction) -> Result<()> {
    use loom_daemon::forge_cmd::{dispatch, ForgeCmd};
    // #9548: the verbs that write are vetted before they run. `issue`/`pr`
    // pass straight through to `gh`, which would otherwise pick an `upstream`
    // remote over `origin`; the arm/disarm verbs mutate a PR.
    if let Some(repo) = write_target(&action) {
        let cwd = std::env::current_dir()?;
        if let loom_daemon::write_scope::Verdict::Deny(why) =
            loom_daemon::write_scope::may_write_from(&cwd, repo.as_deref())
        {
            eprintln!("loom-daemon forge: refusing the write (#9548): {why}");
            std::process::exit(1);
        }
    }
    let cmd = match action {
        ForgeAction::Token {
            repo,
            access,
            force,
        } => return super::forge_identity_cmd::token(&repo, &access, force),
        ForgeAction::Egress { action } => return super::forge_egress_cmd::handle(action),
        ForgeAction::MergeQueue { action } => super::forge_merge_queue_cmd::run(action),
        ForgeAction::IsFleet { login } => return super::forge_identity_cmd::is_fleet(&login),
        ForgeAction::Identities { json } => return super::forge_identity_cmd::identities(json),
        ForgeAction::Calls(args) => return super::forge_calls_cmd::handle(args),
        ForgeAction::MayWrite { repo } => return super::forge_identity_cmd::may_write(repo),
        ForgeAction::DashboardLink {
            repo,
            number,
            pr,
            body,
        } => {
            let (owner, name) = repo
                .split_once('/')
                .ok_or_else(|| anyhow::anyhow!("--repo must be OWNER/REPO, got {repo:?}"))?;
            let nwo = format!("{owner}/{name}");
            print!(
                "{}",
                loom_daemon::forge_comment::build_dashboard_footer(
                    &loom_daemon::forge_comment::dashboard_base_url(),
                    &nwo,
                    number,
                    pr,
                    body.as_deref().unwrap_or(""),
                )
            );
            return Ok(());
        }
        ForgeAction::Comment {
            number,
            repo,
            body,
            body_file,
            pr,
            patch_created,
        } => {
            return loom_daemon::forge_comment::cli_entrypoint(
                loom_daemon::forge_comment::CommentArgs {
                    number,
                    repo,
                    body,
                    body_file,
                    is_pr: pr,
                    patch_created,
                },
            );
        }
        ForgeAction::TrustedComments {
            self_login,
            fetch,
            repo,
            with_body,
            gh_shape,
        } => {
            let fetch = fetch.map(|n| (n, repo, with_body));
            return super::forge_identity_cmd::trusted_comments(self_login, fetch, gh_shape);
        }
        ForgeAction::VerdictStaleNotice {
            label,
            marker_sha,
            head_sha,
            source,
        } => {
            return super::forge_identity_cmd::verdict_stale_notice(
                &label,
                &marker_sha,
                &head_sha,
                &source,
            );
        }
        ForgeAction::Issue { args } => ForgeCmd::Issue(args),
        ForgeAction::Pr { args } => ForgeCmd::Pr(args),
        ForgeAction::Auth { args } => ForgeCmd::Auth(args),
        ForgeAction::WaitChecks {
            selector,
            repo,
            base,
            timeout,
            required_only,
            min_interval,
            max_interval,
        } => loom_daemon::forge_wait_checks::cli_entrypoint(
            loom_daemon::forge_wait_checks::WaitArgs {
                selector,
                repo,
                base,
                timeout,
                required_only,
                min_interval,
                max_interval,
            },
        ),
        ForgeAction::CheckOpenPr { issue } => ForgeCmd::CheckOpenPr { issue },
        ForgeAction::PrCongestion {
            json,
            max_open,
            max_points,
        } => ForgeCmd::PrCongestion {
            json,
            max_open,
            max_points,
        },
        ForgeAction::CheckClaim { issue, force_claim } => {
            ForgeCmd::CheckClaim { issue, force_claim }
        }
        ForgeAction::CheckBranch {
            issue,
            branch,
            closed_pr_head,
        } => ForgeCmd::CheckBranch(loom_daemon::forge_check_branch::CheckBranchArgs {
            issue,
            branch,
            closed_pr_head,
        }),
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
        ForgeAction::TreeUnchanged { base, head } => ForgeCmd::TreeUnchanged { base, head },
        ForgeAction::VerdictEquivalent {
            pr_number,
            reviewed,
            head,
        } => ForgeCmd::VerdictEquivalent {
            pr: pr_number,
            reviewed,
            head,
        },
        ForgeAction::MergeMethod { repo, requested } => ForgeCmd::MergeMethod { repo, requested },
        ForgeAction::MergeConfig {
            repo,
            branch,
            method,
            verbose,
        } => ForgeCmd::MergeConfig {
            repo,
            branch,
            method,
            verbose,
        },
    };
    dispatch(cmd)
}

/// `gh issue` / `gh pr` operations that change the forge.
const WRITE_OPS: &[&str] = &[
    "comment", "edit", "close", "reopen", "create", "delete", "lock", "unlock", "pin", "unpin",
    "transfer", "merge", "ready", "review", "develop",
];

/// `Some(repo)` when `action` writes (`repo` is its `--repo`/`-R`, if any).
fn write_target(action: &ForgeAction) -> Option<Option<String>> {
    match action {
        ForgeAction::Issue { args } | ForgeAction::Pr { args } => {
            let op = args.iter().find(|a| !a.starts_with('-'))?;
            WRITE_OPS.contains(&op.as_str()).then(|| repo_flag(args))
        }
        ForgeAction::AutoMerge { .. } | ForgeAction::DisableAutoMerge { .. } => Some(None),
        // #9772: `forge comment` posts, so it is vetted like the other
        // write verbs — its `--repo` is exactly the `Option<String>` shape
        // `may_write_from` wants.
        ForgeAction::Comment { repo, .. } => Some(repo.clone()),
        // #10255: the queue mutations write (dormant today, vetted anyway).
        ForgeAction::MergeQueue {
            action:
                super::forge_merge_queue_cmd::MergeQueueAction::Enqueue { repo, .. }
                | super::forge_merge_queue_cmd::MergeQueueAction::Dequeue { repo, .. },
        } => Some(repo.clone()),
        _ => None,
    }
}

/// The value of `--repo X`, `--repo=X`, `-R X` or `-RX` in passthrough args.
fn repo_flag(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--repo" || a == "-R" {
            return it.next().cloned();
        }
        if let Some(v) = a.strip_prefix("--repo=").or_else(|| a.strip_prefix("-R")) {
            if !v.is_empty() {
                return Some(v.trim_start_matches('=').to_string());
            }
        }
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod write_target_tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn passthrough_writes_are_vetted_and_reads_are_not() {
        let comment = ForgeAction::Issue {
            args: args(&["comment", "7", "--body", "x", "-R", "acme/w"]),
        };
        assert_eq!(write_target(&comment), Some(Some("acme/w".into())));
        let merge = ForgeAction::Pr {
            args: args(&["merge", "7", "--repo=acme/w"]),
        };
        assert_eq!(write_target(&merge), Some(Some("acme/w".into())));
        let edit = ForgeAction::Pr {
            args: args(&["edit", "7", "--add-label", "x"]),
        };
        assert_eq!(write_target(&edit), Some(None), "no --repo: vet gh's own target");
        for read in [&["view", "7"][..], &["list", "--label", "x"], &["status"]] {
            let a = ForgeAction::Issue { args: args(read) };
            assert_eq!(write_target(&a), None, "{read:?} is a read");
        }
        let disarm = ForgeAction::DisableAutoMerge {
            pr_number: 7,
            audit_comment: true,
            hold: None,
        };
        assert_eq!(write_target(&disarm), Some(None));
        assert_eq!(write_target(&ForgeAction::IsFleet { login: "x".into() }), None);
    }
}
