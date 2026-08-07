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

- **#5579**: Champion can squash-merge a PR while a session is still pushing to its branch, stranding commits invisibly
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## In Progress

Issues currently being built (`loom:building`).

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
- **#5543**: dashboard-deploy is pinned by a miniflare isolated-storage flake — the live Worker has not redeployed since 08-05T07:28Z *(curated)*
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
| Ready (`loom:issue`) | 2 |
| In Progress (`loom:building`) | 1 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 2 |
| Curated | 9 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-07T08:00Z, Guide triage cycle):** The prior pass's three urgent issues (#5577, #5575, #5565) all landed — #5584/#5585 merged fixing #5577/#5575, and #5569 is approved and awaiting Champion merge for #5565 — dropping the urgent/ready queues to 2 (#5579, #5565); no 3rd candidate exists in the `loom:issue` backlog to promote, so the cap is left at 2 rather than force-filled. `loom-recover-orphans --recover` found 0 orphaned `loom:building` issues; the one in-progress issue (#5576) was updated within the hour. Checked the 15 most-recently-merged PRs against their linked issues — all closed correctly via `Closes #N`, no orphans. **#5543 flagged and demoted**: its own body named #5329 as the condition for mootness ("if that lands first, this becomes moot and can close"); #5329 closed via PR #5570 (merged 2026-08-06), which deleted `dashboard-deploy.yml` outright — every acceptance criterion in #5543 targets a file that no longer exists. Removed `loom:urgent`/`loom:issue`, added `loom:blocked`, and commented with the overlap rationale; left for Curator/human to close (Guide has no closing authority for obsolete issues). Epic #4489 unchanged at 6/7 phases closed (Phase 7 = #4496, still gated on a live Codex canary). WORK_LOG.md gained six new entries this pass (PRs #5585/#5584/#5583, Issues #5575/#5577/#5578).
