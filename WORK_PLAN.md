# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## In Progress

Issues currently being built (`loom:building`).

- **#5601**: role_runner: hermit is not in DEFAULT_ROLES, so it can never be dispatched

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5569**: fix(fleet): idle-shutdown guard asks daemon eligibility instead of vetoing on bare process presence
- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5605**: design: token pool identity should be (provider, account_id), not email — blocks multi-runtime (codex/kimi/qwen) *(curated)*
- **#5604**: tokens import-from-monitor: email-only keying lets a non-Anthropic credential occupy an Anthropic pool slot *(curated)*
- **#5601**: role_runner: hermit is not in DEFAULT_ROLES, so it can never be dispatched *(curated)*
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
| Urgent | 1 |
| Ready (`loom:issue`) | 1 |
| In Progress (`loom:building`) | 1 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 2 |
| Curated | 8 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-07T13:29Z, Guide triage cycle):** Backlog remains thin and healthy — no priority changes made. Urgent stayed at 1/3 slots (#5565, the sole ready `loom:issue`, already urgent); no other candidate to promote and nothing to demote. `loom-recover-orphans --verbose` confirmed the one `loom:building` issue (#5601) is live (fresh worktree, tracked in the sweep journal) — not orphaned. Checked recently merged PRs for still-open linked issues — none found, no orphaned closures to fix. Blocked-issue scan: #5385 remains correctly `loom:blocked` — no parseable body dependency, but a superseding block is active (linked PR #5397 is OPEN with `loom:changes-requested` after exhausting the Doctor-cycle budget, parked for human review); left untouched. Epic #4489 unchanged at 6/7 phases closed (Phase 7 = #4496, still `loom:curated` + `loom:operator-only`, correctly gated on a human decision, not stalled). WORK_LOG.md already current — no new merged PRs or closed issues outside its existing entries (30-day window scan). WORK_PLAN.md regenerated from current label state: added #5601 to In Progress, added #5605/#5604/#5601 to Proposed (three issues curated since the last regeneration), and updated the Backlog Balance counts accordingly.
