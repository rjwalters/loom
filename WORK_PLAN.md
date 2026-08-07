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
- **#5576**: The fleet family can only see hosts add-worker created — let it read an operator-supplied roster
- **#5575**: fleet status reports a BUSY worker as UNREACHABLE — the 8s per-host timeout is hardcoded
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

- **#5578**: observability.md points readers at identity content reference-deployment.md no longer carries *(curated)*
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
| Ready (`loom:issue`) | 5 |
| In Progress (`loom:building`) | 0 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 2 |
| Curated | 10 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-07T06:40Z, Guide triage cycle):** Filled the urgent queue to its 3-issue cap — added #5575 (`fleet status` reports a busy worker `UNREACHABLE` under an 8s hardcoded per-host timeout, inverting the exact signal the command exists to give) and #5577 (a system-wide `loom-daemon` install shadows test stubs via a hardcoded `/usr/local/bin`-first PATH, causing deterministic false-negative `cargo nextest` failures on standard daemon hosts) alongside the already-urgent #5565 (idle-shutdown guard is a no-op under the fleet's own supervision). Left #5576 (fleet roster limited to `add-worker` hosts) at `loom:curated`/`loom:issue` without urgent — it already has a documented `LOOM_FLEET_PATH` workaround and needs a design decision among three options, not a quick mechanical fix. `loom-recover-orphans --recover` found 0 orphans (1 claim watched, not yet stale). Swept the 20 most-recently-merged PRs for non-closing issue references left erroneously open — none found; all `Closes #N` issues confirmed CLOSED. Re-checked all 6 open `loom:blocked` issues (#5543, #5385, #4196, #4167, #4136, #3979) for parseable `Blocked by/Depends on/Requires #N` references — none found on any, so none qualify for the mechanical dependency-unblock path; #5543 is freshly re-blocked (06:27Z) pending a Curator/Champion obsolescence call on PR #5570 (already flagged by multiple prior passes, not re-flagged again here), and #5385 remains correctly blocked under the superseding-block gate (linked PR #5397 still OPEN with `loom:changes-requested`). Epic #4489 unchanged at 6/7 phases closed (Phase 7 = #4496, `loom:operator-only`-gated for a live Codex canary; last activity 2026-08-06T09:27Z, well under the 7-day stale threshold). WORK_LOG.md gained two new closed-issue entries this pass (#5329, #5574); PR #5570 and Issue #5567 were already recorded by a prior tick's docs PR (#5571) before this checkout ran.
