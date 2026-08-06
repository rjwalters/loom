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

- **#5501**: live_state_sandbox guards state paths but not launchd/systemd labels — a sandboxed test can stop the real daemon
- **#5508**: Role-runner spawned sessions (Judge/Champion/etc.) inherit the daemon's own GH_CONFIG_DIR instead of the per-owner one — 404s on every non-default-owner repo

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#5507**: fix: guard test sandbox supervisor identity, not just state paths

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5501**: live_state_sandbox guards state paths but not launchd/systemd labels — a sandboxed test can stop the real daemon *(curated)*
- **#5385**: Worktree-isolation guard fails closed on variable-expanded write paths that resolve outside the main checkout *(curated)*
- **#5038**: Design: who owns continuous maintenance? Split by determinism and granularity, not topic — and why a janitor agent cannot own install repair *(curated)*
- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#4136**: measure: every sweep phase re-reads the repo from scratch — quantify the duplicated-read cost *(curated)*
- **#5508**: Role-runner spawned sessions (Judge/Champion/etc.) inherit the daemon's own GH_CONFIG_DIR instead of the per-owner one — 404s on every non-default-owner repo *(curated)*

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

**Assessment (2026-08-06, ~05:13 UTC pass):** WORK_LOG.md appended one entry — Issue #5499 (closed) and its PR #5509 (fix a Codex ChatGPT-plan-seat model-pin 400 misclassified as RECOVERABLE) — the only new content above the prior watermark (PR #5503 / Issue #5497). WORK_PLAN.md regenerated: the ready queue (#5508, #5501) that existed at the start of this pass was claimed by Builders mid-cycle — both now show `loom:building`, confirmed live (age 1-2 min, well inside `loom-recover-orphans`'s 4h staleness threshold, no orphans). #5508 already carries `loom:urgent` (pre-existing, not added by this pass — it is legitimately the fleet's top priority: a 6+ hour-old role-runner credential misconfiguration silently blocking Judge/Champion evaluation on every `2AMLogic/*` repo) and is left alone per policy (never touch labels on issues already building). No new issues were available to triage into Urgent/Ready this pass. Re-verified all 7 `loom:blocked` issues (#5385, #5329, #4496, #4196, #4167, #4136, #3979): none carry a parseable `Depends on #N` reference this pass either — all remain correctly parked, consistent with prior passes. Epics unchanged: #4489 at 6/6 filed phases closed (Phase 7 = #4496, operator-gated, blocked); #5038 at 2 filed phases closed (#5488, #5489), no new phase issues filed, not stale.
