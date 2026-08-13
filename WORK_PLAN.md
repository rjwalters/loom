# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#6148**: fix(clean): keep build-artifact dirs that back a running program
- **#6118**: fix(merge-pr): recheck stale mergeable=false before refusing a merge
- **#6018**: fix(install): stop the reinstall sweep from deleting repo-owned files under .loom/
- **#5999**: fix(uninstall): preserve live .loom/worktrees/* by default in --local mode
- **#5995**: chore: resync installed Loom surfaces
- **#5619**: tokens: record (provider, upstream account id) in the pool storage layer

## Urgent

Issues flagged as highest priority (`loom:urgent`).

_None._

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#6127**: clean --deep deletes a running service's binary — target/release/safehoused is reaped by fleet-clean, fatal on next restart
- **#6104**: merge-pr.sh refuses PRs on GitHub's stale cached mergeable state when git merges them cleanly
- **#5973**: Reinstall silently removes existing .loom/worktrees/issue-* worktrees
- **#5607**: tokens: record (provider, upstream account id) in the pool storage layer

## In Progress

Issues currently being built (`loom:building`).

_None._

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#6148**: fix(clean): keep build-artifact dirs that back a running program
- **#6118**: fix(merge-pr): recheck stale mergeable=false before refusing a merge
- **#6018**: fix(install): stop the reinstall sweep from deleting repo-owned files under .loom/
- **#5999**: fix(uninstall): preserve live .loom/worktrees/* by default in --local mode
- **#5995**: chore: resync installed Loom surfaces
- **#5619**: tokens: record (provider, upstream account id) in the pool storage layer

## Proposed

Issues carrying `loom:curated`.

- **#6134**: Guide/Judge/Champion: fast-path review+merge for docs-only WORK_LOG/WORK_PLAN PRs *(curated)*
- **#6127**: clean --deep deletes a running service's binary — target/release/safehoused is reaped by fleet-clean, fatal on next restart *(curated)*
- **#6123**: Guard friction: worktree-write-confinement blocks gitignored build-artifact writes in the primary checkout *(curated)*
- **#6104**: merge-pr.sh refuses PRs on GitHub's stale cached mergeable state when git merges them cleanly *(curated)*
- **#6062**: Pulse (2amlogic.com) narrates closed-unmerged duplicate efforts; should narrate on merge / dedup by merged-PR *(curated)*
- **#5973**: Reinstall silently removes existing .loom/worktrees/issue-* worktrees *(curated)*
- **#5897**: sweep skill: presumed-dead Builder Task can survive a session roll — verify task liveness before re-dispatch (duplicate-builder hazard) *(curated)*
- **#5729**: loom-daemon is DOWN on robb-studio and watchdog recovery is exhausted *(curated)*
- **#5660**: Vendored guard-destructive-generic.sh has drifted ~2,200 lines ahead of its upstream, and the single-marker capability probe makes partial reconciliation unsafe *(curated)*
- **#5607**: tokens: record (provider, upstream account id) in the pool storage layer *(curated)*
- **#5512**: Quarantine stashes accumulate with no lifecycle — 37 across one fleet, oldest 9 days, all referencing closed issues *(curated)*
- **#5385**: Worktree-isolation guard fails closed on variable-expanded write paths that resolve outside the main checkout *(curated)*
- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#4136**: measure: every sweep phase re-reads the repo from scratch — quantify the duplicated-read cost *(curated)*

## Proposed (Architect / Hermit)

- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*
- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#3979**: Architecture: elastic compute — expand sweep parallelism onto cloud worker hosts when local CPU saturates *(architect)*

## Epics

- **#6109**: Add a runtime-neutral scientific research lifecycle with evidence-gated phase contracts
- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 6 |
| Urgent | 0 |
| Ready (`loom:issue`) | 4 |
| In Progress (`loom:building`) | 0 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 6 |
| Curated | 14 |
| Architect / Hermit proposals | 3 |
| Active epics | 2 |
<!-- guide:plan-body:end -->
