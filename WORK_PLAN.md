# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#5607**: tokens: record (provider, upstream account id) in the pool storage layer
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#5607**: tokens: record (provider, upstream account id) in the pool storage layer
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## In Progress

Issues currently being built (`loom:building`).

- **#5614**: Issue #5565 rapidly flapping between loom:issue and loom:building (~10 transitions in 7 minutes)

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#5620**: fix(daemon): stop reaper from resuming a clean-exit sweep that made no checkpoint progress

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5569**: fix(fleet): idle-shutdown guard asks daemon eligibility instead of vetoing on bare process presence
- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5614**: Issue #5565 rapidly flapping between loom:issue and loom:building (~10 transitions in 7 minutes) *(curated)*
- **#5607**: tokens: record (provider, upstream account id) in the pool storage layer *(curated)*
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision *(curated)*
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
| PRs awaiting review | 1 |
| Approved PRs awaiting merge | 2 |
| Curated | 7 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-07T15:02Z, Guide triage cycle):** Ready queue (`loom:issue`, not building) holds #5607 and #5565, both already `loom:urgent` — max-3-urgent unaffected (2/3 used), nothing to promote or demote this cycle. Since the prior tick: #5615 (docs-guide-lock cross-host race issue) closed via merged PR #5617; #5607 cycled back from `loom:building` to `loom:issue`+`loom:urgent` (label-timeline events lag briefly behind live state — confirmed current via direct issue view, not stale); a new PR #5620 opened `loom:review-requested`, unrelated to the docs-guide/token-pool work (reaper checkpoint-progress fix). `loom-recover-orphans --json` found zero orphans; the sole `loom:building` issue (#5614, tracking #5565's earlier flapping) is "watched" only (age ~19min, well under the 4h stale threshold) — the flapping itself stopped around 14:00 UTC and has not recurred. Checked the 10 most recently merged PRs for still-open linked issues — #5615, #5601, #5604, #5605 (via #5610) all closed correctly, no orphaned closures. Blocked-issue scan unchanged: #5385 and #4136 have no parseable `#N` dependency reference in their bodies, left `loom:blocked` for manual review; #5608/#5609 correctly stay blocked on open dependency #5607 (not yet merged). Epic #4489 at 6/7 phases closed (Phase 7 = #4496, `loom:curated` + `loom:operator-only`, gated on human sign-off — not a stale-epic case, work is active). WORK_LOG.md gained PR #5617 and Issue #5615 (both new since the committed snapshot). WORK_PLAN.md regenerated: In Progress lost #5615/#5607 (closed / reverted to Ready), PRs Awaiting Review swapped #5617→#5620, Curated gained #5614, Backlog Balance counts refreshed.
