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

- **#5511**: loom-recover-orphans (or similar) reset loom:building -> loom:issue on #5501 despite an open, Closes-referencing PR
- **#5504**: loom-daemon fleet has no roll subcommand — and a roll needs a measured verdict, not --version

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#5525**: feat(fleet): add fleet roll with a measured process-vs-build verdict

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5512**: Quarantine stashes accumulate with no lifecycle — 37 across one fleet, oldest 9 days, all referencing closed issues *(curated)*
- **#5511**: loom-recover-orphans (or similar) reset loom:building -> loom:issue on #5501 despite an open, Closes-referencing PR *(curated)*
- **#5504**: loom-daemon fleet has no roll subcommand — and a roll needs a measured verdict, not --version *(curated)*
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
| PRs awaiting review | 1 |
| Approved PRs awaiting merge | 1 |
| Curated | 7 |
| Architect / Hermit proposals | 3 |
| Active epics | 2 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-06, ~06:41 UTC pass):** WORK_LOG.md updated — 4 new merged PRs and their closed issues appended above the prior watermark (last PR #5509 → #5524; last issue #5501 → #5510): #5502→#5519, #5508→#5522, #5515→#5521, #5510→#5524. WORK_PLAN.md's generated region regenerated: the queue drained to empty for both `loom:urgent` and `loom:issue` (nothing ready to prioritize this pass — the fleet is currently building #5511 and #5504, with #5525 in review and #5485 approved awaiting merge). No urgent-label changes were needed since there was nothing in `loom:issue` to rank. **Note on tooling**: the cached `gh-cached` helper (`$GH_READ`) returned a stale, near-empty snapshot for `loom:blocked` (1 of 7 issues) during this pass; all label-state decisions below were cross-checked against plain `gh`, which returned the true set. Re-verified all 7 `loom:blocked` issues (#5385, #5329, #4496, #4196, #4167, #4136, #3979): none unblock — #4496 stays gated on the live operator-authenticated Codex canary (double-labeled `loom:blocked` + `loom:operator-only`; its two coded prerequisites #4478/#4495 are now both closed, but the remaining blocker is explicitly non-mechanical), #5329 waits on an external `2AMLogic/2am` deploy-workflow gate not yet green, #4136/#4167/#4196/#3979 require explicit operator sign-off or have no parseable dependency. `loom-recover-orphans --verbose` found no orphans (2 live building claims, both within the reclaim threshold). Epic #4489 unchanged at 6/7 phases closed (Phase 7 = #4496, operator-gated). Epic #5038's two filed phases (#5488, #5489) remain closed; Phase 4 (`janitor` role) remains conditional and unfiled — flagged for Champion/Curator, Guide does not file phase issues or close epics.
