# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision
- **#5543**: dashboard-deploy is pinned by a miniflare isolated-storage flake — the live Worker has not redeployed since 08-05T07:28Z

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision
- **#5543**: dashboard-deploy is pinned by a miniflare isolated-storage flake — the live Worker has not redeployed since 08-05T07:28Z

## In Progress

Issues currently being built (`loom:building`).

_None._

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
| In Progress (`loom:building`) | 0 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 2 |
| Curated | 7 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-07, Guide triage cycle):** Both `loom:issue` issues (#5565, #5543) are already `loom:urgent` (2 of 3 slots used, no other ready-and-unclaimed issue exists to fill the third), and both fixes are already merged or approved-and-queued (PR #5570 closed #5567's dashboard-deploy retirement; PR #5569 is `loom:pr` for #5565's idle-shutdown fix), so no priority change was needed this pass. #5565 moved from `loom:building` back to ready/urgent since the prior pass (its PR is up for Champion merge), so In Progress is now empty. `loom-recover-orphans --recover` found 0 orphans. Verified the newest merged PR (#5570, closing #5567) correctly closed its linked issue via GitHub's `closingIssuesReferences`; also swept all 15 most-recently-merged PRs for non-closing issue references left erroneously open — none found. Re-checked all 6 open `loom:blocked` issues (#5385, #5329, #4196, #4167, #4136, #3979) for parseable `Blocked by/Depends on/Requires #N` references — none found on any of them, so none qualify for the mechanical dependency-unblock path; #5385 in particular remains correctly blocked under the superseding-block gate (linked PR #5397 is still OPEN with `loom:changes-requested`, capped after exhausting the Doctor-cycle budget). Epic #4489 unchanged at 6/7 phases closed (Phase 7 = #4496, `loom:operator-only`-gated for a live Codex canary requiring operator credentials). WORK_LOG.md gained one new entry pair this pass (PR #5570 / Issue #5567).
