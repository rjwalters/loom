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
    /// Supervised persistent-container transport backing spawn-codex.sh.
    #[command(subcommand)]
    SessionExec(loom_daemon::session_exec::SessionExecCommand),
    /// Private workspace endpoint used inside a session container.
    #[command(subcommand)]
    PrivateWorkspace(loom_daemon::tokens_pool::private_workspace::WorkerCommand),
    /// Durable phase completion markers and trace observations (#8525).
    SweepCheckpoint(super::sweep_checkpoint::SweepCheckpointArgs),

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

    /// Why an already-resolved release has no artifact for a target (#8654):
    /// the #8515 age + asset-count classification, for
    /// `loom-daemon-update.sh`'s forced `--fetch` refusal. Exit 0 + one line
    /// = the reason, 1 = the release does carry the artifact. Optional to its
    /// caller — an older binary lacking it degrades to the flat reason.
    ReleaseExplain(super::release_explain::ReleaseExplainArgs),

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

    /// `worktree.sh remove <N>` (#8195, slice 3): the operator-facing
    /// single-worktree removal verb, and the destructive half of that script —
    /// `git worktree remove --force`, the #5177 `rm -rf` fallback, the #7239
    /// cargo-target-dir reclaim and the squash-aware `git branch -D` all hang
    /// off it. Exit 0 = removed or an idempotent no-op, 1 = refused (nothing
    /// deleted) or the removal failed.
    WorktreeRemove(super::worktree_remove::WorktreeRemoveArgs),

    /// `worktree.sh`'s post-`git worktree add` symlink provisioning (#8195,
    /// slice 4): root and nested `node_modules`, `worktree.linkPaths`,
    /// `.mcp.json`, and the `info/exclude` entry each one needs so `git add
    /// -A` cannot stage it (#3528/#5474). The part of the create path that is
    /// all path interpolation — four `ln -s "$src" "$dst"` pairs and a
    /// `find | read` loop — which is #7858's class. Exit 0 always: this is
    /// best-effort by contract and the worktree already exists.
    WorktreeLink(super::worktree_link::WorktreeLinkArgs),

    /// `worktree.sh`'s crash-debris pre-flight (#8195, slice 5): the stale
    /// `index.lock`/`HEAD.lock`/`gitdir.lock` sweep and the **orphan guard**
    /// that `rm -rf`s an `issue-<N>` dir `git worktree list` does not know
    /// about. The guard whose false answer deleted a LIVE worktree twice over
    /// (#7858/#7849 — a porcelain path split on whitespace, and a candidate
    /// resolved logically instead of physically). Exit 0 always: both shell
    /// call sites already discard the status with `|| true`.
    WorktreeCleanup(super::worktree_cleanup::WorktreeCleanupArgs),

    /// `lib/worktree-race-rescue.sh`'s `loom_worktree_reset_or_rescue` (#8195,
    /// slice 6): the guard in front of `worktree.sh`'s stale-worktree
    /// `git reset --hard` — refuse while a live process holds the worktree
    /// (#7463), refuse when it gained commits, and capture foreign uncommitted
    /// tracked changes to a `.snapshots/` patch before resetting rather than
    /// discarding them (#6706/#6334, the #6320 incident). Exit 0 = reset,
    /// 1 = refused and nothing changed, 2 = the reset itself failed.
    WorktreeReset(super::worktree_reset::WorktreeResetArgs),

    /// `worktree.sh`'s `_handle_feature_branch_in_main_worktree` (#8195 slice
    /// 7): the recovery `_try_worktree_add` falls into when `git worktree
    /// add` refuses because the target branch is already checked out in the
    /// main workspace. The one arm of the create path that is pure string
    /// parsing of an arbitrary git error message — #7858's class again. Exit
    /// 0 = handled (no retry), 1 = not this error, 2 = auto-recovered, retry.
    WorktreeBranchConflict(super::worktree_branch_conflict::WorktreeBranchConflictArgs),

    /// `worktree.sh`'s post-`git worktree add` submodule initialization
    /// (#8195 slice 8): the `git submodule status | grep '^-' | awk '{print
    /// $2}'` work list — #7858's whitespace-split-path class, on the string
    /// that is then used as BOTH a `--reference` directory and a git
    /// pathspec — plus a `timeout(1)` that does not exist on a stock macOS, a
    /// `--reference` fast path that never once fired, and a `$$`-keyed
    /// failure flag in world-writable `/tmp`. Exit 0 always: best-effort by
    /// contract, and the worktree already exists.
    WorktreeSubmodules(super::worktree_submodules::WorktreeSubmodulesArgs),

    /// `worktree.sh`'s upstream-tracking correction and stale-worktree drift
    /// report (#8195 slice 9) — which the script carried as TWO hand-
    /// maintained copies of the same fix, one per create-path arm
    /// (#6095/#6100 on branch reuse, #6257/#6291 on the registered-worktree
    /// fast path), differing only in a noun and in whether the
    /// behind-the-pushed-tip report runs. Both defects shipped silently:
    /// their failure mode is a message that is never printed and an upstream
    /// that is never corrected. Exit 0 always — advisory repair; a repo that
    /// cannot be fetched from must not block worktree creation.
    WorktreeUpstream(super::worktree_upstream::WorktreeUpstreamArgs),

    /// `claude-wrapper.sh`'s retry/rotation classifiers (#8037): retry vs give
    /// up, rotate, mark a credential dead, and the backoff curve. Exit 0 when
    /// the predicate holds, 1 when it does not — an answer, not an error.
    #[command(subcommand)]
    RetryClassify(super::retry_classify::RetryClassifyCommand),
    /// Provenance stamps (#9027, D33): commit trailers, the `commit-msg`
    /// stamper and the PR-body `loom:provenance` marker.
    #[command(subcommand)]
    Provenance(super::provenance::ProvenanceCommand),
    /// Host-side autonomy-loss detector (#8086), backing
    /// `loom-daemon-watchdog.sh`. Run by a launchd/systemd timer on a
    /// `StartInterval` cadence, so it owns no long-lived process.
    DaemonWatchdog(super::watchdog::WatchdogArgs),

    /// Safe start wrapper for the raw `loom-daemon` process (#8087), backing
    /// `loom-daemon-start.sh`. Unlike every other port here it STARTS A
    /// PROCESS, and it must bring that process up with byte-identical
    /// autonomy flags to what the shell computed — see
    /// `daemon_start`'s module doc for why "did it start?" is not a test of
    /// that.
    DaemonStart(super::daemon_start::DaemonStartArgs),

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

    /// The premise gate (#8396), one stage before Curator: is this issue in
    /// the gated population, and does a consistent premise record exist for
    /// it? Backs `premise-check.sh`. Exit 0 proceed, 10 record required, 11
    /// route to `loom:operator-decision`, 12 record malformed, 13 premise
    /// false, 1 could not run — and 1 must be treated as 10, never as 0. Not
    /// a port either: same frozen-`main.rs` reason as `shell-budget` above.
    PremiseCheck(super::premise_check::PremiseCheckArgs),

    /// `reconcile-stack.sh`'s rebase planner and executor (#8583): fetch and
    /// PIN the remote default-branch tip, route to the worktree holding the
    /// child branch, resolve the parent ref (with the #7982 pin fallback and
    /// its #8010 ancestry check), then replay only the child's own commits
    /// onto the pinned commit. Exit 0 planned/rebased, 1 a prerequisite
    /// refused with nothing mutated, 2 the rebase itself failed.
    ReconcileStack(super::reconcile_stack::ReconcileStackArgs),

    /// Generate `.agents/skills/loom-<name>/SKILL.md` from every
    /// `defaults/roles/<name>.md` role prompt (#8673) — the cross-vendor
    /// skill-discovery surface Codex, Kimi Code, Mistral Vibe, and Grok read
    /// natively. Not a port either: brand-new logic, native from the start
    /// per the shell-language policy, backing
    /// `generate-agent-skills.sh`'s Shape-A stub.
    GenerateAgentSkills(super::agent_skills::AgentSkillsArgs),

    /// `verify-proposal-refs.sh`'s line-range check (#8656): resolve a cited
    /// path against a rev, FOLLOWING a `120000` (symlink) tree entry to the
    /// document it points at, and answer the range question against that
    /// document. Since #7842 every `.loom/docs/*.md` with a `defaults/docs/`
    /// counterpart is such a link, so the script's old `git show <rev>:<path>
    /// | wc -l` measured the link-target STRING and reported every in-range
    /// citation as a miss — on a script that BLOCKS FILING. Ported out of the
    /// `contract`-category script per the shell language policy.
    GitBlobLines(super::git_blob_lines::GitBlobLinesArgs),

    /// The fleet singleton-job captain gate (#8848), shell-facing half: is
    /// THIS host the one declared to run `<job-name>`? Exit 0 arm, 3 another
    /// host is the captain, 4 no `fleet.captain` declared at all — three
    /// codes, not two, so a never-declared/typo'd captain is distinguishable
    /// from a routine "not my turn" instead of silently leaving every
    /// singleton unarmed fleet-wide. Replaces the fail-closed host gate each
    /// singleton's schedule wrapper used to hand-roll. Not a port: brand-new
    /// logic, native from the start per the shell-language policy.
    FleetCaptain(super::fleet_captain_cmd::FleetCaptainArgs),

    /// The per-role tool-restriction allowlist (#8322, for #8256), shell-facing
    /// half: the `--disallowedTools` spec list `spawn-claude.sh` injects, and
    /// the "is this role restricted" predicate `spawn-codex.sh` needs to warn
    /// that the guard hook is the ONLY enforcement on its path. Ported out of
    /// both `contract`-category scripts because inlining it there is exactly
    /// the portable-shell growth `shell-budget --check` refuses. Exit 0 =
    /// a restriction applies, 1 = none does — an answer, not an error, landing
    /// on the same no-op branch as an unavailable binary.
    #[command(subcommand)]
    RoleToolPolicy(super::role_tool_policy::RoleToolPolicyCommand),

    /// Tier-3 "generic passthrough" launch-shape resolution (#8671): reads a
    /// runtime capability manifest's `launch` object and renders it as
    /// eval-ready shell defaults. Backs `spawn-generic-launch.sh`. Exit 0
    /// resolved, 1 no manifest reachable (soft), 78 (`EX_CONFIG`) malformed
    /// manifest or an unrecognized `launch` key.
    RuntimeLaunchEnv(super::runtime_launch_cmd::RuntimeLaunchEnvArgs),

    /// The fleet-wide `loom:blocked` re-check (#8927): every open
    /// `loom:blocked` issue whose cited blocker has since closed/merged (a
    /// stale block), plus every one carrying the label with no parseable
    /// blocker reference at all (an undocumented block). The fourth pre-wave
    /// advisory check, backing `check-stale-blocked.sh`. Strictly read-only,
    /// **always exit 0** — it reports, it never relabels. Not a port: brand-new
    /// logic, native from the start per the shell-language policy.
    CheckStaleBlocked(super::stale_blocked::StaleBlockedArgs),
}

