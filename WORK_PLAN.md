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

- **#5589**: loom-daemon forge auto-merge lacks the head-SHA precondition added to the shell merge path (#5579 follow-up)

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5569**: fix(fleet): idle-shutdown guard asks daemon eligibility instead of vetoing on bare process presence
- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision *(curated)*
- **#5546**: loom-daemon is DOWN on robb-pro and watchdog recovery is exhausted *(curated)*
- **#5512**: Quarantine stashes accumulate with no lifecycle — 37 across one fleet, oldest 9 days, all referencing closed issues *(curated)*
- **#5385**: Worktree-isolation guard fails closed on variable-expanded write paths that resolve outside the main checkout *(curated)*
- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#4136**: measure: every sweep phase re-reads the repo from scratch — quantify the duplicated-read cost *(curated)*
- **#5589**: loom-daemon forge auto-merge lacks the head-SHA precondition added to the shell merge path (#5579 follow-up) *(curated)*

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
| Curated | 7 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-07T09:41Z, Guide triage cycle):** Backlog is thin and healthy — no priority changes made. Urgent stayed at 1/3 slots (#5565, the sole ready issue, already urgent); no other `loom:issue` candidate to promote and nothing to demote. `loom-recover-orphans` found 0 orphans — the one `loom:building` issue (#5589) is 3 minutes old, well inside the grace period. Checked the 5 PRs merged since the prior tick (5594/5592/5588/5585/5584) and their linked issues (5579/5576/5543/5575/5577) — all closed correctly via `Closes #N`, no orphans; WORK_LOG.md already had all 5 recorded (presence-check found nothing new to append). Blocked-issue scan (#5385, #4196, #4167, #4136, #3979) found no resolvable dependencies — none have a parseable `Blocked by`/`Depends on`/`Requires #N`; #5385 is complexity-flagged, #4136 is operator-gated exploratory-only, and the three Architect proposals (#4196, #4167, #3979) remain parked pending Champion/operator evaluation. Epic #4489 unchanged at 6/7 phases closed (Phase 7 = #4496, `loom:operator-only`, gated on a live Codex canary, last updated 2026-07-31 — 7 days idle but expected, since it's an operator-gated live-canary step, not neglect). WORK_PLAN.md was stale: #5589 started building after the prior tick's snapshot and was still absent from In Progress / present in neither Approved nor other sections' counts — regenerated from current label state (In Progress 0→1, Curated 6→7).
