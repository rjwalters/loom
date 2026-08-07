# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#5577**: cargo nextest: fleet::add_worker tests fail on hosts with loom-daemon installed system-wide (PATH stub shadowed)
- **#5575**: fleet status reports a BUSY worker as UNREACHABLE — the 8s per-host timeout is hardcoded
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#5577**: cargo nextest: fleet::add_worker tests fail on hosts with loom-daemon installed system-wide (PATH stub shadowed)
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision
- **#5543**: dashboard-deploy is pinned by a miniflare isolated-storage flake — the live Worker has not redeployed since 08-05T07:28Z

## In Progress

Issues currently being built (`loom:building`).

- **#5578**: observability.md points readers at identity content reference-deployment.md no longer carries
- **#5577**: cargo nextest: fleet::add_worker tests fail on hosts with loom-daemon installed system-wide (PATH stub shadowed)
- **#5576**: The fleet family can only see hosts add-worker created — let it read an operator-supplied roster
- **#5575**: fleet status reports a BUSY worker as UNREACHABLE — the 8s per-host timeout is hardcoded

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#5585**: fix(fleet): make fleet status per-host timeout configurable
- **#5584**: fix(loom-daemon): stop fleet::add_worker tests shadowing on hosts with system loom-daemon

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5583**: docs(observability): stop claiming reference-deployment.md holds operator identity
- **#5569**: fix(fleet): idle-shutdown guard asks daemon eligibility instead of vetoing on bare process presence
- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5579**: Champion can squash-merge a PR while a session is still pushing to its branch, stranding commits invisibly *(curated)*
- **#5578**: observability.md points readers at identity content reference-deployment.md no longer carries *(curated)*
- **#5577**: cargo nextest: fleet::add_worker tests fail on hosts with loom-daemon installed system-wide (PATH stub shadowed) *(curated)*
- **#5576**: The fleet family can only see hosts add-worker created — let it read an operator-supplied roster *(curated)*
- **#5575**: fleet status reports a BUSY worker as UNREACHABLE — the 8s per-host timeout is hardcoded *(curated)*
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
| Urgent | 3 |
| Ready (`loom:issue`) | 3 |
| In Progress (`loom:building`) | 4 |
| PRs awaiting review | 2 |
| Approved PRs awaiting merge | 3 |
| Curated | 12 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-07T07:10Z, Guide triage cycle):** Urgent queue unchanged at its 3-issue cap (#5577, #5575, #5565) — all three are now `loom:building` as well (Builder claimed them since the prior pass), so per the Safety Check they were left alone rather than relabeled; none of the three PRs implementing them (#5585 for #5575, #5584 for #5577, #5569 approved for #5565) are in yet. `loom-recover-orphans --recover` found 0 orphaned `loom:building` issues. Checked the 10 most-recently-merged PRs for non-closing issue references left erroneously open — none found; #5573, #5567, #5559 all confirmed CLOSED via their `Closes #N` PRs. #5543 remains `loom:blocked` + `loom:issue` (no parseable dependency, so it does not qualify for mechanical unblocking) pending the Curator/Champion obsolescence call already flagged by prior passes and by Doctor's 06:38Z note — not re-flagged again here. Epic #4489 unchanged at 6/7 phases closed (Phase 7 = #4496, still `loom:operator-only`-gated for a live Codex canary). WORK_LOG.md gained three new entries this pass (Issue #5582, Issue #5573, PR #5580).
