# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#5579**: Champion can squash-merge a PR while a session is still pushing to its branch, stranding commits invisibly
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## In Progress

Issues currently being built (`loom:building`).

- **#5579**: Champion can squash-merge a PR while a session is still pushing to its branch, stranding commits invisibly
- **#5576**: The fleet family can only see hosts add-worker created — let it read an operator-supplied roster

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#5592**: feat: support an operator-supplied fleet roster via LOOM_FLEET_PATH

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5569**: fix(fleet): idle-shutdown guard asks daemon eligibility instead of vetoing on bare process presence
- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5579**: Champion can squash-merge a PR while a session is still pushing to its branch, stranding commits invisibly *(curated)*
- **#5576**: The fleet family can only see hosts add-worker created — let it read an operator-supplied roster *(curated)*
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision *(curated)*
- **#5546**: loom-daemon is DOWN on robb-pro and watchdog recovery is exhausted *(curated)*
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
| Urgent | 2 |
| Ready (`loom:issue`) | 1 |
| In Progress (`loom:building`) | 2 |
| PRs awaiting review | 1 |
| Approved PRs awaiting merge | 2 |
| Curated | 8 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-07T08:45Z, Guide triage cycle):** Backlog is thin and healthy — no priority changes made. Urgent stayed at 2/3 slots (#5579, building, left alone per policy; #5565, the sole ready issue, already urgent) since there was no other `loom:issue` candidate to promote. `loom-recover-orphans` found 0 orphans (both building issues, #5579 and #5576, within the staleness grace period). Checked the 7 most-recently-merged PRs (5588/5585/5584/5583/5580/5570/5561) and their linked issues (5543/5575/5577/5578/5573/5567/5559) — all closed correctly via `Closes #N`, no orphans. Blocked-issue scan (#5385, #4196, #4167, #4136, #3979) found no resolvable dependencies — #5385 stays blocked on its own open, `loom:changes-requested` PR #5397; the four Architect proposals stay blocked on standing Champion/operator holds. Epic #4489 unchanged at 6/7 phases closed (Phase 7 = #4496, `loom:operator-only`, gated on a live Codex canary, last updated 2026-08-06 — not stale). WORK_LOG.md needed no new entries (all recent merged PRs/closed issues already recorded via the presence-check). WORK_PLAN.md was stale by one entry: PR #5592 (`loom:review-requested`) opened after the prior tick's snapshot — added to "PRs Awaiting Review" and the Backlog Balance count.
