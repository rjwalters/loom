# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#5543**: dashboard-deploy is pinned by a miniflare isolated-storage flake — the live Worker has not redeployed since 08-05T07:28Z

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#5543**: dashboard-deploy is pinned by a miniflare isolated-storage flake — the live Worker has not redeployed since 08-05T07:28Z

## In Progress

Issues currently being built (`loom:building`).

_None._

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#5550**: fix(dashboard-deploy): patch miniflare isolated-storage flake, retry-once + failed-test-gate visibility

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5546**: loom-daemon is DOWN on robb-pro and watchdog recovery is exhausted *(curated)*
- **#5543**: dashboard-deploy is pinned by a miniflare isolated-storage flake — the live Worker has not redeployed since 08-05T07:28Z *(curated)*
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
| Urgent | 1 |
| Ready (`loom:issue`) | 1 |
| In Progress (`loom:building`) | 0 |
| PRs awaiting review | 1 |
| Approved PRs awaiting merge | 1 |
| Curated | 6 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-06, ~22:27 UTC pass):** #5543 (dashboard-deploy miniflare flake) is now `loom:issue` + `loom:urgent` with a PR open (#5550, `loom:review-requested`) — active and moving normally; left as the sole urgent issue (well under the max-3 cap, no demotion needed). #5038 (janitor-role epic) closed since the last pass, dropping Active epics from 2 to 1; #4489 remains at 6/7 phases closed (Phase 7 = #4496, operator-gated for a Codex canary). Re-checked all 6 `loom:blocked` issues (#5385, #5329, #4196, #4167, #4136, #3979) for parseable `Blocked by/Depends on/Requires #N` references — none found on any of them, so none are eligible for the dependency-based unblock path; #5385's superseding block (PR #5397, `CHAIN_NOT_CONVERGING`, Doctor-cycle-capped) is still active per its own comment thread, and #5329's external `2AMLogic/2am` gate remains unverifiable (repo returns 404 to this token — not evidence the gate cleared). `loom-recover-orphans` scope: 0 `loom:building` issues, nothing to recover. Verified the 3 most recently merged non-docs PRs (#5554, #5553, #5551) all used proper `Closes #N` syntax with their issues confirmed CLOSED (#5548, #5552, #5542) — no orphaned-open-issue cleanup needed. WORK_LOG.md: appended entries for #5554/#5548, #5553/#5552, #5551/#5542 (new since the prior pass, found via presence-check over the last 30 days per #5516/#5539's fix, not a number watermark). Curated queue (6) unchanged in composition from last pass aside from #5543 promoting out of "awaiting Champion" and into `loom:issue`; the remaining 5 stay non-promotable for the same reasons logged previously (`loom:operator-only` on #5546/#4496, Champion-escalated #5512, blocked #5385/#4136).
