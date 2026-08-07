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
- **#5607**: tokens: record (provider, upstream account id) in the pool storage layer

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5569**: fix(fleet): idle-shutdown guard asks daemon eligibility instead of vetoing on bare process presence
- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5615**: Guide's docs-guide-lock only serializes same-host ticks — cross-host role-runner races still open duplicate docs PRs *(curated)*
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
| Urgent | 1 |
| Ready (`loom:issue`) | 1 |
| In Progress (`loom:building`) | 2 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 2 |
| Curated | 8 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-07T14:28Z, Guide triage cycle):** Ready queue (`loom:issue`, not building) holds only #5565, which already carries `loom:urgent` — nothing new to promote or demote. **Flagging for operator attention:** issue #5614 (filed this cycle, `loom:curated`, not yet approved) documents #5565 label-flapping between `loom:issue` and `loom:building` roughly every 90s–3min since 13:57Z, most likely a Builder claiming and immediately releasing it on a fast retry loop. This is a diagnosis-only finding — Guide has no action to take (the issue isn't `loom:issue`-approved, so it's outside the urgent-labeling authority, and the flapping itself is a Builder/daemon bug, not a triage decision) — surfacing it here since it explains why In Progress / Ready counts may look inconsistent between consecutive ticks. `loom-recover-orphans --verbose` found no orphans — the two `loom:building` issues (#5615, #5607) are within the claim grace period. Checked the most recently merged PRs for still-open linked issues — #5601, #5604, #5605, #5589, #5579, #5576 all closed correctly, no orphaned closures. Blocked-issue scan unchanged: #5385 stays `loom:blocked` (superseding block — PR #5397 OPEN with `loom:changes-requested`, parked after exhausting the Doctor-cycle budget); #5608/#5609 stay blocked on open dependency #5607; #4136 and the three architect proposals (#4196, #4167, #3979) stay `loom:blocked` on Champion/operator review, not resolvable dependencies. Epic #4489 unchanged at 6/7 phases closed (Phase 7 = #4496, `loom:curated`, gated on human sign-off). WORK_LOG.md updated with PR #5613/Issue #5601 and PR #5611/Issue #5604. WORK_PLAN.md regenerated: Ready/In Progress reflect this instant's snapshot, Proposed gained #5615 and #5614, Backlog Balance counts refreshed.
