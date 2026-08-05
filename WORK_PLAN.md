# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#5385**: Worktree-isolation guard fails closed on variable-expanded write paths that resolve outside the main checkout
- **#5409**: #4693 recurred: plain start on the RECOVERY path silently downgraded autonomy again (~1h idle) *(also `loom:building` — pre-existing state, not modified by this update; see the "Safety Check: Never Mark Building Issues Urgent" guardrail in `.loom/roles/guide.md`)*
- **#5401**: Daemon mints one installation token for its workspace root's owner — cross-owner repos are unreachable

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#5401**: Daemon mints one installation token for its workspace root's owner — cross-owner repos are unreachable
- **#5385**: Worktree-isolation guard fails closed on variable-expanded write paths that resolve outside the main checkout
- **#5232**: Guard: tee heredoc delimiter misparsed as write target, false worktree-isolation DENY *(also `loom:blocked` — superseding open PR #5233, duplicate-dispatch avoidance)*
- **#4889**: worktree.sh remove can't delete squash-merged branches — uses git branch -d while merge-pr.sh has a squash-aware path *(also `loom:blocked` — superseding open PR #4918, duplicate-dispatch avoidance)*

## In Progress

Issues currently being built (`loom:building`).

- **#5409**: #4693 recurred: plain start on the RECOVERY path silently downgraded autonomy again (~1h idle)
- **#5395**: fleet add-worker is Linux-only with no platform check — Mac hosts have no encoded onboarding

## PRs Awaiting Review

- **#5423**: fix(guide): give Document Maintenance a managed worktree to write in (`loom:review-requested`)
- **#5397**: fix(guard): allow for-loop-bound literal variables as write-target roots (`loom:review-requested`)

## Changes Requested

- **#5420**: fix(daemon): mint a GitHub App token per managed-repo owner so cross-owner repos are reachable (`loom:changes-requested`)

## Approved (Awaiting Merge)

PRs that have passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5233**: fix(guard): exclude heredoc redirection tokens from tee/cp/mv/sed write-target scan
- **#4940**: feat(install): serialize concurrent installs with a per-target PID lock
- **#4918**: fix(worktree): make worktree.sh remove squash-aware when deleting the attached branch
- **#4770**: fix(codex-bridge): validate a model-chosen workdir before trusting it as GUARD_CWD

## Proposed

Issues awaiting Champion evaluation (`loom:curated`).

- **#5409**: #4693 recurred: plain start on the RECOVERY path silently downgraded autonomy again (~1h idle) *(curated)*
- **#5401**: Daemon mints one installation token for its workspace root's owner — cross-owner repos are unreachable *(curated)*
- **#5395**: fleet add-worker is Linux-only with no platform check — Mac hosts have no encoded onboarding *(curated)*
- **#5393**: install.sh and loom-daemon assume a login-shell PATH — false 'missing dependency' over ssh *(curated)*
- **#5390**: auto-update drain exits 0 for a launchd relaunch that never comes (the #4011 failure mode) *(curated)*
- **#5385**: Worktree-isolation guard fails closed on variable-expanded write paths that resolve outside the main checkout *(curated)*
- **#5353**: Operator-session lane: let session tools skip Curator for mechanically-verifiable trivial changes *(curated)*
- **#5338**: loom-worker-2 reports 0 registered repos despite 8 workspaces on disk *(curated)*
- **#5266**: Remaining stale Loom installs beyond #5184's eight — active tool repos (anvil, kicad-tools, claude-monitor, safehouse) still lack create-issue.sh *(curated)*
- **#5131**: something removed the live autonomy-desired marker on robb-studio while its daemon kept running — crash protection silently disarmed *(curated)*
- **#5063**: host_identity() is whatever `hostname` prints: three naming schemes across the fleet, $HOSTNAME makes it launch-context-dependent, and it drives peer-claim self-recognition *(curated)*
- **#5062**: loom-worker-1 telemetry ingest key is bound to ip-172-31-74-176 while filing under loom-worker-1 (~35h unactioned) *(curated)*
- **#5038**: Design: who owns continuous maintenance? Split by determinism and granularity, not topic — and why a janitor agent cannot own install repair *(curated)*
- **#5007**: operator: provision additional Codex accounts + install/trust the managed pre-tool hook so the allocation can be used *(curated)*
- **#4992**: operator: enroll 2AM Logic in the Apple Developer Program (org account) *(curated)*
- **#4859**: [Epic #4702] 2AM production deploy: dashboard.2amlogic.com cutover *(curated)*
- **#4607**: Wire defaults/scripts/check-cas-recheck-consistency.sh into .github/workflows/ci.yml's installer-tests job *(curated)*
- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#4136**: measure: every sweep phase re-reads the repo from scratch — quantify the duplicated-read cost *(curated)*
- **#4057**: Provision a dedicated shared AWS CI runner for the project fleet (operator-only; gated on #4038) *(curated)*

Note: `loom:curated` is never removed once applied (see CLAUDE.md "Note on label
cleanup"), so several entries above are also already `loom:issue`/`loom:building`
— "Proposed" here means "carries `loom:curated`", not "exclusively awaiting
Champion", matching the literal query in `.loom/roles/guide.md` § "Document
Maintenance" Step 3.

## Proposed (Architect / Hermit)

- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*
- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#3979**: Architecture: elastic compute — expand sweep parallelism onto cloud worker hosts when local CPU saturates *(architect)*

## Epics

- **#4702**: Epic: Rich fleet observability dashboard with user-configurable hosting
- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management
<!-- guide:plan-body:end -->

## Backlog Balance

| Tier | Count |
|------|-------|
| Urgent | 3 |
| Ready (total loom:issue) | 4 |
| Building | 2 |
| Approved PRs awaiting merge | 4 |
| PRs awaiting review | 2 |
| Changes requested | 1 |
| Curated (awaiting Champion or already promoted) | 20 |
| Active epics | 2 |

**Assessment (2026-08-05):** Ready queue is thin: 4 issues carry `loom:issue`, but only #5401 and #5385 are actually dispatchable (both already `loom:urgent`, the max the Guide allows) — #5232 and #4889 are `loom:issue` + `loom:blocked` under active superseding-PR tracking by Curator (PRs #5233 and #4918, both `loom:pr`, Judge-approved, awaiting Champion auto-merge), so no Guide action is needed there. All other `loom:blocked` issues checked this cycle (#5411, #5329, #4928, #4767, #4196, #4167, #4136, #3979, and epic-phase #4496) are blocked for non-dependency reasons (external sequencing gates, operator-only prerequisites, or unfiled companion work) — none had a resolvable numeric dependency, so none were unblocked. Epic #4702 is 14/15 complete (only the operator-gated #4859 cutover remains); epic #4489 is 6/7 complete (only operator-gated Phase 7 canary #4496 remains). Note: PR #5423 (open, `loom:review-requested`) will move this phase's writes into a dedicated managed worktree per the worktree-isolation guard's intended confinement — this tick's edits still landed directly in the primary checkout, consistent with the mechanism that produced the last several `docs/guide-update-*` PRs, but that path is expected to change once #5423 merges. The `Proposed` section stays large relative to `Ready` because `loom:curated` persists after promotion — see the note above.
