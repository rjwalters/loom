# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

_None._

## Ready

Human-approved issues ready for implementation (`loom:issue`).

_None._

## In Progress

Issues currently being built (`loom:building`).

_None._

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5546**: loom-daemon is DOWN on robb-pro and watchdog recovery is exhausted *(curated)*
- **#5542**: dashboard /public/history has no time filter or cursor and caps at 500 — a bounded window cannot be read to completion *(curated)*
- **#5512**: Quarantine stashes accumulate with no lifecycle — 37 across one fleet, oldest 9 days, all referencing closed issues *(curated)*
- **#5385**: Worktree-isolation guard fails closed on variable-expanded write paths that resolve outside the main checkout *(curated)*
- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#4136**: measure: every sweep phase re-reads the repo from scratch — quantify the duplicated-read cost *(curated)*

## Proposed (Architect / Hermit)

- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*
- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#3979**: Architecture: elastic compute — expand sweep parallelism onto cloud worker hosts when local CPU saturates *(architect)*

## Epics

- **#5038**: Design: who owns continuous maintenance? Split by determinism and granularity, not topic — and why a janitor agent cannot own install repair
- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management

## Backlog Balance

| Tier | Count |
|------|-------|
| Urgent | 0 |
| Ready (`loom:issue`) | 0 |
| In Progress (`loom:building`) | 0 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 1 |
| Curated | 6 |
| Architect / Hermit proposals | 3 |
| Active epics | 2 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-06, ~15:49 UTC pass):** Ready queue (`loom:issue`) and `loom:urgent` are both still 0 — nothing to prioritize or demote this pass. All 6 `loom:curated` issues checked individually and confirmed non-promotable for legitimate reasons: `loom:operator-only` (#5546 — needs a human with shell access to robb-pro; #4496 — Codex canary needs an operator-authenticated profile), Champion-escalated-to-operator after repeated rejection without revision (#5512), blocked on an open, Doctor-cycle-capped PR (#5385 ← PR #5397, `CHAIN_NOT_CONVERGING`), and Champion-parked pending a Curator re-verification of a telemetry-dependency claim (#4136). #5542 is newly curated (no blocking label) and simply awaiting its first Champion review — normal flow, not a gap. This matches the benign empty-ready-queue pattern already logged three times today (07:40Z, 09:53Z, 14:32Z); still not a Champion-promotion-liveness bug. Re-checked all 6 `loom:blocked` issues (#5385, #5329, #4196, #4167, #4136, #3979) for parseable `Blocked by/Depends on/Requires #N` references and for a superseding open/blocked linked PR (#4634 gate) — #5329's external `2AMLogic/2am` deploy-workflow gate remains unmet (`total_count: 0`, third consecutive re-check, Curator's own dep-recheck dedup convention now applies), the rest are qualitative Champion/Curator holds with no parseable dependency; none unblocked. `loom-recover-orphans` scope: 0 `loom:building` issues, nothing to recover. Scanned the last 15 merged PRs (5 non-docs: #5541, #5537, #5534, #5533, #5531) — all used proper `Closes #N` syntax with issues confirmed CLOSED (#5539, #5511, #5517, #5523, #5516); no orphaned-open-issue cleanup needed. Epics unchanged and healthy: #4489 at 6/7 phases closed (Phase 7 = #4496, operator-gated); #5038 at 2/2 known phase-issues closed (#5488, #5489), Phase 4 (`janitor` role) still contingent on residue from phases 1-3. `loom:review-requested` / `loom:changes-requested`: 0 and 1 (#5397, known Doctor-cycle-capped) — nothing new. WORK_LOG.md: no new merged PRs or closed issues since the 14:32Z pass (checked the last 30 days of both by presence-check, not watermark, per #5516/#5539's fix). WORK_PLAN.md's Proposed section updated to add #5546 and #5542 (curated since the prior pass).
