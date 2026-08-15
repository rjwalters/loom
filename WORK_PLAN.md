# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#6240**: fix(merge): strip stale loom:building from issues a merge closes
- **#6212**: fix(ci-settle-poll): guard against empty gh pr checks output false-settling
- **#6210**: feat(worktree): add a main target to stash-push/stash-pop and stop advising git stash pop in the primary clone
- **#6209**: fix(scripts): loom-daemon-update.sh resolves the built artifact from cargo, not hardcoded paths
- **#6207**: fix(guard): mask echo/printf positional args in catastrophic-tier scan
- **#6206**: fix: extend --quick reinstall stash guard to cover root AGENTS.md

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#6242**: Mesh liveness must be a health signal: a dispatcher receiving zero peer claims while peers are up is non-green
- **#6239**: Role health is green while a role skips every tick pre-spawn: role_liveness can't see it and assess_roles' escalation is evicted from the ring
- **#6169**: CI settle-polls false-settle on empty gh pr checks output — mandate a row-count guard

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#6242**: Mesh liveness must be a health signal: a dispatcher receiving zero peer claims while peers are up is non-green
- **#6239**: Role health is green while a role skips every tick pre-spawn: role_liveness can't see it and assess_roles' escalation is evicted from the ring
- **#6216**: Timeline existing-PR probe treats a prose mention as a partial-increment reference, misrouting the issue away from its Builder
- **#6199**: loom:building is never cleared when an issue closes — 20 stale claims on one consumer repo
- **#6196**: Consumer AGENTS.md is 100% managed block — no room for repo-authored guidance
- **#6169**: CI settle-polls false-settle on empty gh pr checks output — mandate a row-count guard
- **#6160**: loom-daemon-update.sh cannot install a binary it just built when cargo target-dir is redirected — silent no-op that reports a build
- **#6076**: Guard friction: stash-scope:main-checkout ASKs recur in headless runs despite a documented bypass toggle existing
- **#6068**: Guard false positive: catastrophic-tier positional masking doesn't cover echo/printf, so a heading echo containing the trigger phrase hard-denies

## In Progress

Issues currently being built (`loom:building`).

- **#6242**: Mesh liveness must be a health signal: a dispatcher receiving zero peer claims while peers are up is non-green
- **#6216**: Timeline existing-PR probe treats a prose mention as a partial-increment reference, misrouting the issue away from its Builder

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#6279**: fix(health): surface a role stuck pre-spawn-skipping every tick, independent of the tick ring
- **#6240**: fix(merge): strip stale loom:building from issues a merge closes
- **#6212**: fix(ci-settle-poll): guard against empty gh pr checks output false-settling
- **#6210**: feat(worktree): add a main target to stash-push/stash-pop and stop advising git stash pop in the primary clone
- **#6209**: fix(scripts): loom-daemon-update.sh resolves the built artifact from cargo, not hardcoded paths
- **#6207**: fix(guard): mask echo/printf positional args in catastrophic-tier scan
- **#6206**: fix: extend --quick reinstall stash guard to cover root AGENTS.md

## Proposed

Issues carrying `loom:curated`.

- **#6243**: Dispatcher repo-sharding by fleet_priority: make cross-host claim collisions structurally rare *(curated)*
- **#6242**: Mesh liveness must be a health signal: a dispatcher receiving zero peer claims while peers are up is non-green *(curated)*
- **#6239**: Role health is green while a role skips every tick pre-spawn: role_liveness can't see it and assess_roles' escalation is evicted from the ring *(curated)*
- **#6216**: Timeline existing-PR probe treats a prose mention as a partial-increment reference, misrouting the issue away from its Builder *(curated)*
- **#6199**: loom:building is never cleared when an issue closes — 20 stale claims on one consumer repo *(curated)*
- **#6196**: Consumer AGENTS.md is 100% managed block — no room for repo-authored guidance *(curated)*
- **#6169**: CI settle-polls false-settle on empty gh pr checks output — mandate a row-count guard *(curated)*
- **#6161**: Check-in on #6018: after the next fleet resync, verify the ownership classifier neither deleted repo-owned files nor over-preserved stale ones *(curated)*
- **#6160**: loom-daemon-update.sh cannot install a binary it just built when cargo target-dir is redirected — silent no-op that reports a build *(curated)*
- **#6159**: Check-in on #5999: has any live worktree been lost to a reinstall, and is the rm -rf fallback dead code? *(curated)*
- **#6158**: Efficacy review: after ~100 PRs, verify #6148's live-process gate still reclaims disk (over-detection fails silently) *(curated)*
- **#6156**: Efficacy review: after ~300 merges, verify #6118's mergeable-recheck is behaving (and never merged a real conflict) *(curated)*
- **#6134**: Guide/Judge/Champion: fast-path review+merge for docs-only WORK_LOG/WORK_PLAN PRs *(curated)*
- **#6076**: Guard friction: stash-scope:main-checkout ASKs recur in headless runs despite a documented bypass toggle existing *(curated)*
- **#6068**: Guard false positive: catastrophic-tier positional masking doesn't cover echo/printf, so a heading echo containing the trigger phrase hard-denies *(curated)*
- **#6062**: Pulse (2amlogic.com) narrates closed-unmerged duplicate efforts; should narrate on merge / dedup by merged-PR *(curated)*
- **#5897**: sweep skill: presumed-dead Builder Task can survive a session roll — verify task liveness before re-dispatch (duplicate-builder hazard) *(curated)*
- **#5660**: Vendored guard-destructive-generic.sh has drifted ~2,200 lines ahead of its upstream, and the single-marker capability probe makes partial reconciliation unsafe *(curated)*
- **#5512**: Quarantine stashes accumulate with no lifecycle — 37 across one fleet, oldest 9 days, all referencing closed issues *(curated)*
- **#5385**: Worktree-isolation guard fails closed on variable-expanded write paths that resolve outside the main checkout *(curated)*
- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#4136**: measure: every sweep phase re-reads the repo from scratch — quantify the duplicated-read cost *(curated)*

## Proposed (Architect / Hermit)

- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*
- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#3979**: Architecture: elastic compute — expand sweep parallelism onto cloud worker hosts when local CPU saturates *(architect)*

## Epics

- **#6165**: Complete #4028: give the forge claim a liveness dimension (a lease), so cross-host correctness stops depending on the safehouse channel
- **#6109**: Add a runtime-neutral scientific research lifecycle with evidence-gated phase contracts
- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 6 |
| Urgent | 3 |
| Ready (`loom:issue`) | 9 |
| In Progress (`loom:building`) | 2 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 7 |
| Curated | 22 |
| Architect / Hermit proposals | 3 |
| Active epics | 3 |
<!-- guide:plan-body:end -->
