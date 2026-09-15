# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#6290**: fix: name-allowlist printenv SECRET/TOKEN/KEY ask pattern to stop LOOM_TOKEN_NAME false positive
- **#7444**: feat(spawn-claude): per-sweep container resource limits + containment observability
- **#7467**: fix(worktree): refuse stale-worktree reset when a live process holds it open
- **#7496**: fix(guard): distinguish escaped from live backtick/$( in --body masking
- **#7519**: fix(guard): stop hard-denying for-loop wordlists whose only consumer is a jq --arg filter script (#7515)
- **#7533**: guard: extend force-op:detached safe-list to a worktree's own branch (#7530)
- **#7536**: tokens: expire session-limit bad-marks with their own 5h window
- **#7594**: fix: back off and surface permanently-failing worktree removals (#7590)

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#7647**: Judge: reconcile paginated formal reviews and inline threads before approval
- **#7652**: classify-dependency-block.sh: dependency-keyword regex misses 'cannot start until #N' phrasing

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#7647**: Judge: reconcile paginated formal reviews and inline threads before approval
- **#7652**: classify-dependency-block.sh: dependency-keyword regex misses 'cannot start until #N' phrasing
- **#7664**: watchdog: peer-coordination escalation needs hysteresis and dedup — refiles a tracking issue on every self-recovering flap
- **#7668**: Builder/Doctor: rebase onto origin/main immediately before opening a PR that touches a hot-churn file (3 consecutive merge-conflict rejections on one README status paragraph)

## In Progress

Issues currently being built (`loom:building`).

- **#7658**: Hermit/Architect: verify every cited path, line range and repo-state claim against origin/main before filing (verify-proposal-refs.sh)
- **#7659**: Proposer roles: cite only paths that exist in the dispatched workspace; sibling checkouts named in a repo docs are context, never a citation target
- **#7660**: Doctor: Priority-1 conflict query does not exclude loom:operator-held PRs, so a held PR is claimed, rebased, then stood down
- **#7662**: dashboard: host card roster — show active repos, fold the idle set into a closed-by-default accordion
- **#7663**: dashboard: @cloudflare/vitest-pool-workers 0.22.0 upgrade needs vitest v4 config migration + breaks D1 per-test storage isolation
- **#7665**: champion-pr-merge.md: one remaining 'for file in $FILES' loop (line 1092) breaks under zsh
- **#7666**: champion: a passing, already-decomposed epic is escalated to the operator after repeated identical stand-down passes

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#7653**: fix(classify-dependency-block): recognize "cannot start/proceed until" phrasing
- **#7656**: fix(judge): reconcile paginated formal reviews and inline threads before approval
- **#7669**: fix(champion-pr-merge): convert remaining for-loop to zsh-safe while-read
- **#7670**: fix(champion): stop escalating a passing, already-decomposed epic to the operator
- **#7671**: fix(dashboard): complete vitest-pool-workers 0.22.0 upgrade, fix D1 test isolation regression

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#6290**: fix: name-allowlist printenv SECRET/TOKEN/KEY ask pattern to stop LOOM_TOKEN_NAME false positive
- **#7444**: feat(spawn-claude): per-sweep container resource limits + containment observability
- **#7467**: fix(worktree): refuse stale-worktree reset when a live process holds it open
- **#7496**: fix(guard): distinguish escaped from live backtick/$( in --body masking
- **#7519**: fix(guard): stop hard-denying for-loop wordlists whose only consumer is a jq --arg filter script (#7515)
- **#7533**: guard: extend force-op:detached safe-list to a worktree's own branch (#7530)
- **#7536**: tokens: expire session-limit bad-marks with their own 5h window
- **#7594**: fix: back off and surface permanently-failing worktree removals (#7590)

## Proposed

Issues carrying `loom:curated`.

