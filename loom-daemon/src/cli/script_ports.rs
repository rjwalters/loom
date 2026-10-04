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
    /// Ordered PR work for Judge, Doctor and Champion.
    PrQueue(super::pr_queue::PrQueueArgs),
    /// Supervised persistent-container transport backing spawn-codex.sh.
    #[command(subcommand)]
    SessionExec(loom_daemon::session_exec::SessionExecCommand),
    /// Private workspace endpoint used inside a session container.
    #[command(subcommand)]
    PrivateWorkspace(loom_daemon::tokens_pool::private_workspace::WorkerCommand),
    /// Readiness of Loom's managed Codex hook, behind
    /// `provision-codex-hooks.sh verify` (#9390).
    #[command(subcommand)]
    CodexHooks(super::codex_hooks::CodexHooksCommand),
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

    /// Whether an exit-0 Codex session ran nothing because its sandbox
    /// refused every shell command (#10003). Backs `spawn-codex.sh`'s
    /// terminal classification. Exit 0 + one `shape=…` line = a no-op, 1 = it
    /// ran something. Optional to its caller — an older binary lacking it
    /// leaves the session classified as before.
    CodexSandboxNoop(super::codex_sandbox_noop_cli::CodexSandboxNoopArgs),

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

    /// Seed and evaluate the ETA estimators (#9325, Phase 2 of #9289).
    ///
    /// `eta backfill` populates the stage-sample journal from `pr-latency`
    /// history so the heuristics have a baseline on day one; `eta backtest`
    /// replays a heuristic against real outcomes leak-free and scores it.
    ///
    /// Not a script port either: it lives here for the same
    /// frozen-`main.rs` reason as `shell-budget` above, which is also what
    /// keeps `eta` a real nested subcommand (`loom-daemon eta backfill`,
    /// not a flattened top-level `backfill`) at zero cost to `main.rs`.
    #[command(subcommand)]
    Eta(super::eta_cmd::EtaCommand),

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

    /// `worktree.sh`/`spawn-claude.sh`/`lib/cargo-target-dir.sh`'s per-worktree
    /// cargo target dir (#8458): provision one and record its marker, derive
    /// the path a not-yet-created worktree would get, or answer the two
    /// removal-side predicates (`is-attributable`, `marker`). The two creation
    /// verbs exit 0 always — "the feature is off" is an answer, not an error,
    /// and must never abort a worktree creation over a build-cache
    /// optimisation. The two predicates use their exit code as the answer.
    /// `resolve` + `reclaim` (#9153) are `merge-pr.sh`'s post-merge #7239
    /// reclaim — the last bash copy of that call sequence — split at the point
    /// the removal has to happen: `cargo metadata` needs the manifest that is
    /// about to disappear. Both exit 0 always; the merge already succeeded.
    #[command(subcommand)]
    CargoTargetDir(super::cargo_target_dir::CargoTargetDirCommand),

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

    /// `worktree.sh`'s staleness REFERENCE for the already-registered-worktree
    /// fast path (#8287, ported in #8354): `origin/<branch>` whenever that ref
    /// exists and has not already landed as a merged PR (the #5657 skip), else
    /// `BASE_REF` as before — plus the ahead/behind counts measured against
    /// whichever it chose. The decision that stops a `git reset --hard` onto
    /// the base from discarding an open PR's only local trace (#8147/#8190).
    /// Exit 0 always; one line, four tokens.
    WorktreeStaleRef(super::worktree_stale_ref::WorktreeStaleRefArgs),

    /// `worktree.sh`'s CLOSED-UNMERGED arm (#9083): the third state a pushed
    /// `origin/feature/issue-N` can be in, next to the open-PR (#4823/#7765)
    /// and merged-PR (#5657) arms that already had answers. A tip that is the
    /// head of a PR closed WITHOUT merging carries work somebody decided not
    /// to take, and reusing it silently seeded a worktree with a reverted
    /// slice plus a rejected follow-up, tens of commits behind `main` (the
    /// #8195 incident). Exit 0 = proceed and reuse as before, 1 = refuse; every
    /// inability to decide is 0, because a forge outage must never block
    /// worktree creation.
    WorktreeClosedPrBranch(super::worktree_closed_pr_branch::WorktreeClosedPrBranchArgs),
    /// `worktree.sh`'s `--sparse <paths...>` / `--full` family (#8195 slice
    /// 10), on both arms it reaches: after `git worktree add --no-checkout`,
    /// and the "worktree already exists" re-configure early exit. Retires a
    /// silent exit 128 on any cone git rejects, an unescaped cone-to-JSON
    /// builder, and a `git worktree list | grep -q` substring registration
    /// check that could disagree with the orphan guard about the same
    /// directory. Exit 0 applied, 1 refused/failed, 2 could not run.
    WorktreeSparse(super::worktree_sparse::WorktreeSparseArgs),

    /// `worktree.sh`'s base-ref preparation (#8195 slice 13): the fetch of
    /// `origin/$DEFAULT_BRANCH` and the `--base <branch>` stacked-PR
    /// resolution (validate, fetch, prefer `origin/<base>`, fall back to a
    /// local branch, refuse when neither exists). Prints `TOKEN<TAB>text`
    /// records the shell replays; `--json` refusal documents are built with
    /// `serde_json` instead of spliced by hand. Exit 0 resolved, 1 refused.
    WorktreeBase(super::worktree_base::WorktreeBaseArgs),

    /// `worktree.sh`'s in-worktree predicate and both decisions it gated
    /// (#8195 slice 11): the `--check` verb, and the create path's
    /// auto-navigation out of a worktree. The retired
    /// `[[ "$(git rev-parse --git-common-dir)" != "$(git rev-parse
    /// --show-toplevel)/.git" ]]` compared a path git answers RELATIVELY
    /// against an absolute one, so it was constant-true from every position a
    /// caller can stand in: `--check` reported the primary clone as a worktree
    /// and its exit 1 was dead code, and every `worktree.sh <N>` printed four
    /// spurious navigation lines. Exit 0/1 are the verb's two answers;
    /// `--porcelain` (the create arm's record stream) is always 0.
    WorktreeCheck(super::worktree_check::WorktreeCheckArgs),

    /// `worktree.sh`'s "the worktree directory already exists" arm, whole
    /// (#8195 slice 12): the registration probe, the preserve-vs-reset verdict,
    /// the #3548 sentinel back-fill and the stale-worktree `git reset --hard`
    /// behind its #6334 rescue guard. Retires the LAST unanchored
    /// `git worktree list | grep -q "$WORKTREE_PATH"` in the script — a
    /// substring match against symlink-RESOLVED paths, which refused a live
    /// worktree on any repo reached through a symlink (every macOS `/tmp`
    /// fixture) and accepted an unregistered `issue-4` beside a registered
    /// `issue-44`, then wrote a sentinel into it. Exit 0 = the worktree is
    /// usable, 1 = not a registered worktree (or the sentinel could not be
    /// written).
    WorktreeExisting(super::worktree_existing::WorktreeExistingArgs),

    /// `worktree.sh`'s LOCAL-branch reuse arm, whole (#8195 slice 14): the
    /// "already exists - reusing it" warning, the #6095/#6100 upstream
    /// correction (reached in-process, retiring the shell's last
    /// `_worktree_upstream_check` call site), the #8280 already-landed refusal
    /// and its degenerate-tip guard, and the base-ref divergence warning. Four
    /// steps whose ORDER is the contract, so they move as one unit. Retires a
    /// hand-spliced `--json` refusal document that was not valid JSON for any
    /// refname holding a quote — `git check-ref-format` permits one, and
    /// `$BRANCH_NAME` is operator input via the custom-branch argument. Exit 0
    /// = reuse, 1 = refuse; every inability to decide is 0, because a forge
    /// outage must never block worktree creation.
    WorktreeBranchReuse(super::worktree_branch_reuse::WorktreeBranchReuseArgs),

    /// The forge round-trip behind `lib/worktree-forge-pr-check.sh`'s #7765
    /// fresh-branch-shadow guard (#8195 slice 15) — the ORIGIN-branch sibling
    /// of `WorktreeBranchReuse`'s LOCAL-branch arm. Answers whether an OPEN
    /// PR already head-matches a branch `worktree.sh` is about to create
    /// fresh, distinguishing "confirmed no PR" / "no forge remote at all,
    /// nothing to shadow" from "the query itself failed" — the #7863
    /// regression was exactly those last two collapsing into each other.
    /// Prints a `TOKEN<TAB>text` record stream; always exits 0 (a query, not
    /// a refusal — the shell library still builds the `jq -cn` refusal
    /// documents itself).
    WorktreeOpenPr(super::worktree_open_pr::WorktreeOpenPrArgs),

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

    /// Fetch-or-rebuild, provision and restart the daemon (#8088), backing
    /// `loom-daemon-update.sh` — epic #7810's last and highest-risk port,
    /// because the file it provisions over is very often the binary
    /// executing right now. See `daemon_update::selfrepl` for the
    /// self-replacement design and why `resolve_daemon_bin()` is the wrong
    /// helper for the post-roll version check.
    DaemonUpdate(super::daemon_update::DaemonUpdateArgs),

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

    /// The operator-decision helper (#9344): validate a ranked-options
    /// decision (2-4 options best -> worst, each with a why, recommended
    /// first) and write it onto an issue as a fenced `decision` block before
    /// labelling it `loom:operator-decision`. Refuses, touching nothing, on
    /// any contract failure. Same frozen-`main.rs` reason as `shell-budget`.
    #[command(subcommand)]
    OperatorDecision(super::operator_decision::OperatorDecisionCommand),

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

    /// The `PreToolUse` decision for the `mcp__loom__*` tool namespace (#9108),
    /// behind the `defaults/hooks/guard-mcp-tools.sh` hook entry. Reads the
    /// hook payload on stdin, prints a deny document or nothing, and **always
    /// exits 0**. Not a port: brand-new logic, native from the start per the
    /// shell-language policy — MCP tool calls were the one tool class outside
    /// every `PreToolUse` matcher.
    GuardMcpTools(super::guard_mcp_tools::GuardMcpToolsArgs),

    /// The `PreToolUse` matcher-coverage contract for that guard (#9108): the
    /// `mcp__loom__.*` matcher exists in `.claude/settings.json` AND in the
    /// installer's `_PHOOK_*` arrays, its entry routes through
    /// `hook-wiring.sh`, and it carries the same fail-closed broken-install
    /// floor the `Bash` / `Edit|Write` entries carry. Exit 1 on any violation.
    /// Not a port of working shell: the check's first cut lived inside
    /// `contract`-category `check-guard-scan-contracts.sh`, which the
    /// shell-language policy and the #7810 shell-budget gate both send here.
    CheckGuardWiring(super::check_guard_wiring::CheckGuardWiringArgs),

    /// Per-segment PR latency, derived live from the forge timeline (#8923):
    /// review-queue wait, approval path, `loom:pr`→merged **split by operator
    /// gate**, Doctor response, and verdict invalidations — plus the live queue
    /// view with **dwell, not PR age**. `--advise` is the pre-wave advisory
    /// form: open PRs only, warns past `--threshold-hours`, always exit 0.
    /// Read-only; it never writes a label or a comment. Not a port: brand-new
    /// logic, native from the start per the shell-language policy.
    PrLatency(super::pr_latency_cmd::PrLatencyArgs),

    /// Render and read the `loom:blocked` **park record** (#8925) — the
    /// machine-readable `<!-- loom:park Blocked by: #N … -->` marker a role
    /// writes into an artifact body when it applies the label. `render` is what
    /// keeps the format from drifting when an LLM copies it out of prose;
    /// `parse` is its inverse for a shell caller. Pure — no forge read, no label
    /// write. See `defaults/docs/park-record.md`.
    #[command(subcommand)]
    ParkRecord(super::park_record::ParkRecordCommand),

    /// Curator's `<!-- loom:points=<N> -->` estimate-marker validation
    /// (#9056), backing `require-complexity-marker.sh`'s points-marker gate.
    /// Reads the issue body on stdin — the same body the shell script's own
    /// complexity-tier check already fetched, no second `gh` call. Not a
    /// port of a whole script: the complexity-tier half of
    /// `require-complexity-marker.sh` stays inline shell, unchanged; only the
    /// new points check moved here, to keep the #7810 `shell-budget` gate a
    /// non-event for this addition.
    CheckPointsMarker(super::points_marker_check::CheckPointsMarkerArgs),

    /// Refuse content carrying a credential shape — a Claude OAuth/API key,
    /// GitHub/Tailscale/Slack token, AWS key id, private key — whatever its
    /// path (#9133). Backs the `guard-loom-workflow.sh` commit/push check,
    /// `.githooks/pre-commit`/`pre-push`, and the CI scan. Never prints a
    /// value, only path, class and sha256[:8]. Exit 0 clean, 1 found, 2 could
    /// not scan. Not a port: new logic, native per the shell-language policy.
    SecretScan(super::secret_scan_cmd::SecretScanArgs),

    /// The close-triggered `loom:blocked` re-check (#9102, item 2 of #8927's
    /// deferred fix list): given `--closed <N>...` (the merged PR plus every
    /// issue it closed), comments on each open `loom:blocked` issue/PR that
    /// cites one of them as a blocker and now classifies stale. Reuses
    /// `check-stale-blocked`'s enumeration and the `dep_recheck` parsers.
    /// Called from `merge-pr.sh`'s post-merge path. Unlike
    /// `CheckStaleBlocked` it WRITES (a comment, never a label); `--dry-run`
    /// previews. Always exits 0.
    NotifyClearedBlockers(super::notify_cleared_blockers::NotifyClearedBlockersArgs),
    /// `sync-labels.sh`'s duplicate-declared-name scan (#8875): a
    /// `labels.yml` that carries two `- name:` entries for the same label
    /// (the pre-#4187-upgrade shape `merge_labels_block` now absorbs on
    /// install) is structural drift in the file itself, independent of
    /// forge state. Ported out of the `contract`-category script per the
    /// shell language policy. Prints each duplicated name once, in file
    /// order.
    LabelDuplicates(super::label_duplicates::LabelDuplicatesArgs),

    /// `fleet-send.sh`'s one-shot safehouse envelope post (#9517, epic
    /// #7810) — the lifecycle-role posting helper whose bash body was a
    /// hand-copy of `safehouse.rs`'s protocol. The implementation reuses the
    /// canonical `build_send_request` so the daemon and the role helper can
    /// never disagree about the wire again. Its HARD degradation contract is
    /// inherited verbatim: every failure — missing env, absent socket,
    /// invalid argument, wire error — is a silent `exit 0`, because the room
    /// is optional and the role's work is not. (This is also why the stub
    /// bypasses `lib/script-helper.sh`, whose missing-daemon path is a loud
    /// error: silence IS this entry point's interface.)
    FleetSend(super::fleet_send::FleetSendArgs),

    /// The versioned forge **operation inventory** and its accounting (#9777,
    /// phase 1 of epic #9769): the coverage validator, the unclassified-call
    /// change gate, the four-axis coverage report and the hosted-probe
    /// manifest. Not a port: brand-new logic, native from the start per the
    /// shell-language policy — and native specifically so the gate can be a
    /// real ratchet rather than a grep in a `contract`-category script. It
    /// lives in this flattened enum for the same frozen-`main.rs` reason as
    /// `shell-budget`, which keeps `forge-inventory` a real nested subcommand
    /// at zero cost to that file. Makes no forge call.
    #[command(subcommand)]
    ForgeInventory(super::forge_inventory_cmd::ForgeInventoryCommand),
}

