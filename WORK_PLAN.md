# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#5579**: Champion can squash-merge a PR while a session is still pushing to its branch, stranding commits invisibly
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## In Progress

Issues currently being built (`loom:building`).

- **#5579**: Champion can squash-merge a PR while a session is still pushing to its branch, stranding commits invisibly
- **#5576**: The fleet family can only see hosts add-worker created — let it read an operator-supplied roster

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5569**: fix(fleet): idle-shutdown guard asks daemon eligibility instead of vetoing on bare process presence
- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5579**: Champion can squash-merge a PR while a session is still pushing to its branch, stranding commits invisibly *(curated)*
- **#5576**: The fleet family can only see hosts add-worker created — let it read an operator-supplied roster *(curated)*
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision *(curated)*
- **#5546**: loom-daemon is DOWN on robb-pro and watchdog recovery is exhausted *(curated)*
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
| Ready (`loom:issue`) | 1 |
| In Progress (`loom:building`) | 2 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 2 |
| Curated | 8 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-07T08:23Z, Guide triage cycle):** #5543 (previously demoted last pass as "likely moot" once #5329 deleted `dashboard-deploy.yml`) was disputed by a Curator re-check and re-approved by Champion at 07:59Z — it turned out to be real and got a genuine fix (PR #5588, merged 08:15Z: WAL sidecar files tolerated in the miniflare isolated-storage teardown), not an obsolete duplicate. Lesson: a body-stated "moot if #X lands" claim needs verification against what #X actually did, not just whether #X closed — #5329's fix (delete the file) didn't make #5543's flake fix moot, since the flake reproduces in `dashboard/test/redaction.test.ts` regardless of the deploy workflow's existence. #5543 then moved `loom:issue`→`loom:building` (#5588's branch) and closed on merge, so urgent/ready dropped to #5579 (now building) and #5565 (still the only ready, unclaimed issue) — 1 in Ready, 2 In Progress. `loom-recover-orphans` found 0 orphans (2 building issues both within the 4h staleness grace period). Checked the 7 most-recently-merged PRs (5588/5585/5584/5583/5580/5570/5561) against their linked issues — all closed correctly via `Closes #N`, no orphans found. Blocked-issue dependency scan (#5385, #4196, #4167, #4136, #3979) found no parseable, resolvable dependencies — all five stay blocked. Epic #4489 unchanged at 6/7 phases closed (Phase 7 = #4496, still curated-only, gated on a live Codex canary). #5546 (loom-daemon down on robb-pro, `loom:operator-only`) was already confirmed resolved by a prior Guide pass (02:08Z comment) — daemon healthy, sentinel files cleared; left as-is since Guide has no closing authority and the label already routes it to a human. WORK_LOG.md gained one new entry this pass (PR #5588 / Issue #5543) that a same-morning docs PR (#5587) missed — its docs-worktree branched before #5588 merged but the docs PR itself merged after, so the two races past each other; the presence-check approach (#5516/#5539) caught the gap on this pass regardless of the miss.