- **#4136**: measure: every sweep phase re-reads the repo from scratch — quantify the duplicated-read cost *(curated)*
- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#5512**: Quarantine stashes accumulate with no lifecycle — 37 across one fleet, oldest 9 days, all referencing closed issues *(curated)*
- **#5660**: Vendored guard-destructive-generic.sh has drifted ~2,200 lines ahead of its upstream, and the single-marker capability probe makes partial reconciliation unsafe *(curated)*
- **#6245**: Guard ask-pattern false positive: printenv of an account-label env var denied by credential-exposure TOKEN pattern, blocks headless runs *(curated)*
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space *(curated)*
- **#6565**: Dogfood config: loom-repo curator starved 3d — runtime=codex admitted with no codex model configured (#5028 skip, DEBUG-silent) *(curated)*
- **#6646**: Sweep resync committed, rebased and bypass-pushed the primary clone's main while an operator session was active in that clone *(curated)*
- **#6650**: .loom/config.json commits a live Matrix room id and ingest URL — intentional, or move to the private overlay tier? *(curated)*
- **#6704**: Roster-driven role-runner shard assignment: reassign a dead host's slice within a bounded window (follow-up to #6374's static ring) *(curated)*
- **#6969**: auto_update drain-and-restart: one relaunch waited ~4 min for the watchdog instead of launchd (KeepAlive.SuccessfulExit) — single observation *(curated)*
- **#7356**: Guard friction: worktree-write-confinement-unresolved-var denies mktemp/tmp-scoped writes (44/126 = top guard-decision volume) *(curated)*
- **#7359**: merge=ours driver on .loom/install-metadata.json can silently drop non-loom_version field edits during rebase, uncaught by version-check-gate.sh *(curated)*
- **#7430**: [Epic #6896] Phase 3: Per-sweep resource limits and containment observability *(curated)*
- **#7463**: Dispatch-time worktree prep can reset/clean a worktree while orphaned processes from a prior interrupted session are still writing into it *(curated)*
- **#7515**: Guard false positive: catastrophic:aws s3 rb hard-denies for-loop wordlists with no live aws invocation, post-#7292 *(curated)*
- **#7522**: tokens: readmit session-limit accounts when the 5h window resets instead of holding them for the 6h exhaustion cooldown *(curated)*
- **#7526**: daemon: use #7513's phase-timing instrumentation to fix the actual status/health IPC bottleneck (ask 2/3) *(curated)*
- **#7530**: Guard friction: force-op:detached ASKs on a Loom worktree resetting to its OWN feature branch's origin tip *(curated)*
- **#7590**: worktree_reaper retries and fails forever on root-owned build-cache files, no backoff or health visibility *(curated)*
- **#7647**: Judge: reconcile paginated formal reviews and inline threads before approval *(curated)*
- **#7652**: classify-dependency-block.sh: dependency-keyword regex misses 'cannot start until #N' phrasing *(curated)*
- **#7657**: Champion: close a proposal whose central premise is verified false instead of escalating it as an operator decision *(curated)*
- **#7658**: Hermit/Architect: verify every cited path, line range and repo-state claim against origin/main before filing (verify-proposal-refs.sh) *(curated)*
- **#7659**: Proposer roles: cite only paths that exist in the dispatched workspace; sibling checkouts named in a repo docs are context, never a citation target *(curated)*
- **#7660**: Doctor: Priority-1 conflict query does not exclude loom:operator-held PRs, so a held PR is claimed, rebased, then stood down *(curated)*
- **#7662**: dashboard: host card roster — show active repos, fold the idle set into a closed-by-default accordion *(curated)*
- **#7663**: dashboard: @cloudflare/vitest-pool-workers 0.22.0 upgrade needs vitest v4 config migration + breaks D1 per-test storage isolation *(curated)*
- **#7664**: watchdog: peer-coordination escalation needs hysteresis and dedup — refiles a tracking issue on every self-recovering flap *(curated)*
- **#7665**: champion-pr-merge.md: one remaining 'for file in $FILES' loop (line 1092) breaks under zsh *(curated)*
- **#7666**: champion: a passing, already-decomposed epic is escalated to the operator after repeated identical stand-down passes *(curated)*
- **#7668**: Builder/Doctor: rebase onto origin/main immediately before opening a PR that touches a hot-churn file (3 consecutive merge-conflict rejections on one README status paragraph) *(curated)*

## Proposed (Architect / Hermit)

- **#3979**: Architecture: elastic compute — expand sweep parallelism onto cloud worker hosts when local CPU saturates *(architect)*
- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*
- **#7431**: [Epic #6896] Phase 3: Fleet-default rollout — soak criteria, flip containment default on Linux fleet hosts *(architect)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
- **#6109**: Add a runtime-neutral scientific research lifecycle with evidence-gated phase contracts
- **#6896**: Epic: Session containers — persistent Codex auth, mandatory worker containment, and a remote-execution job seam

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 8 |
| Urgent | 3 |
| Ready (`loom:issue`) | 5 |
| In Progress (`loom:building`) | 7 |
| PRs awaiting review | 5 |
| Approved PRs awaiting merge | 8 |
| Curated | 32 |
| Architect / Hermit proposals | 4 |
| Active epics | 3 |
<!-- guide:plan-body:end -->
