# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#5543**: dashboard-deploy is pinned by a miniflare isolated-storage flake — the live Worker has not redeployed since 08-05T07:28Z

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#5543**: dashboard-deploy is pinned by a miniflare isolated-storage flake — the live Worker has not redeployed since 08-05T07:28Z

## In Progress

Issues currently being built (`loom:building`).

_None._

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5550**: fix(dashboard-deploy): patch miniflare isolated-storage flake, retry-once + failed-test-gate visibility
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
| Urgent | 1 |
| Ready (`loom:issue`) | 1 |
| In Progress (`loom:building`) | 0 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 2 |
| Curated | 7 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-07, ~04:41 UTC pass):** #5559 (resync-installed.sh version-header restamp) closed via merged PR #5561 since the prior pass — both were missing from WORK_LOG.md (the prior docs PR, #5564, merged before #5561 landed) and have now been backfilled under a new 2026-08-07 date section. #5543 remains the sole `loom:issue`/`loom:urgent` (well under the max-3 cap) — no change needed there; it also has an approved fix (PR #5550, `loom:pr`) queued for Champion auto-merge. Curated queue grew to 7 with the addition of #5565 (fleet add-worker idle-shutdown guard no-op), a fresh curation awaiting first review — not eligible for Guide-level urgent promotion (urgent only applies within the `loom:issue` ready queue). `loom-recover-orphans --verbose` found 0 orphaned `loom:building` issues (queue is empty). Checked all 6 `loom:blocked` issues (#5385, #5329, #4196, #4167, #4136, #3979) for parseable `Blocked by/Depends on/Requires #N` references — none found on any of them, so none are eligible for the dependency-based unblock path; all remain blocked for the structural/policy reasons already documented in their bodies (external sequencing gate, exploratory/proposal status, or scoped-complex fix pending). Epic #4489 unchanged at 6/7 phases closed (Phase 7 = #4496, `loom:operator-only`-gated for a Codex canary; last phase closed 2026-07-31 — not yet past the 7-day stale threshold). Verified the last 5 non-docs merged PRs (#5561, #5554, #5553, #5551, #5541) all correctly closed their linked issues — no orphaned closures found.