impl ScriptPortCommand {
    /// Never returns: every arm exits with its subcommand's own code, which
    /// the stubs' callers branch on.
    pub(crate) fn run(self) -> Result<()> {
        match self {
            ScriptPortCommand::PrQueue(args) => args.run(),
            ScriptPortCommand::SessionExec(args) => args.run(),
            ScriptPortCommand::PrivateWorkspace(args) => args.run(),
            ScriptPortCommand::SweepCheckpoint(args) => args.run(),
            ScriptPortCommand::DepClassify(cmd) => cmd.run(),
            ScriptPortCommand::DepRecheckFingerprint(cmd) => cmd.run(),
            ScriptPortCommand::ReleaseFetch(args) => args.run(),
            ScriptPortCommand::ReleaseResolve(args) => args.run(),
            ScriptPortCommand::ReleaseExplain(args) => args.run(),
            ScriptPortCommand::CodexSandboxNoop(args) => args.run(),
            ScriptPortCommand::MergePr(cmd) => cmd.run(),
            ScriptPortCommand::ShellBudget(args) => args.run(),
            ScriptPortCommand::Eta(cmd) => cmd.run(),
            ScriptPortCommand::MergePrRefs(cmd) => cmd.run(),
            ScriptPortCommand::WorktreeLock(cmd) => cmd.run(),
            ScriptPortCommand::CargoTargetDir(cmd) => cmd.run(),
            ScriptPortCommand::WorktreeWip(cmd) => cmd.run(),
            ScriptPortCommand::WorktreeRemove(args) => args.run(),
            ScriptPortCommand::WorktreeLink(args) => args.run(),
            ScriptPortCommand::WorktreeCleanup(args) => args.run(),
            ScriptPortCommand::WorktreeReset(args) => args.run(),
            ScriptPortCommand::WorktreeBranchConflict(args) => args.run(),
            ScriptPortCommand::WorktreeSubmodules(args) => args.run(),
            ScriptPortCommand::WorktreeUpstream(args) => args.run(),
            ScriptPortCommand::WorktreeStaleRef(args) => args.run(),
            ScriptPortCommand::WorktreeClosedPrBranch(args) => args.run(),
            ScriptPortCommand::WorktreeSparse(args) => args.run(),
            ScriptPortCommand::WorktreeBase(args) => args.run(),
            ScriptPortCommand::WorktreeCheck(args) => args.run(),
            ScriptPortCommand::CodexHooks(cmd) => cmd.run(),
            ScriptPortCommand::WorktreeExisting(args) => args.run(),
            ScriptPortCommand::WorktreeBranchReuse(args) => args.run(),
            ScriptPortCommand::WorktreeOpenPr(args) => args.run(),
            ScriptPortCommand::RetryClassify(cmd) => cmd.run(),
            ScriptPortCommand::Provenance(cmd) => cmd.run(),
            ScriptPortCommand::DaemonWatchdog(args) => args.run(),
            ScriptPortCommand::DaemonStart(args) => args.run(),
            ScriptPortCommand::DaemonUpdate(args) => args.run(),
            ScriptPortCommand::FleetSend(args) => args.run(),
            ScriptPortCommand::SkipLabels(args) => args.run(),
            ScriptPortCommand::WorktreeState(cmd) => cmd.run(),
            ScriptPortCommand::DuplicateScan(args) => args.run(),
            ScriptPortCommand::PremiseCheck(args) => args.run(),
            ScriptPortCommand::OperatorDecision(cmd) => cmd.run(),
            ScriptPortCommand::ReconcileStack(args) => args.run(),
            ScriptPortCommand::GenerateAgentSkills(args) => args.run(),
            ScriptPortCommand::GitBlobLines(args) => args.run(),
            ScriptPortCommand::FleetCaptain(args) => args.run(),
            ScriptPortCommand::RoleToolPolicy(cmd) => cmd.run(),
            ScriptPortCommand::RuntimeLaunchEnv(args) => args.run(),
            ScriptPortCommand::CheckStaleBlocked(args) => args.run(),
            ScriptPortCommand::GuardMcpTools(args) => args.run(),
            ScriptPortCommand::CheckGuardWiring(args) => args.run(),
            ScriptPortCommand::PrLatency(args) => args.run(),
            ScriptPortCommand::ParkRecord(cmd) => cmd.run(),
            ScriptPortCommand::CheckPointsMarker(args) => args.run(),
            ScriptPortCommand::SecretScan(args) => args.run(),
            ScriptPortCommand::NotifyClearedBlockers(args) => args.run(),
            ScriptPortCommand::LabelDuplicates(args) => args.run(),
            ScriptPortCommand::ForgeInventory(cmd) => cmd.run(),
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

    /// The stale-cached-mergeable recheck decision (#6104): once REST
    /// `.mergeable` has read `false`, classify the backoff re-reads plus the
    /// local `git merge-tree` corroboration into `merge:` / `refuse-stale:` /
    /// `refuse-conflict:` — distinguishing "genuinely conflicts" from "the
    /// forge's cached state is stale/unknown". Always exits 0 with exactly
    /// one `<action>:<reason>` line; the I/O loop stays in the shell.
    MergeableRecheck(super::merge_pr_mergeable_recheck::MergeableRecheckArgs),

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

    /// Read-only report of which required checks and paths forced the #8508
    /// re-dates (#9746), from the trailers on the re-date commits reachable
    /// from `--ref` (default origin/main) within `--since` (default 24h).
    RedateReport(super::merge_pr_redate_report::RedateReportArgs),

    /// The pre-merge `loom:pr` review-signal guard (#7419): refuse a merge
    /// whose current head does not carry `loom:pr`, unless
    /// `--allow-unapproved` asserts responsibility. Exit 0 = present or
    /// overridden (see stdout for which), 1 = absent with no override.
    LoomPrGuard(super::merge_pr_loom_pr_guard::LoomPrGuardArgs),

    /// The `--allow-unapproved` audit-comment BODY (#7419, a later #8191
    /// slice than `LoomPrGuard` above): byte-frozen from the retired shell's
    /// `_check_loom_pr_label`, posted on the PR after a REAL (non-dry-run)
    /// override. Reads labels on stdin (one caller-added trailing newline
    /// stripped). Always exits 0 with the `LOOM-MERGE-PR-COMMENT` sentinel
    /// then the body, matching `PartialComment`'s protocol below — except
    /// exit 2 when stdin itself could not be read at all, which is not the
    /// same as an empty label set.
    LoomPrOverrideComment(super::merge_pr_loom_pr_override_comment::LoomPrOverrideCommentArgs),

    /// The `champion:hold-state` staleness WARNING (#7419 AC #3): the other
    /// half of `loom-pr-guard`'s story, fired only when `loom:pr` IS present
    /// and Champion's recorded hold head is not the head about to merge.
    /// Advisory, never a gate — always exits 0 (stdout is either the
    /// `LOOM-HOLD-STATE-CLEAN` sentinel or the warning), except exit 2 when
    /// the comment stream could not be read at all.
    HoldState(super::merge_pr_hold_state::HoldStateArgs),

    /// Evaluate one durable "approved, but not yet" sequencing hold against
    /// live forge state (#9378): read the newest trusted `<!-- loom:sequence
    /// … -->` marker on the PR, re-read the recorded predecessor, and print
    /// one sentinel — CLEAR (predecessor merged at the recorded head),
    /// DISSOLVED (closed unmerged), KEEP (waiting at the recorded head),
    /// REPLAN (a pinned head moved), NONE (no marker). Exit 0 on any answer,
    /// 2 on a failed read (never release on a failed read). The gate itself
    /// is the `loom:sequenced` label in `verdict-contradiction`'s BLOCKING
    /// set; this verb is what moves that label when the condition is met.
    SequenceEval(super::merge_pr_sequence::SequenceEvalArgs),

    /// Compute the repository's landing-order plan and print it, touching
    /// nothing (#9686) — the read-only replay surface: groups, chain edges,
    /// and what the sequencing pass would apply this tick.
    SequencePlan(super::merge_pr_sequence::SequencePlanArgs),

    /// Prepare a combined candidate PR from an eligible group of component
    /// PRs (#9688, contract ADR-0023): verify eligibility fresh, construct
    /// the candidate in a scratch worktree, push, create ONE candidate PR
    /// carrying the trusted component mapping, and reserve every source via
    /// the #9378 sequencing gate (source = sequenced behind the candidate).
    /// Adopt-first on the deterministic attempt id, only while the attempt
    /// is live; hard-abort on conflict or on any push to the candidate or a
    /// source (ADR-0023 §3).
    ConsolidatePrepare(super::merge_pr_consolidate::ConsolidatePrepareArgs),

    /// Abort a consolidation attempt (#9688): release ONLY the attempt's own
    /// still-live reservations, close the candidate PR with the abort cause
    /// recorded, clean up its branch. Sources
    /// are preserved untouched. A merged candidate cannot be aborted —
    /// landing wins and #9689's reconcile owns the aftermath.
    ConsolidateAbort(super::merge_pr_consolidate::ConsolidateAbortArgs),

    /// The async-close-race worktree-cleanup gate (#4186): whether a merged
    /// PR's issue is actually finished, so a partial-increment worktree the
    /// next Builder increment still needs is not removed out from under it.
    /// Reads `forge_pr_close_targets`'s output on stdin; `--state` is
    /// OPTIONAL and supplied only on the shell's second call. Exit 0 =
    /// cleanup authorized (`CLOSE-TARGET`/`STATE-CLOSED`), 1 = preserve, 3 =
    /// the shell must fetch `forge_get_issue_state` and call again with
    /// `--state` — see `cli::merge_pr_issue_close_gate`.
    IssueCloseGate(super::merge_pr_issue_close_gate::IssueCloseGateArgs),

    /// `_maybe_delete_local_branch` (#4100/#5015/#7812): the squash-aware
    /// local-branch delete rule, now shared verbatim with `worktree.sh
    /// remove` (#8195 slice 3) instead of duplicated. Always exits 0 — a
    /// branch that could not be deleted is a `WARNING` line, not a failure —
    /// and prints one `LEVEL<TAB>message` line per decision for the caller to
    /// replay through its own logging (see `cli::merge_pr_delete_branch`).
    DeleteBranch(super::merge_pr_delete_branch::DeleteBranchArgs),

    /// The post-merge worktree-cleanup data-loss guard (#5031, classified by
    /// #5658): refuse a `git worktree remove --force` on a worktree that still
    /// holds uncommitted user work, so a colliding branch name cannot destroy a
    /// live sibling builder's edits. Reads `git status --porcelain` on stdin.
    /// Exit 0 + sentinel = no user work (removal may proceed), 1 = refuse (the
    /// refusal arrives as `LEVEL<TAB>message` lines to replay), 2 = the
    /// worktree's state could not be read, which the caller must ALSO treat as
    /// a refusal.
    DirtyGuard(super::merge_pr_dirty_guard::DirtyGuardArgs),

    /// The post-merge `git worktree remove --force` itself (#6372, #8191
    /// slice), run only after every guard passed: one prune-and-retry on
    /// failure, then `LOOM-WORKTREE-TEARDOWN REMOVED|FAILED` and
    /// `LEVEL<TAB>message` records to replay. Always exits 0; the shell reads
    /// anything else as "did not run" and removes nothing — see
    /// `cli::merge_pr_worktree_teardown`.
    WorktreeTeardown(super::merge_pr_worktree_teardown::WorktreeTeardownArgs),

    /// Decide ONE zero-row check-runs poll of `--auto`'s settle wait (#9091):
    /// settle now, keep waiting, or report the whole wait spent. Bounded only
    /// when the base branch requires no status-check contexts; a lookup that
    /// errors, or a required context present, keeps #6169's full wait. Always
    /// exits 0 with one sentinel-led line — see `cli::merge_pr_zero_checks`.
    ZeroChecksSettle(super::merge_pr_zero_checks::ZeroChecksSettleArgs),

    /// The persistent-vs-transient check-runs HTTP 404 classification in the
    /// same wait loop (#6389, an #8191 slice): invoked only after a poll's
    /// fetch attempt has already failed, decides whether a confirmed 404 has
    /// now repeated `LOOM_CHECK_RUNS_404_STREAK` times in a row — in which
    /// case give up waiting and proceed straight to the synchronous merge —
    /// or is still below threshold, in which case the caller's existing
    /// truncated-check/deadline/sleep handling runs unchanged. Always exits 0
    /// with one `LOOM-CHECK-RUNS-STREAK <PROCEED|PENDING> <streak>` line —
    /// see `cli::merge_pr_check_runs_streak`.
    CheckRunsStreak(super::merge_pr_check_runs_streak::CheckRunsStreakArgs),

    /// The per-poll READ of the check-runs rollup in the same wait loop
    /// (#8191 slice): failing names, pending names and `total_count` out of
    /// the `forge_get_check_runs` payload on stdin. Exit 0 with four
    /// NUL-terminated fields ending `LOOM-CHECK-RUNS-ROLLUP`; exit 2 (nothing
    /// on stdout) for a payload outside the forge contract, which the caller
    /// treats as still pending — see `cli::merge_pr_check_runs_rollup`.
    CheckRunsRollup(super::merge_pr_check_runs_rollup::CheckRunsRollupArgs),

    /// The OTHER classification in the same wait loop (#8191 slice): once a
    /// poll finds a FAILING check, whether it is a required status-check
    /// context (refuse), informational with nothing pending (proceed to the
    /// synchronous merge), or informational with something else still
    /// pending (keep waiting, unchanged). Exit 0 = `PROCEED`/`PENDING`, 1 =
    /// `REQUIRED` (refuse), 2 = malformed stdin frame (also refuse) — see
    /// `cli::merge_pr_checks_failure`.
    ChecksFailure(super::merge_pr_checks_failure::ChecksFailureArgs),

    /// The pre-merge no-hand-bump guard (#7827) and its oracle choice
    /// (#8284): run the canonical version checker from the right ref against
    /// the merge base. Exit 0 = pass/skip/dry-run report, 1 = confirmed
    /// forbidden version edit; output is `WARNING`/`BLOCK<TAB>line` records
    /// — see `cli::merge_pr_version_policy`.
    VersionPolicy(super::merge_pr_version_policy::VersionPolicyArgs),

    /// The pre-merge merge-ordering guard (#3747 item 2, reshaped by #7982):
    /// discover open CHILD PRs still targeting this parent branch and
    /// ESTABLISH the postcondition `reconcile-stack.sh` needs by pinning the
    /// parent tip to `refs/loom/parent/<branch>`. Exit 0 = proceed (skip,
    /// bypass, dry-run report, or pin written), 1 = the tip could not be
    /// pinned, which is the one case still refused. Output is
    /// `CHILDREN`/`PIN-WRITTEN`/`WARNING`/`BLOCK` records — see
    /// `cli::merge_pr_stacked_children`.
    StackedChildren(super::merge_pr_stacked_children::StackedChildrenArgs),
    /// The PRIMARY (main) worktree's path, parsed from `git worktree list
    /// --porcelain` on stdin (#8191 slice: the #3710 guard's input). Empty
    /// output at exit 0 means the input held no `worktree` record — which the
    /// caller must NOT read as "the target is not the primary checkout".
    WorktreePrimary(super::merge_pr_worktrees::WorktreePrimaryArgs),

    /// The branch short-name checked out at `--path`, from porcelain on stdin.
    /// Empty at exit 0 for a detached/bare entry or a path in no stanza.
    WorktreeBranchFor(super::merge_pr_worktrees::WorktreeBranchForArgs),

    /// The worktree path with `--branch` checked out, from porcelain on stdin.
    /// Empty at exit 0 when no worktree holds it.
    WorktreeFindByBranch(super::merge_pr_worktrees::WorktreeFindByBranchArgs),

    /// `--worktree-path`'s PRE-flight registered-worktree check, from
    /// porcelain on stdin: is `--path` ANY `worktree ` record, not just the
    /// first. Exit 0 = registered, 1 = parsed and not registered — the
    /// retired `awk` answered through exit status alone, so this does too.
    WorktreeContains(super::merge_pr_worktrees::WorktreeContainsArgs),

    /// The post-merge partial-increment label reset decision (#3667/#4569):
    /// from the referenced issue's fresh body on stdin plus the pre-merge
    /// guard's `--conflicted` / `--open-before-merge` facts, the ordered
    /// `INFO`/`WARNING<TAB>text`, `REOPEN` and `SWAP` steps the shell
    /// performs. Exit 0 with the plan (empty = silent skip, a PR), 2 = stdin
    /// unreadable — see `cli::merge_pr_partial_reset`.
    PartialReset(super::merge_pr_partial_reset::PartialResetArgs),

    /// The pre-merge partial-increment close-conflict decision (#4569/#4595):
    /// from a NUL-framed body / commit messages / sidebar close targets /
    /// `(issue, fresh body)` record on stdin, the `OPEN`/`CONFLICT<TAB>n` set
    /// entries and `WARNING<TAB>text` lines the shell replays, terminated by
    /// `LOOM-PARTIAL-CONFLICT-DONE`. Exit 2 = malformed frame; the shell
    /// refuses the merge without the terminator — see
    /// `cli::merge_pr_partial_conflict`.
    PartialConflict(super::merge_pr_partial_conflict::PartialConflictArgs),

    /// The two post-merge partial-increment AUDIT COMMENTS (#3667/#4569) —
    /// `## Partial Increment Merged` and `## Premature Auto-Close Reverted` —
    /// byte-frozen from the retired shell. Prints `LOOM-MERGE-PR-COMMENT`,
    /// then the body verbatim with no trailing newline. Always exits 0; the
    /// seam fails OPEN because both are posted AFTER the mutation they
    /// describe, but a body is only ever posted when the sentinel is present,
    /// so a binary predating the verb cannot make the shell overwrite the
    /// audit trail with silence — see `cli::merge_pr_partial_comment`.
    PartialComment(super::merge_pr_partial_comment::PartialCommentArgs),

    /// Which route a FAILED merge's forge error text sends the retry ladder
    /// down (#8191 slice): `merge-in-progress` (405, wait and retry),
    /// `head-mismatch` (#5579 — never retry-and-merge), `base-modified` (sync
    /// the head and retry) or `other` (stop). Reads the response on stdin and
    /// always exits 0 with one `LOOM-MERGE-RESPONSE <token>` line; `other` is
    /// an answer, not a failure. Exit 2 = stdin unreadable. The precedence
    /// between the two SHA-shaped routes is the safety property — see
    /// `cli::merge_pr_response`.
    ClassifyResponse(super::merge_pr_response::ClassifyResponseArgs),

    /// The post-merge closed-issue `loom:building` cleanup decision (#6199):
    /// from the fresh body of an issue THIS merge closed, on stdin, either
    /// `STRIP` (the claim label is stale — remove it) or `SKIP<TAB><reason>`.
    /// Exit 0 with the decision, 2 = stdin unreadable. Silence is never a
    /// decision — see `cli::merge_pr_closed_building`.
    ClosedBuilding(super::merge_pr_closed_building::ClosedBuildingArgs),

    /// The post-merge stacked-child reconcile PLAN (#3747 item 1): the
    /// parent-branch gate, the `[{number, headRefName}]` children-rollup parse
    /// (on stdin) and each child's derived issue number. Prints
    /// `NOT-STACKED`, `UNREADABLE <detail>`, or `COUNT` + one `CHILD`/`MALFORMED`
    /// line per element. Exit 0 with the plan, 2 = stdin unreadable; the seam
    /// fails OPEN (skip the pass) — see `cli::merge_pr_reconcile`.
    ReconcilePlan(super::merge_pr_reconcile::ReconcilePlanArgs),

    /// Which route ONE stacked child takes (#3747 item 1): `defer` when its
    /// issue's label list (on stdin) still carries `loom:building` — a Builder
    /// probably has that branch checked out, and rebasing it would race live
    /// work — otherwise `reconcile`. The defer answer carries the comment body
    /// to post. Exit 0 with the route, 2 = stdin unreadable — see
    /// `cli::merge_pr_reconcile`.
    ReconcileChild(super::merge_pr_reconcile::ReconcileChildArgs),

    /// The #6694/#6264 remove-vs-preserve decision for post-merge worktree
    /// cleanup, shared across the three call sites (the Loom-convention path,
    /// the porcelain discovery fallback, and a co-existing Judge/Doctor review
    /// worktree) that used to run it identically three times. Always exits 0
    /// with `REMOVE`/`PRESERVE` plus `LEVEL<TAB>message` lines to replay — see
    /// `cli::merge_pr_worktree_preserve`.
    WorktreePreserve(super::merge_pr_worktree_preserve::WorktreePreserveArgs),

    /// Which worktree paths a merged PR owns (#6264/#3530, #8191 slice): the
    /// strict `^feature/issue-([0-9]+)$` classification, the override-aware
    /// worktree root, and from them the default cleanup target plus the
    /// co-existing Judge/Doctor `pr-<N>` review worktree. Prints one
    /// `LOOM-CLEANUP-PATHS<TAB>default<TAB>issue<TAB>judge-pr` line (the
    /// always-non-empty path leads: tab is IFS whitespace, so bash's `read`
    /// cannot preserve an empty LEADING field); every
    /// `[[ -d ]]`/sentinel test and the removal itself stay in the shell. Exit
    /// 2 = the resolved root cannot be framed unambiguously. The seam fails
    /// OPEN (no targets, clean up nothing) — see `cli::merge_pr_cleanup_paths`.
    CleanupPaths(super::merge_pr_cleanup_paths::CleanupPathsArgs),

    /// The identity/ownership gate in front of `_remove_loom_worktree` (#8191
    /// slice): the #3710 primary-worktree hard guard, then the
    /// `.loom-managed` sentinel guard with its `--worktree-path` bypass.
    /// `git worktree list --porcelain` on stdin; first line
    /// `LOOM-REMOVE-GATE PROCEED|REFUSE` then `LEVEL<TAB>message` records.
    /// The shell treats anything else as REFUSE — see
    /// `cli::merge_pr_remove_gate`.
    RemoveGate(super::merge_pr_remove_gate::RemoveGateArgs),
}

impl MergePrCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            MergePrCommand::VerdictContradiction(args) => args.run(),
            MergePrCommand::MergeableRecheck(args) => args.run(),
            MergePrCommand::StaleChecks(args) => args.run(),
            MergePrCommand::HeadSyncRetry(args) => args.run(),
            MergePrCommand::RedateChecks(args) => args.run(),
            MergePrCommand::RedateReport(args) => args.run(),
            MergePrCommand::LoomPrGuard(args) => args.run(),
            MergePrCommand::LoomPrOverrideComment(args) => args.run(),
            MergePrCommand::HoldState(args) => args.run(),
            MergePrCommand::SequenceEval(args) => args.run(),
            MergePrCommand::SequencePlan(args) => args.run(),
            MergePrCommand::ConsolidatePrepare(args) => args.run(),
            MergePrCommand::ConsolidateAbort(args) => args.run(),
            MergePrCommand::IssueCloseGate(args) => args.run(),
            MergePrCommand::DeleteBranch(args) => args.run(),
            MergePrCommand::DirtyGuard(args) => args.run(),
            MergePrCommand::WorktreeTeardown(args) => args.run(),
            MergePrCommand::ZeroChecksSettle(args) => args.run(),
            MergePrCommand::CheckRunsStreak(args) => args.run(),
            MergePrCommand::CheckRunsRollup(args) => args.run(),
            MergePrCommand::VersionPolicy(args) => args.run(),
            MergePrCommand::StackedChildren(args) => args.run(),
            MergePrCommand::WorktreePrimary(args) => args.run(),
            MergePrCommand::WorktreeBranchFor(args) => args.run(),
            MergePrCommand::WorktreeFindByBranch(args) => args.run(),
            MergePrCommand::WorktreeContains(args) => args.run(),
            MergePrCommand::PartialReset(args) => args.run(),
            MergePrCommand::PartialConflict(args) => args.run(),
            MergePrCommand::PartialComment(args) => args.run(),
            MergePrCommand::ClassifyResponse(args) => args.run(),
            MergePrCommand::ClosedBuilding(args) => args.run(),
            MergePrCommand::ReconcilePlan(args) => args.run(),
            MergePrCommand::ReconcileChild(args) => args.run(),
            MergePrCommand::ChecksFailure(args) => args.run(),
            MergePrCommand::WorktreePreserve(args) => args.run(),
            MergePrCommand::RemoveGate(args) => args.run(),
            MergePrCommand::CleanupPaths(args) => args.run(),
        }
    }
}
