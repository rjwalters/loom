# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#5401**: Daemon mints one installation token for its workspace root's owner — cross-owner repos are unreachable *(also building — see #5413 label-discipline note below)*

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#5394**: install.sh checks for pnpm but not that it can run — corepack floats to a pnpm that needs Node 22+
- **#5385**: Worktree-isolation guard fails closed on variable-expanded write paths that resolve outside the main checkout
- **#5232**: Guard: tee heredoc delimiter misparsed as write target, false worktree-isolation DENY
- **#4889**: worktree.sh remove can't delete squash-merged branches — uses git branch -d while merge-pr.sh has a squash-aware path

## In Progress

Issues currently being built (`loom:building`).

- **#5413**: Guide document-maintenance phase silently stopped landing PRs since 2026-02-26 (WORK_LOG high-water mark stuck at #3028)
- **#5401**: Daemon mints one installation token for its workspace root's owner — cross-owner repos are unreachable *(also carries `loom:urgent` — pre-existing state, not modified by this update; see the "Safety Check: Never Mark Building Issues Urgent" guardrail in `.loom/roles/guide.md`)*
- **#5395**: fleet add-worker is Linux-only with no platform check — Mac hosts have no encoded onboarding

## PRs Awaiting Review

- **#5417**: fix(daemon): report real installation_id + warn on cross-owner managed repos (`loom:review-requested`)

## Approved (Awaiting Merge)

PRs that have passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5233**: fix(guard): exclude heredoc redirection tokens from tee/cp/mv/sed write-target scan
- **#4940**: feat(install): serialize concurrent installs with a per-target PID lock
- **#4918**: fix(worktree): make worktree.sh remove squash-aware when deleting the attached branch
- **#4770**: fix(codex-bridge): validate a model-chosen workdir before trusting it as GUARD_CWD

## Proposed

Issues awaiting Champion evaluation (`loom:curated`).

- **#5413**: Guide document-maintenance phase silently stopped landing PRs since 2026-02-26 (WORK_LOG high-water mark stuck at #3028) *(curated)*
- **#5406**: CI pins Node 20, which is EOL — and no engines/.nvmrc states a supported version *(curated)*
- **#5403**: checkpoint: a closed issue's .loom-checkpoint persists indefinitely in the primary checkout and still returns a recovery_path *(curated)*
- **#5401**: Daemon mints one installation token for its workspace root's owner — cross-owner repos are unreachable *(curated)*
- **#5395**: fleet add-worker is Linux-only with no platform check — Mac hosts have no encoded onboarding *(curated)*
- **#5394**: install.sh checks for pnpm but not that it can run — corepack floats to a pnpm that needs Node 22+ *(curated)*
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
Maintenance" Step 3. Pre-existing quirk, not introduced by this update.

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
| Urgent | 1 |
| Ready (total loom:issue) | 4 |
| Building | 3 |
| Approved PRs awaiting merge | 4 |
| PRs awaiting review | 1 |
| Curated (awaiting Champion or already promoted) | 22 |
| Active epics | 2 |

**Assessment (2026-08-05):** This snapshot is a manual catch-up (see #5413) after Guide's Document Maintenance phase went silent for ~5.5 months (root cause: `guide` was missing from `.loom/config.json` → `autonomous.roleRunner.roles`, fixed by #5392/PR #5407). The `Proposed` section is large (22) relative to `Ready` (4) because `loom:curated` persists on issues after promotion — see the note above. Next real Guide tick should regenerate this file from current label state using the `<!-- guide:plan-body:start/end -->` markers now present.
