# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Operator Attention: Merge-Risk-Hold Pileup

Judge-approved PRs stuck under a `loom:operator` merge-risk hold — implementation work is done, only a human merge decision is missing.

- **#6212**: fix(ci-settle-poll): guard against empty gh pr checks output false-settling
- **#6209**: fix(scripts): loom-daemon-update.sh resolves the built artifact from cargo, not hardcoded paths

## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#6199**: loom:building is never cleared when an issue closes — 20 stale claims on one consumer repo
- **#6196**: Consumer AGENTS.md is 100% managed block — no room for repo-authored guidance
- **#6068**: Guard false positive: catastrophic-tier positional masking doesn't cover echo/printf, so a heading echo containing the trigger phrase hard-denies

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#6254**: [Epic #6172] Teach the same-command declaration workaround in guard deny messages and guard-hooks.md
- **#6199**: loom:building is never cleared when an issue closes — 20 stale claims on one consumer repo
- **#6198**: warn-operator-gated: vocabulary misses "Operator task — requires human action", the most common operator-task phrasing
- **#6196**: Consumer AGENTS.md is 100% managed block — no room for repo-authored guidance
- **#6194**: test-verify-install-scope.sh ships to consumer repos but cannot run there
- **#6169**: CI settle-polls false-settle on empty gh pr checks output — mandate a row-count guard
- **#6160**: loom-daemon-update.sh cannot install a binary it just built when cargo target-dir is redirected — silent no-op that reports a build
- **#6076**: Guard friction: stash-scope:main-checkout ASKs recur in headless runs despite a documented bypass toggle existing
- **#6068**: Guard false positive: catastrophic-tier positional masking doesn't cover echo/printf, so a heading echo containing the trigger phrase hard-denies

## In Progress

Issues currently being built (`loom:building`).

- **#6254**: [Epic #6172] Teach the same-command declaration workaround in guard deny messages and guard-hooks.md
- **#6253**: [Epic #6172] Formalize the ambiguity contract and add permanent #5397 repro-shape regression coverage
- **#6252**: [Epic #6172] Fix COMMAND_NO_COMMENT quote-unawareness and audit write idioms sharing COMMAND_ASK_SCAN

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#6240**: fix(merge): strip stale loom:building from issues a merge closes
- **#6238**: feat(warn-operator-gated): match the "Operator task — requires human action" phrasing
- **#6233**: fix(tests): make test-verify-install-scope.sh resolve its subject in installed repos
- **#6210**: feat(worktree): add a main target to stash-push/stash-pop and stop advising git stash pop in the primary clone
- **#6207**: fix(guard): mask echo/printf positional args in catastrophic-tier scan
- **#6206**: fix: extend --quick reinstall stash guard to cover root AGENTS.md

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#6212**: fix(ci-settle-poll): guard against empty gh pr checks output false-settling
- **#6209**: fix(scripts): loom-daemon-update.sh resolves the built artifact from cargo, not hardcoded paths

## Proposed

Issues carrying `loom:curated`.

- **#6254**: [Epic #6172] Teach the same-command declaration workaround in guard deny messages and guard-hooks.md *(curated)*
- **#6199**: loom:building is never cleared when an issue closes — 20 stale claims on one consumer repo *(curated)*
- **#6198**: warn-operator-gated: vocabulary misses "Operator task — requires human action", the most common operator-task phrasing *(curated)*
- **#6196**: Consumer AGENTS.md is 100% managed block — no room for repo-authored guidance *(curated)*
- **#6194**: test-verify-install-scope.sh ships to consumer repos but cannot run there *(curated)*
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

- **#6172**: Redesign the variable-rooted write-target analysis in the worktree-isolation guard — #5397's carve-out approach produced three distinct bypasses
- **#6165**: Complete #4028: give the forge claim a liveness dimension (a lease), so cross-host correctness stops depending on the safehouse channel
- **#6109**: Add a runtime-neutral scientific research lifecycle with evidence-gated phase contracts
- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management

## Backlog Balance

| Tier | Count |
|------|-------|
| Operator merge-risk holds | 2 |
| Urgent | 3 |
| Ready (`loom:issue`) | 9 |
| In Progress (`loom:building`) | 3 |
| PRs awaiting review | 6 |
| Approved PRs awaiting merge | 2 |
| Curated | 21 |
| Architect / Hermit proposals | 3 |
| Active epics | 4 |
<!-- guide:plan-body:end -->