impl ScriptPortCommand {
    /// Never returns: every arm exits with its subcommand's own code, which
    /// the stubs' callers branch on.
    pub(crate) fn run(self) -> Result<()> {
        match self {
            ScriptPortCommand::SessionExec(args) => args.run(),
            ScriptPortCommand::PrivateWorkspace(args) => args.run(),
            ScriptPortCommand::SweepCheckpoint(args) => args.run(),
            ScriptPortCommand::DepClassify(cmd) => cmd.run(),
            ScriptPortCommand::DepRecheckFingerprint(cmd) => cmd.run(),
            ScriptPortCommand::ReleaseFetch(args) => args.run(),
            ScriptPortCommand::ReleaseResolve(args) => args.run(),
            ScriptPortCommand::ReleaseExplain(args) => args.run(),
            ScriptPortCommand::MergePr(cmd) => cmd.run(),
            ScriptPortCommand::ShellBudget(args) => args.run(),
            ScriptPortCommand::MergePrRefs(cmd) => cmd.run(),
            ScriptPortCommand::WorktreeLock(cmd) => cmd.run(),
            ScriptPortCommand::WorktreeWip(cmd) => cmd.run(),
            ScriptPortCommand::WorktreeRemove(args) => args.run(),
            ScriptPortCommand::WorktreeLink(args) => args.run(),
            ScriptPortCommand::WorktreeCleanup(args) => args.run(),
            ScriptPortCommand::WorktreeReset(args) => args.run(),
            ScriptPortCommand::WorktreeBranchConflict(args) => args.run(),
            ScriptPortCommand::WorktreeSubmodules(args) => args.run(),
            ScriptPortCommand::WorktreeUpstream(args) => args.run(),
            ScriptPortCommand::RetryClassify(cmd) => cmd.run(),
            ScriptPortCommand::Provenance(cmd) => cmd.run(),
            ScriptPortCommand::DaemonWatchdog(args) => args.run(),
            ScriptPortCommand::DaemonStart(args) => args.run(),
            ScriptPortCommand::SkipLabels(args) => args.run(),
            ScriptPortCommand::WorktreeState(cmd) => cmd.run(),
            ScriptPortCommand::DuplicateScan(args) => args.run(),
            ScriptPortCommand::PremiseCheck(args) => args.run(),
            ScriptPortCommand::ReconcileStack(args) => args.run(),
            ScriptPortCommand::GenerateAgentSkills(args) => args.run(),
            ScriptPortCommand::GitBlobLines(args) => args.run(),
            ScriptPortCommand::FleetCaptain(args) => args.run(),
            ScriptPortCommand::RoleToolPolicy(cmd) => cmd.run(),
            ScriptPortCommand::RuntimeLaunchEnv(args) => args.run(),
            ScriptPortCommand::CheckStaleBlocked(args) => args.run(),
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

    /// The automated remedy for a merge the #8248 freshness guard blocked:
    /// first re-run the workflow runs holding the stale required checks IN
    /// PLACE (#8914, needs Actions: write; no commit, verdict kept), else push
    /// a tree-identical no-op commit so CI re-dates every check (#8508).
    /// Exit 5 = re-ran in place and fresh (only with --allow-proceed), 0 =
    /// re-running in place / fresh without opt-in / pushed (re-queue), 3 =
    /// head already moved (not a failure, re-evaluate fresh), 4 = push remedy
    /// already spent on this head, so the PR was escalated to a durable
    /// `loom:operator` hold, 1 = could not produce fresh evidence.
    RedateChecks(super::merge_pr_redate::RedateChecksArgs),

    /// The pre-merge `loom:pr` review-signal guard (#7419): refuse a merge
    /// whose current head does not carry `loom:pr`, unless
    /// `--allow-unapproved` asserts responsibility. Exit 0 = present or
    /// overridden (see stdout for which), 1 = absent with no override.
    LoomPrGuard(super::merge_pr_loom_pr_guard::LoomPrGuardArgs),

    /// The `champion:hold-state` staleness WARNING (#7419 AC #3): the other
    /// half of `loom-pr-guard`'s story, fired only when `loom:pr` IS present
    /// and Champion's recorded hold head is not the head about to merge.
    /// Advisory, never a gate — always exits 0 (stdout is either the
    /// `LOOM-HOLD-STATE-CLEAN` sentinel or the warning), except exit 2 when
    /// the comment stream could not be read at all.
    HoldState(super::merge_pr_hold_state::HoldStateArgs),

    /// `_maybe_delete_local_branch` (#4100/#5015/#7812): the squash-aware
    /// local-branch delete rule, now shared verbatim with `worktree.sh
    /// remove` (#8195 slice 3) instead of duplicated. Always exits 0 — a
    /// branch that could not be deleted is a `WARNING` line, not a failure —
    /// and prints one `LEVEL<TAB>message` line per decision for the caller to
    /// replay through its own logging (see `cli::merge_pr_delete_branch`).
    DeleteBranch(super::merge_pr_delete_branch::DeleteBranchArgs),

    /// Decide ONE zero-row check-runs poll of `--auto`'s settle wait (#9091):
    /// settle now, keep waiting, or report the whole wait spent. Bounded only
    /// when the base branch requires no status-check contexts; a lookup that
    /// errors, or a required context present, keeps #6169's full wait. Always
    /// exits 0 with one sentinel-led line — see `cli::merge_pr_zero_checks`.
    ZeroChecksSettle(super::merge_pr_zero_checks::ZeroChecksSettleArgs),
}

impl MergePrCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            MergePrCommand::VerdictContradiction(args) => args.run(),
            MergePrCommand::StaleChecks(args) => args.run(),
            MergePrCommand::HeadSyncRetry(args) => args.run(),
            MergePrCommand::RedateChecks(args) => args.run(),
            MergePrCommand::LoomPrGuard(args) => args.run(),
            MergePrCommand::HoldState(args) => args.run(),
            MergePrCommand::DeleteBranch(args) => args.run(),
            MergePrCommand::ZeroChecksSettle(args) => args.run(),
        }
    }
}
