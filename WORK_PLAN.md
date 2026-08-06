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

- **#5501**: live_state_sandbox guards state paths but not launchd/systemd labels — a sandboxed test can stop the real daemon
- **#5499**: Codex: a roleModels pin that a ChatGPT-plan seat cannot serve fails as RECOVERABLE and retries forever

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5501**: live_state_sandbox guards state paths but not launchd/systemd labels — a sandboxed test can stop the real daemon *(curated)*
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
| In Progress (`loom:building`) | 2 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 1 |
| Curated | 6 |
| Architect / Hermit proposals | 3 |
| Active epics | 2 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-06, ~04:27 UTC pass):** No change to WORK_LOG.md — the only merged PR above the prior watermark (PR #5503) was #5505, this phase's own previous docs PR, correctly excluded by the self-referential filter. WORK_PLAN.md regenerated: `loom:building` went from 0→2 (#5501, #5499 both claimed and confirmed live via `loom-recover-orphans --verbose`, no orphans), and Curated grew 5→6 (#5501 newly curated). Urgent and Ready both remain empty — nothing to prioritize this pass. Re-verified all 7 `loom:blocked` issues (#5385, #5329, #4496, #4196, #4167, #4136, #3979): none carry a parseable `Depends on #N` list except #4496, whose 7 body-declared prerequisites (#4490-#4495, #4478) are all confirmed CLOSED — but it stays blocked because it carries `loom:operator-only` and Curator has already re-confirmed twice (2026-08-04, 2026-08-05) that the sole remaining gate is live operator credential/budget authorization, not a code dependency; it also never carried `loom:issue`, so no restore would apply even if unblocked. Epic #4489 unchanged at 6/7 phases closed (Phase 7 = #4496, operator-gated). Epic #5038: all 3 filed phases (#5035, #5488, #5489) closed; Phase 4 (janitor role) remains conditional/unfiled per its own "only if residue" criterion — not stale, no action.
