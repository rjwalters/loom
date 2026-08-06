# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#5508**: Role-runner spawned sessions (Judge/Champion/etc.) inherit the daemon's own GH_CONFIG_DIR instead of the per-owner one — 404s on every non-default-owner repo

## Ready

Human-approved issues ready for implementation (`loom:issue`).

_None._

## In Progress

Issues currently being built (`loom:building`).

- **#5508**: Role-runner spawned sessions (Judge/Champion/etc.) inherit the daemon's own GH_CONFIG_DIR instead of the per-owner one — 404s on every non-default-owner repo
- **#5501**: live_state_sandbox guards state paths but not launchd/systemd labels — a sandboxed test can stop the real daemon

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#5507**: fix: guard test sandbox supervisor identity, not just state paths

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5508**: Role-runner spawned sessions (Judge/Champion/etc.) inherit the daemon's own GH_CONFIG_DIR instead of the per-owner one — 404s on every non-default-owner repo *(curated)*
- **#5501**: live_state_sandbox guards state paths but not launchd/systemd labels — a sandboxed test can stop the real daemon *(curated)*
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
| Urgent | 1 |
| Ready (`loom:issue`) | 0 |
| In Progress (`loom:building`) | 2 |
| PRs awaiting review | 1 |
| Approved PRs awaiting merge | 1 |
| Curated | 6 |
| Architect / Hermit proposals | 3 |
| Active epics | 2 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-06, ~05:12 UTC pass):** WORK_LOG.md gained one entry pair (Issue #5499 closed, PR #5509 merged — Codex ChatGPT-plan-seat model-pin fix). WORK_PLAN.md regenerated: marked #5508 `loom:urgent` (live bug: role-runner sessions have used the wrong `GH_CONFIG_DIR` and been blind to the entire `2AMLogic/*` queue for 6+ hours) — it was then claimed by a Builder mid-pass, so it now shows in both Urgent and In Progress. Corrected a stale label on #5501: its `loom:building` had been silently reset to `loom:issue` at 05:05:12Z despite an open, mergeable, `Closes #5501` PR (#5507) already in Doctor treatment — restored `loom:building`, commented with the evidence, and filed #5511 to track the underlying recovery-logic bug. Re-verified all 7 `loom:blocked` issues (#5385, #5329, #4496, #4196, #4167, #4136, #3979): none carry a parseable `Depends on #N` list except #4496 (unchanged from prior pass — still gated on `loom:operator-only`, not a code dependency). Epic #4489 unchanged at 6/7 phases closed (Phase 7 = #4496, operator-gated). Epic #5038 appears fully complete (all filed phase issues closed) — flagged for Champion/Curator to consider closing; Guide does not close epics.
