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

- **#5499**: Codex: a roleModels pin that a ChatGPT-plan seat cannot serve fails as RECOVERABLE and retries forever *(curated)*
- **#5385**: Worktree-isolation guard fails closed on variable-expanded write paths that resolve outside the main checkout *(curated)*
- **#5038**: Design: who owns continuous maintenance? Split by determinism and granularity, not topic — and why a janitor agent cannot own install repair *(curated)*
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
| Curated | 5 |
| Architect / Hermit proposals | 3 |
| Active epics | 2 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-06, ~03:28 UTC pass):** `loom:urgent` remains empty, and for the first time in recent passes the `loom:issue` Ready queue is genuinely empty too (not just blocked-while-labeled) — the dual-label `loom:issue`+`loom:blocked` cases flagged in prior passes (#5232, #4889, #4767) are gone from the open-issue set, consistent with the human batch-merge of their Judge-approved fixes (#5233/#4918/#4770) recorded in memory. Approved-PRs-awaiting-merge dropped from 9 to 1 (#5485) as the six deps-bump PRs and three guard/worktree fixes merged. Re-checked all 7 `loom:blocked` issues (#5385, #5329, #4496, #4196, #4167, #4136, #3979) for parseable `Depends on #N` dependencies — none found; each is held for an architectural/operator/external-gate reason already recorded in memory (e.g. #5385 parked behind PR #5397's capped Doctor-cycle chain; #4496 is Epic #4489 Phase 7, fully dependency-clear per its own checklist but explicitly operator-gated; #5329 waits on an external `2AMLogic/2am` deploy workflow going green). No orphan-recovery action needed (`loom:building` is empty). Epic #4489 unchanged at 6/7 phases complete (Phase 7 = #4496, operator-gated canary). Epic #5038's two filed phase issues (#5488, #5489) are both closed; no further phase issues found for it. New curated item this pass: #5499. WORK_LOG.md updated for 5 items above the prior watermark (PR #5494 / Issue #5489): PRs #5503, #5500, #5498 and closed issues #5495, #5497 — all genuine new merges/closures, none self-referential docs PRs.
