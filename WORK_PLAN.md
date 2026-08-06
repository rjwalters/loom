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

**Assessment (2026-08-06, ~14:32 UTC pass):** PR #5541 (merged today, `Closes #5539`) fixed the issue-side counterpart of the #5516 watermark bug — WORK_LOG.md's closed-issue tracking previously used a pure number watermark that permanently dropped out-of-order closures. Re-running the presence-check scan (bounded at the documented 2026-08-05 gap-notice floor per WORK_LOG.md's own "Historical gap notice" section, which explicitly set that date as the resumption point for a ~5.5-month non-exhaustive backfill — NOT re-litigated here) surfaced 26 real drops: 7 PRs (#5541, #4941, #5132, #4972, #4770, #4918, #4940) and 19 issues (#5539, #5511, #5232, #4928, #4889, #4767, #5266, #5131, #5007, #4607, #5063, #4702, #4057, #4859, #4993, #4992, #4996, #5062, #4933) across 2026-08-05/06. All now appended to WORK_LOG.md. Ready queue (`loom:issue`) is 0 with all 4 remaining `loom:curated` issues correctly non-promotable (#5512/#4496 `loom:operator-only`, #5385/#4136 `loom:blocked`) — matches the benign pattern already logged twice today (07:40Z, 09:53Z); not a Champion stall. `loom-recover-orphans --recover` found zero orphans (0 `loom:building`). Re-checked all 6 `loom:blocked` issues (#5385, #5329, #4196, #4167, #4136, #3979) for parseable `Blocked by/Depends on/Requires #N` dependency references — none found on any; all require human/manual review (proposals or qualitative gates), consistent with prior passes. Epics: #4489 at 6/7 phases closed (Phase 7 = #4496, operator-gated on a Codex canary); #5038 now shows all 3 known phases closed (#5488, #5489, and #5035 confirmed closed) — Phase 4 (`janitor` role) remains contingent on whether phases 1-3 leave a genuine residue, per the epic's own "Suggested phasing" text; neither epic is stale. Verified the 4 most-recently-merged non-docs PRs (#5541, #5537, plus the earlier-batch #4940/#4918/#4770) all used proper `Closes #N` syntax with their issues confirmed CLOSED — no orphaned-open-issue cleanup needed. `loom:review-requested` and `loom:changes-requested` both checked: 0 and 1 (#5397, a known Doctor-cycle-capped PR per prior Champion note) respectively — nothing new to flag.
