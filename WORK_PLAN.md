# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#4767**: Codex guard bridge: model-controlled `workdir` bypasses managed-worktree write confinement

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#5232**: Guard: tee heredoc delimiter misparsed as write target, false worktree-isolation DENY
- **#4889**: worktree.sh remove can't delete squash-merged branches — uses git branch -d while merge-pr.sh has a squash-aware path
- **#4767**: Codex guard bridge: model-controlled `workdir` bypasses managed-worktree write confinement

## In Progress

Issues currently being built (`loom:building`).

- **#5440**: Guard: tee/sed/cp/mv write-target scan misparses heredoc opener as a bogus target, causing false worktree-confinement DENY
- **#5431**: Wire remaining daemon gh call sites (guards/quarantine/watchdog/worktree_ops) through per-owner GH_CONFIG_DIR
- **#5393**: install.sh and loom-daemon assume a login-shell PATH — false 'missing dependency' over ssh
- **#5390**: auto-update drain exits 0 for a launchd relaunch that never comes (the #4011 failure mode)
- **#5353**: Operator-session lane: let session tools skip Curator for mechanically-verifiable trivial changes
- **#5329**: Retire dashboard-deploy.yml + remove 2AM Cloudflare secrets once 2AMLogic/2am-side deploy is green
- **#5063**: host_identity() is whatever `hostname` prints: three naming schemes across the fleet, $HOSTNAME makes it launch-context-dependent, and it drives peer-claim self-recognition
- **#4607**: Wire defaults/scripts/check-cas-recheck-consistency.sh into .github/workflows/ci.yml's installer-tests job

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5469**: fix(install): probe non-login install roots for deps; document loom-daemon over ssh
- **#5462**: chore(deps): bump docker/setup-buildx-action from 3 to 4
- **#5461**: chore(deps): bump actions/download-artifact from 7 to 8
- **#5460**: chore(deps): bump docker/login-action from 3 to 4
- **#5459**: chore(deps): bump docker/build-push-action from 6 to 7
- **#5233**: fix(guard): exclude heredoc redirection tokens from tee/cp/mv/sed write-target scan
- **#4940**: feat(install): serialize concurrent installs with a per-target PID lock
- **#4918**: fix(worktree): make worktree.sh remove squash-aware when deleting the attached branch
- **#4770**: fix(codex-bridge): validate a model-chosen workdir before trusting it as GUARD_CWD

## Proposed

Issues carrying `loom:curated`.

- **#5431**: Wire remaining daemon gh call sites (guards/quarantine/watchdog/worktree_ops) through per-owner GH_CONFIG_DIR *(curated)*
- **#5393**: install.sh and loom-daemon assume a login-shell PATH — false 'missing dependency' over ssh *(curated)*
- **#5390**: auto-update drain exits 0 for a launchd relaunch that never comes (the #4011 failure mode) *(curated)*
- **#5385**: Worktree-isolation guard fails closed on variable-expanded write paths that resolve outside the main checkout *(curated)*
- **#5353**: Operator-session lane: let session tools skip Curator for mechanically-verifiable trivial changes *(curated)*
- **#5266**: Remaining stale Loom installs beyond #5184's eight — active tool repos (anvil, kicad-tools, claude-monitor, safehouse) still lack create-issue.sh *(curated)*
- **#5131**: something removed the live autonomy-desired marker on robb-studio while its daemon kept running — crash protection silently disarmed *(curated)*
- **#5063**: host_identity() is whatever `hostname` prints: three naming schemes across the fleet, $HOSTNAME makes it launch-context-dependent, and it drives peer-claim self-recognition *(curated)*
- **#5038**: Design: who owns continuous maintenance? Split by determinism and granularity, not topic — and why a janitor agent cannot own install repair *(curated)*
- **#5007**: operator: provision additional Codex accounts + install/trust the managed pre-tool hook so the allocation can be used *(curated)*
- **#4607**: Wire defaults/scripts/check-cas-recheck-consistency.sh into .github/workflows/ci.yml's installer-tests job *(curated)*
- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#4136**: measure: every sweep phase re-reads the repo from scratch — quantify the duplicated-read cost *(curated)*

## Proposed (Architect / Hermit)

- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*
- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#3979**: Architecture: elastic compute — expand sweep parallelism onto cloud worker hosts when local CPU saturates *(architect)*

## Epics

- **#5038**: Design: who owns continuous maintenance? Split by determinism and granularity, not topic — and why a janitor agent cannot own install repair
- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management

## Backlog Balance

| Tier | Count |
|------|-------|
| Urgent | 1 |
| Ready (`loom:issue`) | 3 |
| In Progress (`loom:building`) | 8 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 9 |
| Curated | 13 |
| Architect / Hermit proposals | 3 |
| Active epics | 2 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-05, ~20:06 UTC pass):** `loom:urgent` holds its one issue (#4767, Codex guard-bridge workdir bypass) — within the max-3 cap, no shift needed. Of the 3 `loom:issue` items, #5232 and #4889 still carry `loom:blocked` simultaneously (each superseded by its own open, Judge-approved `loom:pr` fix — #5233, #4918 — awaiting Champion auto-merge); Curator is actively re-checking both on its own cadence (see recent "Curator dependency re-check" comments), so no Guide action taken — see `project_dual_label_issue_blocked_reblock_bug` in Guide memory. Found and recovered one genuine orphan: **#4607** had sat in `loom:building` since 2026-07-31 (~5 days) with no worktree and no linked PR — `loom-recover-orphans --recover` reset it to `loom:issue`+`loom:curated` (now shows under Proposed/curated, not yet re-approved). The other 7 `loom:building` issues are all within the reclaim grace period (label age <15m, threshold 4h). Checked all 9 `loom:blocked` issues for parseable dependencies — none have a `Blocked by`/`Depends on`/`Requires` reference, so the mechanical unblock logic doesn't apply to any of them (5385/4928 are guard-bug reports, 4196/4167/3979/4136 are long-parked proposals per prior Champion verdicts, 4496 is the operator-gated Epic #4489 Phase 7 canary). Epic #4489 is 6/6 phase-issues complete, only the operator-gated Phase 7 canary #4496 remains open (unchanged). Epic #5038 (Design) has no phase-issues yet — not stale, just not decomposed. Newly recorded PRs this pass: #5470 (CODEOWNERS enforcement), #5471 (probe, reverted), #5472 (revert) — no linked issues to verify closure on. No new closed issues since the last watermark (#5467).
