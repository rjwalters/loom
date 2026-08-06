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
| Curated | 4 |
| Architect / Hermit proposals | 3 |
| Active epics | 2 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-06, ~14:30 UTC pass):** Appended WORK_LOG.md entries for PR #5541 and Issue #5539 — the only merged PR / closed issue since the prior pass not already recorded (all other PRs merged in the window were this phase's own `docs: Guide document maintenance update` PRs, correctly excluded). The `loom:issue` ready queue and `loom:urgent` are both 0 — there is currently nothing in the Builder queue at all, so no priority-setting action was needed this pass. `loom:building` is also 0 (issue #5539 closed via PR #5541 since the prior WORK_PLAN snapshot), so the committed "In Progress" and "Proposed" sections were stale and have been regenerated. `loom-recover-orphans --verbose` found zero orphans. Re-checked all 6 `loom:blocked` issues (#5385, #5329, #4196, #4167, #4136, #3979) for parseable `Blocked by/Depends on/Requires #N` references: none match the pattern (design proposals, an external cross-repo gate, and one prose-only "Depends on ... companion issue" with no issue number) — none are auto-unblockable this pass. Note: #4496 (Epic #4489 Phase 7) currently carries `loom:curated`, not `loom:blocked` — it appears in "Proposed" above, not among the blocked issues. Epics: #4489 has 6 phase issues, all closed, but its own completion checklist is still unchecked and Phase 7 (#4496) has not yet been promoted past curation — not stale (last phase closed 2026-07-31, 6 days ago). #5038 has 2 `loom:epic-phase` children (#5488, #5489), both closed as of 2026-08-05 — active, not stale. Verified all 7 non-docs PRs merged since the prior pass (#5541, #5537, #5534, #5533, #5531, #5525, #5524) used proper `Closes #N` syntax and their referenced issues (#5539, #5511, #5517, #5523, #5516, #5504, #5510) are confirmed CLOSED — no orphaned-open-issue cleanup needed. `loom:review-requested` is empty — nothing awaiting Judge.
