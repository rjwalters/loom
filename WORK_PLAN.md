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

- **#5624**: install.sh records the installer's absolute path in install-metadata.json, which consumers commit

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#5626**: fix(install): stop leaking installer's absolute path into install-metadata.json
- **#5619**: tokens: record (provider, upstream account id) in the pool storage layer

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5569**: fix(fleet): idle-shutdown guard asks daemon eligibility instead of vetoing on bare process presence
- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5624**: install.sh records the installer's absolute path in install-metadata.json, which consumers commit *(curated)*
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
| PRs awaiting review | 2 |
| Approved PRs awaiting merge | 2 |
| Curated | 7 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-07T15:33Z, Guide triage cycle):** Ready queue (`loom:issue`, not building) holds #5607 and #5565, both already `loom:urgent` — max-3-urgent unaffected (2/3 used), no other ready candidates exist, so nothing to promote or demote this cycle. `loom-recover-orphans --verbose` found zero orphaned `loom:building` issues (there are currently none open — #5614, the earlier flapping tracker, closed since the prior tick). Checked the 10 most recently merged PRs (#5620, #5617, #5613, #5611, #5610, #5597, #5594, #5592, #5588, #5585) for still-open linked issues — all their `Closes #N` targets are correctly `CLOSED`, no orphaned closures. Blocked-issue scan: #5609/#5608 correctly stay `loom:blocked` on open dependency #5607 (Phase 1, not yet merged — its open PR #5619 is `loom:changes-requested`); #5385 stays blocked on a superseding open PR (#5397, `loom:changes-requested`) per the #4634 gate; #4196/#4167/#4136/#3979 are Champion-parked architecture/exploratory proposals with explicit operator holds, not simple issue-dependency blocks — none had a parseable `Depends on #N`/`Blocked by #N` pointing at anything now resolved, so all five remain `loom:blocked` for human review, no action taken. Epic #4489 is at 6/7 phases closed; Phase 7 (#4496) carries `loom:curated` + `loom:operator-only` (live-credential/operator-approval gate) — active, not stale, left untouched per the Label Gate Policy. WORK_LOG.md needed no new entries — all of the last 10 merged PRs and closed issues were already recorded from prior ticks. WORK_PLAN.md's `Proposed` section previously omitted #5607/#5565 even though both still carry `loom:curated` (labels are never removed on promotion, per this repo's no-label-cleanup policy) — corrected the render to match `render_plan_body`'s literal, unfiltered `loom:curated` query, which is the documented source of truth for this section; Curated count moves 4 → 6 accordingly. No other section changed.

**Assessment (2026-08-07T16:06Z, Guide triage cycle):** No priority changes — ready queue is still exactly #5607/#5565, both already `loom:urgent` (2/3 used); no new ready candidates to weigh. `loom:building` is empty, so no orphan-recovery action. Checked the 15 most recently merged non-docs PRs (#5620 down to #5561) — every `Closes #N` target is `CLOSED`, no orphaned issues. Blocked-issue scan repeated #5609/#5608 (dependency #5607 still open), #5385 (superseding open PR #5397 still `loom:changes-requested`), and the four Champion-parked proposals (#4196/#4167/#4136/#3979) — all unchanged from the prior tick, all correctly left `loom:blocked`. Epic #4489 unchanged at 6/7 phases, Phase 7 (#4496) still `loom:curated` + `loom:operator-only`. WORK_LOG.md needed no new entries (all recent PRs/issues already recorded). WORK_PLAN.md's `Proposed` section was missing newly-curated #5624 (filed and curated since the prior tick) — added it; Curated count moves 6 → 7. No other section changed.

**Assessment (2026-08-07T16:33Z, Guide triage cycle):** No priority changes — ready queue is still exactly #5607/#5565, both already `loom:urgent` (2/3 used); no new ready candidates. `loom-recover-orphans --verbose` found zero orphaned `loom:building` issues (one live sweep, #5624, tracked). Checked the 10 most recently merged PRs (#5625 down to #5612) — all their `Closes #N` targets are `CLOSED`, no orphaned closures. Blocked-issue scan unchanged from the prior tick: #5609/#5608 (dependency #5607 still open, its PR #5619 now `loom:review-requested` rather than `loom:changes-requested` — still not merged), #5385 (superseding open PR #5397 still `loom:changes-requested`), and the four Champion-parked proposals (#4196/#4167/#4136/#3979) — all correctly left `loom:blocked`, no action. Epic #4489 unchanged at 6/7 phases. WORK_LOG.md needed no new entries (nothing merged/closed since the prior tick's #5625). WORK_PLAN.md's `In Progress` and `PRs Awaiting Review` sections had gone stale (#5624 moved to `loom:building`, and PRs #5626/#5619 opened for review, since the last render) — corrected both sections and the Backlog Balance counts (In Progress 0→1, PRs awaiting review 0→2). README.md: the 2 architecture-pattern-touching recent PRs (#5620, #5613) are internal daemon bug fixes already covered by CLAUDE.md's existing role-runner documentation — no README staleness.
