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

_None._

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
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 2 |
| Curated | 8 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-07T08:30Z, Guide triage cycle):** #5543 (previously flagged as possibly-moot by an earlier pass) was re-checked by Curator, re-approved by Champion, built, and closed via PR #5588 — the earlier mootness read didn't hold up; no action needed here. The ready queue (`loom:issue`, excluding building) is down to a single issue, #5565, which is already `loom:urgent`; #5579 moved from ready into `loom:building` since the last pass and stays listed under Urgent (still `loom:urgent`) but drops out of Ready. With only one ready candidate and it already promoted, urgent stays at 2 (#5579, #5565) rather than force-filling to 3. `loom-recover-orphans --verbose` found 0 orphaned issues; one claim (#5579, building 20m) is within the grace period and not eligible for reclaim for ~3h39m. Checked the 5 blocked issues (#5385, #4196, #4167, #4136, #3979) for parseable dependency references — none had any (`Blocked by`/`Depends on`/`Requires`/task-list `#N` patterns) — all need manual review, none auto-unblocked. Verified the 6 most-recently-merged PRs against their linked issues — all closed correctly via `Closes #N`, no orphans. Epic #4489 unchanged at 6/7 phases closed (Phase 7 = #4496, still `loom:operator-only`, gated on the fleet's single Codex seat — active discussion as recently as 2026-08-06, not stale). WORK_LOG.md gained two new entries this pass (PR #5588, Issue #5543); README checked against the 3 most recent architectural-pattern PRs (#5588/#5585/#5584, all internal test/implementation fixes) and found current, no update needed.
