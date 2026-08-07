# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## Ready

Human-approved issues ready for implementation (`loom:issue`).

_None._

## In Progress

Issues currently being built (`loom:building`).

- **#5604**: tokens import-from-monitor: email-only keying lets a non-Anthropic credential occupy an Anthropic pool slot
- **#5601**: role_runner: hermit is not in DEFAULT_ROLES, so it can never be dispatched
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5569**: fix(fleet): idle-shutdown guard asks daemon eligibility instead of vetoing on bare process presence
- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5607**: tokens: record (provider, upstream account id) in the pool storage layer *(curated)*
- **#5604**: tokens import-from-monitor: email-only keying lets a non-Anthropic credential occupy an Anthropic pool slot *(curated)*
- **#5601**: role_runner: hermit is not in DEFAULT_ROLES, so it can never be dispatched *(curated)*
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
| Ready (`loom:issue`) | 0 |
| In Progress (`loom:building`) | 3 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 2 |
| Curated | 8 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-07T13:58Z, Guide triage cycle):** Ready queue (`loom:issue`, not building) is empty — nothing to prioritize this cycle. The one `loom:urgent` issue (#5565) has since moved to `loom:building`; left it alone per policy (urgency was set before it was claimed). `loom-recover-orphans --verbose` found no orphans — all three `loom:building` issues (#5604, #5601, #5565) are within the claim grace period or tracked live. Checked the 5 most recently merged non-docs PRs for still-open linked issues — all five (#5605, #5589, #5579, #5576, #5543) closed correctly, no orphaned closures. Blocked-issue scan: #5385 stays `loom:blocked` — superseding block still active (PR #5397 OPEN with `loom:changes-requested`, parked for human review after exhausting the Doctor-cycle budget); #4136 stays `loom:blocked` pending explicit operator promotion approval, not a resolvable dependency. Epic #4489 unchanged at 6/7 phases closed (Phase 7 = #4496, `loom:curated` + `loom:operator-only`, correctly gated on a human decision — last phase closed 2026-07-31, at the edge of the 7-day staleness window but self-documented as awaiting sign-off, not neglected). WORK_LOG.md updated with PR #5610 and Issue #5605 (both landed since the last regeneration). WORK_PLAN.md regenerated: Ready emptied, #5604/#5565 added to In Progress, #5607 replacing #5605 in Proposed (curated churn), and Backlog Balance counts refreshed.
