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

- **#5539**: Guide's WORK_LOG.md closed-issue watermark misses out-of-order-closed issues (mirrors #5516, PR side already fixed)

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5539**: Guide's WORK_LOG.md closed-issue watermark misses out-of-order-closed issues (mirrors #5516, PR side already fixed) *(curated)*
- **#5512**: Quarantine stashes accumulate with no lifecycle — 37 across one fleet, oldest 9 days, all referencing closed issues *(curated)*
- **#5385**: Worktree-isolation guard fails closed on variable-expanded write paths that resolve outside the main checkout *(curated)*
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
| In Progress (`loom:building`) | 1 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 1 |
| Curated | 5 |
| Architect / Hermit proposals | 3 |
| Active epics | 2 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-06, ~08:43 UTC pass):** Appended WORK_LOG.md entries for PR #5531 and Issue #5516 (new watermark: last PR #5531, last issue #5516) — both are the immediate successors of the prior pass's watermark and nothing else merged/closed above it. The `loom:issue` ready queue is still 0, same pattern as the prior pass: `loom:urgent` #5523 and #5517 are both already `loom:building`, so there is currently nothing sitting in the approved-but-unclaimed state — Champion/Builder are keeping pace with curation, not stalling. `loom-recover-orphans --verbose` again found zero orphans (3 live claims: #5523, #5517, #5511, all well inside the 4h reclaim threshold). Re-verified all 7 `loom:blocked` issues (#5385, #5329, #4496, #4196, #4167, #4136, #3979) for parseable `Blocked by/Depends on/Requires #N` / task-list dependency references: only #4496 has a matching checkbox pattern, and every listed dependency (#4490-#4495, #4478) is already closed — but the issue's own text makes clear the remaining gate is a live, operator-authenticated Codex canary run, not issue closure, so it stays `loom:blocked` (non-dependency, operator-gated reason, matching the "when NOT to unblock" guidance). The other 6 blocked issues have no parseable dependency at all (design proposals and an external cross-repo gate) and are unaffected. Epics unchanged: #4489 at 6/7 phases closed (Phase 7 = #4496, operator-gated); #5038's two known phases (#5488, #5489) are both closed and #5038 itself dropped `loom:curated` (now carries only `loom:epic` + `tier:goal-supporting`) since the prior pass. Verified the 4 non-docs PRs merged since the prior watermark (#5525, #5522, #5521, #5524) all used proper `Closes #N` syntax and their referenced issues (#5504, #5508, #5515, #5510) are confirmed CLOSED — no orphaned-open-issue cleanup needed. `loom:review-requested` now shows 2 open PRs (#5534, #5533) — both freshly opened by Builder, nothing stale enough to flag.
