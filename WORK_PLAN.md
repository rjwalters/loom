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

- **#5559**: resync-installed.sh never restamps the vendored CLAUDE.md version header (metadata 0.18.0 vs header 0.16.0)

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#5561**: fix(resync): restamp .loom/CLAUDE.md's version header on resync

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5550**: fix(dashboard-deploy): patch miniflare isolated-storage flake, retry-once + failed-test-gate visibility
- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5559**: resync-installed.sh never restamps the vendored CLAUDE.md version header (metadata 0.18.0 vs header 0.16.0) *(curated)*
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
| In Progress (`loom:building`) | 1 |
| PRs awaiting review | 1 |
| Approved PRs awaiting merge | 2 |
| Curated | 7 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-07, ~02:52 UTC pass):** #5559 (resync-installed.sh version-header restamp) moved from curated to `loom:building`/`loom:review-requested` since the prior pass — its Builder PR is #5561, now reflected in In Progress / PRs Awaiting Review. `loom-recover-orphans --recover` confirmed #5559's claim is live (label age 21m, not yet eligible for the 4h stale-reclaim window) — not an orphan. #5543 remains the sole `loom:urgent`/`loom:issue` (well under the max-3 cap) — no change needed there. WORK_LOG.md needed no new entries (checked all merged PRs and closed issues in the last 30 days by date against a presence check on the committed log — nothing new since #5554/#5511 were last recorded; #5561 hasn't merged yet so it isn't logged). Verified the last 20 merged PRs all reference their closing issue correctly (no orphaned open issues from recent merges). Re-checked all 6 `loom:blocked` issues (#5385, #5329, #4196, #4167, #4136, #3979) for parseable `Blocked by/Depends on/Requires #N` references — none found on any of them, so none are eligible for the dependency-based unblock path; no change from prior passes. Epic #4489 unchanged at 6/7 phases closed (Phase 7 = #4496, `loom:operator-only`-gated for a Codex canary; last phase closed 2026-07-31 — not yet past the 7-day stale threshold). Curated queue is 6 (was 7, #5559 moved out into building); the remaining 6 stay non-promotable for reasons already logged (`loom:operator-only` on #5546/#4496/#5512, blocked #5385/#4136 with no parseable dependency). Flagged out-of-band for operator attention (not a Guide-actionable label change): #5546 reports `loom-daemon` DOWN on host `robb-pro` with watchdog recovery exhausted (`loom:operator-only`, filed 2026-08-06T14:58Z) — needs a human on that host to run the recovery command.
