# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

_None._

## Ready

Human-approved issues ready for implementation (`loom:issue`).

_None._

## In Progress

Issues currently being built (`loom:building`).

- **#5516**: Guide WORK_LOG.md watermark misses out-of-order-merged PRs (number > last_pr assumes merge order == number order)
- **#5511**: loom-recover-orphans (or similar) reset loom:building -> loom:issue on #5501 despite an open, Closes-referencing PR

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5516**: Guide WORK_LOG.md watermark misses out-of-order-merged PRs (number > last_pr assumes merge order == number order) *(curated)*
- **#5512**: Quarantine stashes accumulate with no lifecycle — 37 across one fleet, oldest 9 days, all referencing closed issues *(curated)*
- **#5511**: loom-recover-orphans (or similar) reset loom:building -> loom:issue on #5501 despite an open, Closes-referencing PR *(curated)*
- **#5385**: Worktree-isolation guard fails closed on variable-expanded write paths that resolve outside the main checkout *(curated)*
- **#5038**: Design: who owns continuous maintenance? Split by determinism and granularity, not topic — and why a janitor agent cannot own install repair *(curated)*
- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#4136**: measure: every sweep phase re-reads the repo from scratch — quantify the duplicated-read cost *(curated)*

## Proposed (Architect / Hermit)

- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*
- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#3979**: Architecture: elastic compute — expand sweep parallelism onto cloud worker hosts when local CPU saturates *(architect)*

## Epics

- **#5038**: Design: who owns continuous maintenance? Split by determinism and granularity, not topic — and why a janitor agent cannot own install repair
- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management

## Backlog Balance

| Tier | Count |
|------|-------|
| Urgent | 0 |
| Ready (`loom:issue`) | 0 |
| In Progress (`loom:building`) | 2 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 1 |
| Curated | 7 |
| Architect / Hermit proposals | 3 |
| Active epics | 2 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-06, ~07:10 UTC pass):** WORK_LOG.md updated — PR #5525 (closes #5504) appended above the prior watermark (last PR #5509→#5524 unaffected since #5525>#5524; last issue stays #5515). Note: #5504 itself is below the issue watermark (5515) so the number-threshold `new_issues` query didn't pick it up as a closed-issue line — this is the exact out-of-order-merge gap already filed and being fixed at #5516 (`loom:building`); not hand-patched here, left for that fix to land. WORK_PLAN.md's generated region regenerated: `loom:issue`/`loom:urgent` both still empty (nothing to prioritize), `loom:review-requested` drained to empty (PR #5525 merged), `loom:building` is now #5511 + #5516 (#5504 closed via merge, #5516 newly claimed ~19 min old — not orphaned). Checked all 7 `loom:blocked` issues for parseable `Blocked by/Depends on/Requires #N` references: none had one, so nothing to unblock this pass (all 7 are blocked for design/operator reasons: #4496 on a live operator-authenticated Codex canary, #5329 on an external `2AMLogic/2am` deploy gate, the rest on operator sign-off with no coded dependency). `loom-recover-orphans --recover` found no orphans (2 live building claims, both well within the 4h reclaim threshold). Epic #4489 unchanged at 6/7 phases closed (Phase 7 = #4496, operator-gated). Epic #5038's two filed phases (#5488, #5489) remain closed; it also carries a Champion `proposal-escalated` comment (2026-08-05) and `loom:operator-only` — already correctly parked for a human decision, Guide made no changes to it.
