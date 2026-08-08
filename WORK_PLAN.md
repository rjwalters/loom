# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#5629**: Role-spawn token selection (mode=random) hands out accounts marked in tokens-exhausted; monthly-spend-limit errors retried as RECOVERABLE
- **#5607**: tokens: record (provider, upstream account id) in the pool storage layer

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#5629**: Role-spawn token selection (mode=random) hands out accounts marked in tokens-exhausted; monthly-spend-limit errors retried as RECOVERABLE
- **#5607**: tokens: record (provider, upstream account id) in the pool storage layer
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## In Progress

Issues currently being built (`loom:building`).

- **#5656**: architect is not in DEFAULT_ROLES, so a repo whose backlog empties has no way to acquire new work

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#5667**: fix(daemon): stop gating role-runner loop spawn on one workspace's own roleRunner.roles

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5636**: fix(tokens): propagate .ranking exhausted/blocked exclusions to the allowlist and random tiers
- **#5619**: tokens: record (provider, upstream account id) in the pool storage layer
- **#5569**: fix(fleet): idle-shutdown guard asks daemon eligibility instead of vetoing on bare process presence
- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5671**: loom:operator-only carries at least four meanings with no way to tell them apart *(curated)*
- **#5664**: champion escalates dependency-blocked proposals to operator-only and never un-escalates when the blocker clears *(curated)*
- **#5660**: Vendored guard-destructive-generic.sh has drifted ~2,200 lines ahead of its upstream, and the single-marker capability probe makes partial reconciliation unsafe *(curated)*
- **#5656**: architect is not in DEFAULT_ROLES, so a repo whose backlog empties has no way to acquire new work *(curated)*
- **#5629**: Role-spawn token selection (mode=random) hands out accounts marked in tokens-exhausted; monthly-spend-limit errors retried as RECOVERABLE *(curated)*
- **#5607**: tokens: record (provider, upstream account id) in the pool storage layer *(curated)*
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision *(curated)*
- **#5512**: Quarantine stashes accumulate with no lifecycle — 37 across one fleet, oldest 9 days, all referencing closed issues *(curated)*
- **#5385**: Worktree-isolation guard fails closed on variable-expanded write paths that resolve outside the main checkout *(curated)*
- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#4136**: measure: every sweep phase re-reads the repo from scratch — quantify the duplicated-read cost *(curated)*

## Proposed (Architect / Hermit)

- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*
- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#3979**: Architecture: elastic compute — expand sweep parallelism onto cloud worker hosts when local CPU saturates *(architect)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management

## Backlog Balance

| Tier | Count |
|------|-------|
| Urgent | 2 |
| Ready (`loom:issue`) | 3 |
| In Progress (`loom:building`) | 1 |
| PRs awaiting review | 1 |
| Approved PRs awaiting merge | 4 |
| Curated | 11 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->
