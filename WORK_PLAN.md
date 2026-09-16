# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

_None._

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space
- **#7893**: [#4196 Phase 3a] Daemon ChatOps command enum + allowlisted senders + confirm-nonce for inbound safehouse steering

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#4765**: feat(champion): opt-in flag to auto-merge Dependabot dependency PRs
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space
- **#7894**: role_runner: an unpinned codex `runtimes.roles` binding resolves the default `sonnet` and skips forever (ModelRuntimeMismatch) — resolve a runtime-appropriate default or support a 'CLI default' pin
- **#7915**: test-isolation: pr_set_dispatch_exports_no_lease_renewal_marker fails when run from inside a sweep (ambient LOOM_SWEEP_LEASE_RENEW_DISPATCHED leaks into the child)
- **#7919**: check-defaults-version-bump.sh default mode advises a VERSION bump that CI's --forbid-bump job rejects
- **#7929**: epic #7810 PR 3: retire classify-dependency-block.sh + its two sourced helpers (861 code lines) into loom-daemon
- **#7935**: SweepRegistry entry survives its child's death when the pid is recycled — blocks `restart --drain` and every auto_update roll
- **#7952**: epic #7810 PR 3 (final): cycle walk + CLI subcommands + stubs, and delete 861 lines of shell

## In Progress

Issues currently being built (`loom:building`).

- **#7708**: work_finder: dispatching into a pool with zero usable accounts produces a 4-host re-dispatch storm — 228 token-selection deaths in 4h, 39 lease comments on one issue; needs a sweep pre-flight + host-level exhaustion hold
- **#7818**: post_init managed .gitignore omits .loom/gh-config/ — a resync commit swept a live App installation token into a public repo
- **#7873**: dispatch_sweep refuses any issue lacking loom:issue as a 'cross-host collision' (classify_preflip_labels treats never-labeled as peer-removed)
- **#7874**: flaky: test-rebase-stacked-children.sh source-guard assertions fail under concurrency in run-ci-suites.sh
- **#7893**: [#4196 Phase 3a] Daemon ChatOps command enum + allowlisted senders + confirm-nonce for inbound safehouse steering
- **#7949**: docs(token-pool): correct the kicad-tools#5333 comment count in the #7860 forensic record (1,316 -> 187)

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#7953**: feat(daemon): port the dependency-cycle walk, preserving its memoisation invariant (#7952)

## Proposed

Issues carrying `loom:curated`.

- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#6544**: provision-hooks.sh emits unquoted ${CLAUDE_PROJECT_DIR} — every hook breaks when the project path contains a space *(curated)*
- **#7708**: work_finder: dispatching into a pool with zero usable accounts produces a 4-host re-dispatch storm — 228 token-selection deaths in 4h, 39 lease comments on one issue; needs a sweep pre-flight + host-level exhaustion hold *(curated)*
- **#7818**: post_init managed .gitignore omits .loom/gh-config/ — a resync commit swept a live App installation token into a public repo *(curated)*
- **#7873**: dispatch_sweep refuses any issue lacking loom:issue as a 'cross-host collision' (classify_preflip_labels treats never-labeled as peer-removed) *(curated)*
- **#7874**: flaky: test-rebase-stacked-children.sh source-guard assertions fail under concurrency in run-ci-suites.sh *(curated)*
- **#7893**: [#4196 Phase 3a] Daemon ChatOps command enum + allowlisted senders + confirm-nonce for inbound safehouse steering *(curated)*
- **#7894**: role_runner: an unpinned codex `runtimes.roles` binding resolves the default `sonnet` and skips forever (ModelRuntimeMismatch) — resolve a runtime-appropriate default or support a 'CLI default' pin *(curated)*
- **#7915**: test-isolation: pr_set_dispatch_exports_no_lease_renewal_marker fails when run from inside a sweep (ambient LOOM_SWEEP_LEASE_RENEW_DISPATCHED leaks into the child) *(curated)*
- **#7919**: check-defaults-version-bump.sh default mode advises a VERSION bump that CI's --forbid-bump job rejects *(curated)*
- **#7935**: SweepRegistry entry survives its child's death when the pid is recycled — blocks `restart --drain` and every auto_update roll *(curated)*
- **#7949**: docs(token-pool): correct the kicad-tools#5333 comment count in the #7860 forensic record (1,316 -> 187) *(curated)*
- **#7952**: epic #7810 PR 3 (final): cycle walk + CLI subcommands + stubs, and delete 861 lines of shell *(curated)*

## Proposed (Architect / Hermit)

- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
- **#6109**: Add a runtime-neutral scientific research lifecycle with evidence-gated phase contracts
- **#6896**: Epic: Session containers — persistent Codex auth, mandatory worker containment, and a remote-execution job seam
- **#7810**: [epic] Retire shell: 10 sequential PRs, two foundational then update + agent execution

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 0 |
| Urgent | 3 |
| Ready (`loom:issue`) | 8 |
| In Progress (`loom:building`) | 6 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 1 |
| Curated | 13 |
| Architect / Hermit proposals | 2 |
| Active epics | 4 |
<!-- guide:plan-body:end -->
