# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#5904**: fix(daemon): treat stale worktree registrations as already-removed in loom-daemon clean
- **#5899**: chore: resync installed Loom surfaces
- **#5684**: fix: correct BSD sed -i separate-suffix write-target resolution
- **#5683**: fix(guard): count only unescaped/unquoted pipes in read-only fast path (#5673)
- **#5681**: fix(guard-loom-workflow): mask unquoted-delimiter cat-heredoc bodies captured into text-data flags
- **#5636**: fix(tokens): propagate .ranking exhausted/blocked exclusions to the allowlist and random tiers
- **#5619**: tokens: record (provider, upstream account id) in the pool storage layer
- **#5569**: fix(fleet): idle-shutdown guard asks daemon eligibility instead of vetoing on bare process presence
- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#5961**: loom-daemon: process-global PATH mutation in pipeline_snapshot test poisons ~258 concurrent tests, making the build gate unusable
- **#5929**: WORK_PLAN debounce (#5890/#5892) anchors to any docs-PR merge, not to the WORK_PLAN diff itself — can suppress an overdue rewrite indefinitely

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#5929**: WORK_PLAN debounce (#5890/#5892) anchors to any docs-PR merge, not to the WORK_PLAN diff itself — can suppress an overdue rewrite indefinitely
- **#5895**: loom-daemon clean fails on stale worktree registrations — no git worktree prune before worktree remove, and --dry-run cannot see it
- **#5673**: Guard read-only fast path (#5274) still denies sql-ddl when the grep pattern argument itself contains an escaped/quoted pipe
- **#5672**: Guard false positive: loom:gh-pr-merge-redirect denies gh pr comment bodies that merely quote/discuss 'gh pr merge' in prose
- **#5607**: tokens: record (provider, upstream account id) in the pool storage layer
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## In Progress

Issues currently being built (`loom:building`).

- **#5961**: loom-daemon: process-global PATH mutation in pipeline_snapshot test poisons ~258 concurrent tests, making the build gate unusable
- **#5629**: Role-spawn token selection (mode=random) hands out accounts marked in tokens-exhausted; monthly-spend-limit errors retried as RECOVERABLE

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5952**: fix(guide): anchor WORK_PLAN debounce to a WORK_PLAN.md write, not any docs-PR merge
- **#5904**: fix(daemon): treat stale worktree registrations as already-removed in loom-daemon clean
- **#5899**: chore: resync installed Loom surfaces
- **#5684**: fix: correct BSD sed -i separate-suffix write-target resolution
- **#5683**: fix(guard): count only unescaped/unquoted pipes in read-only fast path (#5673)
- **#5681**: fix(guard-loom-workflow): mask unquoted-delimiter cat-heredoc bodies captured into text-data flags
- **#5636**: fix(tokens): propagate .ranking exhausted/blocked exclusions to the allowlist and random tiers
- **#5619**: tokens: record (provider, upstream account id) in the pool storage layer
- **#5569**: fix(fleet): idle-shutdown guard asks daemon eligibility instead of vetoing on bare process presence
- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5961**: loom-daemon: process-global PATH mutation in pipeline_snapshot test poisons ~258 concurrent tests, making the build gate unusable *(curated)*
- **#5929**: WORK_PLAN debounce (#5890/#5892) anchors to any docs-PR merge, not to the WORK_PLAN diff itself — can suppress an overdue rewrite indefinitely *(curated)*
- **#5897**: sweep skill: presumed-dead Builder Task can survive a session roll — verify task liveness before re-dispatch (duplicate-builder hazard) *(curated)*
- **#5895**: loom-daemon clean fails on stale worktree registrations — no git worktree prune before worktree remove, and --dry-run cannot see it *(curated)*
- **#5729**: loom-daemon is DOWN on robb-studio and watchdog recovery is exhausted *(curated)*
- **#5674**: Guard false positive: worktree-write-confinement denies cp/mv writes to /tmp or fully in-repo tmp-then-rename, unrelated to the main checkout *(curated)*
- **#5660**: Vendored guard-destructive-generic.sh has drifted ~2,200 lines ahead of its upstream, and the single-marker capability probe makes partial reconciliation unsafe *(curated)*
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
| Operator merge-risk holds | 9 |
| Urgent | 2 |
| Ready (`loom:issue`) | 6 |
| In Progress (`loom:building`) | 2 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 10 |
| Curated | 14 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->
