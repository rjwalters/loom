# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#7444**: feat(spawn-claude): per-sweep container resource limits + containment observability
- **#7467**: fix(worktree): refuse stale-worktree reset when a live process holds it open
- **#7496**: fix(guard): distinguish escaped from live backtick/$( in --body masking
- **#7519**: fix(guard): stop hard-denying for-loop wordlists whose only consumer is a jq --arg filter script (#7515)
- **#7533**: guard: extend force-op:detached safe-list to a worktree's own branch (#7530)
- **#7536**: tokens: expire session-limit bad-marks with their own 5h window
- **#7594**: fix: back off and surface permanently-failing worktree removals (#7590)
- **#7683**: ci: add warn-only guard-destructive vendored/canonical drift check

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#6646**: Sweep resync committed, rebased and bypass-pushed the primary clone's main while an operator session was active in that clone
- **#7678**: merge-pr.sh silently exits 1 on every PR without a champion:hold-state marker (regression in #7435/bf4f1965)

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#6646**: Sweep resync committed, rebased and bypass-pushed the primary clone's main while an operator session was active in that clone
- **#6650**: .loom/config.json commits a live Matrix room id and ingest URL — intentional, or move to the private overlay tier?
- **#7659**: Proposer roles: cite only paths that exist in the dispatched workspace; sibling checkouts named in a repo docs are context, never a citation target
- **#7664**: watchdog: peer-coordination escalation needs hysteresis and dedup — refiles a tracking issue on every self-recovering flap
- **#7665**: champion-pr-merge.md: one remaining 'for file in $FILES' loop (line 1092) breaks under zsh
- **#7666**: champion: a passing, already-decomposed epic is escalated to the operator after repeated identical stand-down passes
- **#7678**: merge-pr.sh silently exits 1 on every PR without a champion:hold-state marker (regression in #7435/bf4f1965)

## In Progress

Issues currently being built (`loom:building`).

- **#6969**: auto_update drain-and-restart: one relaunch waited ~4 min for the watchdog instead of launchd (KeepAlive.SuccessfulExit) — single observation
- **#7359**: merge=ours driver on .loom/install-metadata.json can silently drop non-loom_version field edits during rebase, uncaught by version-check-gate.sh
- **#7694**: Builder: probe for a live sibling worktree before worktree.sh touches files (generalizing #6765)

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#7669**: fix(champion-pr-merge): convert remaining for-loop to zsh-safe while-read
- **#7670**: fix(champion): stop escalating a passing, already-decomposed epic to the operator
- **#7682**: fix(merge-pr): guard hold_head assignment against pipefail abort
- **#7685**: feat(resync): land resync commits conservatively, never rebase/bypass-push (#6646)
- **#7692**: docs(daemon): design record for roster-driven role-runner shard assignment (#6704)
- **#7699**: fix(config): replace live safehouse room / observability endpoint with placeholders

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#7444**: feat(spawn-claude): per-sweep container resource limits + containment observability
- **#7467**: fix(worktree): refuse stale-worktree reset when a live process holds it open
- **#7496**: fix(guard): distinguish escaped from live backtick/$( in --body masking
- **#7519**: fix(guard): stop hard-denying for-loop wordlists whose only consumer is a jq --arg filter script (#7515)
- **#7533**: guard: extend force-op:detached safe-list to a worktree's own branch (#7530)
- **#7536**: tokens: expire session-limit bad-marks with their own 5h window
- **#7594**: fix: back off and surface permanently-failing worktree removals (#7590)
- **#7683**: ci: add warn-only guard-destructive vendored/canonical drift check

## Proposed

Issues carrying `loom:curated`.

- **#4136**: measure: every sweep phase re-reads the repo from scratch — quantify the duplicated-read cost *(curated)*
- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#5660**: Vendored guard-destructive-generic.sh has drifted ~2,200 lines ahead of its upstream, and the single-marker capability probe makes partial reconciliation unsafe *(curated)*
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space *(curated)*
- **#6565**: Dogfood config: loom-repo curator starved 3d — runtime=codex admitted with no codex model configured (#5028 skip, DEBUG-silent) *(curated)*
- **#6646**: Sweep resync committed, rebased and bypass-pushed the primary clone's main while an operator session was active in that clone *(curated)*
- **#6650**: .loom/config.json commits a live Matrix room id and ingest URL — intentional, or move to the private overlay tier? *(curated)*
- **#6704**: Roster-driven role-runner shard assignment: reassign a dead host's slice within a bounded window (follow-up to #6374's static ring) *(curated)*
- **#6969**: auto_update drain-and-restart: one relaunch waited ~4 min for the watchdog instead of launchd (KeepAlive.SuccessfulExit) — single observation *(curated)*
- **#7359**: merge=ours driver on .loom/install-metadata.json can silently drop non-loom_version field edits during rebase, uncaught by version-check-gate.sh *(curated)*
- **#7430**: [Epic #6896] Phase 3: Per-sweep resource limits and containment observability *(curated)*
- **#7463**: Dispatch-time worktree prep can reset/clean a worktree while orphaned processes from a prior interrupted session are still writing into it *(curated)*
- **#7515**: Guard false positive: catastrophic:aws s3 rb hard-denies for-loop wordlists with no live aws invocation, post-#7292 *(curated)*
- **#7522**: tokens: readmit session-limit accounts when the 5h window resets instead of holding them for the 6h exhaustion cooldown *(curated)*
- **#7530**: Guard friction: force-op:detached ASKs on a Loom worktree resetting to its OWN feature branch's origin tip *(curated)*
- **#7590**: worktree_reaper retries and fails forever on root-owned build-cache files, no backoff or health visibility *(curated)*
- **#7657**: Champion: close a proposal whose central premise is verified false instead of escalating it as an operator decision *(curated)*
- **#7659**: Proposer roles: cite only paths that exist in the dispatched workspace; sibling checkouts named in a repo docs are context, never a citation target *(curated)*
- **#7664**: watchdog: peer-coordination escalation needs hysteresis and dedup — refiles a tracking issue on every self-recovering flap *(curated)*
- **#7665**: champion-pr-merge.md: one remaining 'for file in $FILES' loop (line 1092) breaks under zsh *(curated)*
- **#7666**: champion: a passing, already-decomposed epic is escalated to the operator after repeated identical stand-down passes *(curated)*
- **#7678**: merge-pr.sh silently exits 1 on every PR without a champion:hold-state marker (regression in #7435/bf4f1965) *(curated)*
- **#7694**: Builder: probe for a live sibling worktree before worktree.sh touches files (generalizing #6765) *(curated)*

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
| Ready (`loom:issue`) | 8 |
| In Progress (`loom:building`) | 3 |
| PRs awaiting review | 6 |
| Approved PRs awaiting merge | 8 |
| Curated | 23 |
| Architect / Hermit proposals | 4 |
| Active epics | 3 |
<!-- guide:plan-body:end -->
