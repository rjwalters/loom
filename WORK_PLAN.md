# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## In Progress

Issues currently being built (`loom:building`).

- **#5615**: Guide's docs-guide-lock only serializes same-host ticks — cross-host role-runner races still open duplicate docs PRs
- **#5614**: Issue #5565 rapidly flapping between loom:issue and loom:building (~10 transitions in 7 minutes)
- **#5607**: tokens: record (provider, upstream account id) in the pool storage layer

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#5617**: fix(guide): add uncached pre-create recheck to close cross-host docs-PR race

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5569**: fix(fleet): idle-shutdown guard asks daemon eligibility instead of vetoing on bare process presence
- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

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
| In Progress (`loom:building`) | 3 |
| PRs awaiting review | 1 |
| Approved PRs awaiting merge | 2 |
| Curated | 4 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-07T14:44Z, Guide triage cycle):** Ready queue (`loom:issue`, not building) holds only #5565, which already carries `loom:urgent` and already has an approved PR (#5569) awaiting Champion merge — nothing new to promote or demote, max-3-urgent unaffected (1/3 used). Since the prior tick (14:28Z): #5614 (the issue documenting #5565's label-flapping) has itself been claimed and moved to `loom:building` — a Builder is now working the flapping bug directly, so no further operator flag needed this cycle. PR #5617 (a fix for the Guide's own docs-guide-lock cross-host race) is now open and `loom:review-requested`, awaiting Judge. `loom-recover-orphans --verbose` found no orphans — all three `loom:building` issues (#5615, #5614, #5607) are within the claim grace period. Checked the most recently merged PRs (back through 2026-08-06) for still-open linked issues — #5601, #5604, #5605, #5546, #5589, #5579, #5576, #5543, #5575, #5577, #5578, #5582, #5573, #5574, #5567, #5559 all closed correctly, no orphaned closures. Blocked-issue scan unchanged: #5385 stays `loom:blocked` (superseding block — PR #5397 OPEN with `loom:changes-requested`, parked after exhausting the Doctor-cycle budget); #5608/#5609 stay blocked on open dependency #5607 (still building, not closed); #4136 and the three architect proposals (#4196, #4167, #3979) stay `loom:blocked` on Champion/operator review, not resolvable dependencies — none previously carried `loom:issue`, so none are eligible for restoration even if their blockers cleared. Epic #4489 unchanged at 6/7 phases closed (Phase 7 = #4496, `loom:curated`, gated on human sign-off). WORK_LOG.md unchanged — every merged PR and closed issue since the last tick was already recorded. WORK_PLAN.md regenerated: In Progress gained #5614, Proposed lost #5614, PRs Awaiting Review gained #5617, Backlog Balance counts refreshed.
