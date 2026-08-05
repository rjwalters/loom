# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

_None._

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#5232**: Guard: tee heredoc delimiter misparsed as write target, false worktree-isolation DENY
- **#4889**: worktree.sh remove can't delete squash-merged branches — uses git branch -d while merge-pr.sh has a squash-aware path
- **#4767**: Codex guard bridge: model-controlled `workdir` bypasses managed-worktree write confinement

## In Progress

Issues currently being built (`loom:building`).

- **#5431**: Wire remaining daemon gh call sites (guards/quarantine/watchdog/worktree_ops) through per-owner GH_CONFIG_DIR

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#5482**: fix(daemon): wire guards/quarantine/watchdog/worktree_ops gh calls through per-owner GH_CONFIG_DIR

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

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
- **#5385**: Worktree-isolation guard fails closed on variable-expanded write paths that resolve outside the main checkout *(curated)*
- **#5266**: Remaining stale Loom installs beyond #5184's eight — active tool repos (anvil, kicad-tools, claude-monitor, safehouse) still lack create-issue.sh *(curated)*
- **#5131**: something removed the live autonomy-desired marker on robb-studio while its daemon kept running — crash protection silently disarmed *(curated)*
- **#5038**: Design: who owns continuous maintenance? Split by determinism and granularity, not topic — and why a janitor agent cannot own install repair *(curated)*
- **#5007**: operator: provision additional Codex accounts + install/trust the managed pre-tool hook so the allocation can be used *(curated)*
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
| Urgent | 0 |
| Ready (`loom:issue`) | 3 |
| In Progress (`loom:building`) | 1 |
| PRs awaiting review | 1 |
| Approved PRs awaiting merge | 8 |
| Curated | 8 |
| Architect / Hermit proposals | 3 |
| Active epics | 2 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-05, ~20:47 UTC pass):** `loom:urgent` remains empty — the ready queue's only 3 `loom:issue` items (#4767, #5232, #4889) are still each `loom:blocked`, superseded by their own open, Judge-approved `loom:pr` fixes (#4770, #5233, #4918) parked behind a Champion merge-risk hold; nothing changed there since the prior pass, so no urgent promotion/demotion made. `loom-recover-orphans --verbose` (liveness: `.loom/locks` + sweep-journal) found zero orphaned claims — the single current `loom:building` issue (#5431) is live and already has PR #5482 open under review. Verified the last 20 merged PRs' closing-issue references (#5481→#5474, #4607/#4570→#5480, #5063→#5479, #5390→#5478, #5353→#5475, #5393→#5469, #5454→#5465, #5457→#5464, #5455→#5463) all closed correctly — no orphans. Re-checked all 11 `loom:blocked` issues: none have a parseable `Blocked by`/`Depends on`/`Requires` reference, so mechanical unblock doesn't apply to any (5385/4928 superseded by their own open approved PRs same as the ready-queue three; 4196/4167/3979/4136 are long-parked architect/measurement proposals awaiting Champion/operator action; 4496 is the operator-gated Epic #4489 Phase 7 canary; 5329 is gated on `2AMLogic/2am`'s deploy workflow, still 404 on re-check). Epic #4489 unchanged at 6/7 phases complete (Phase 7 = #4496, operator-gated, last touched 2026-08-05T10:40, not stale). Epic #5038 (Design) still has no phase-issues — not stale, just not yet decomposed by Champion. This pass recorded 4 new PRs (#5481, #5480, #5479, #5478) and 1 new closed issue (#5474) in WORK_LOG since the last watermark (PR #5476 / issue #5467); WORK_PLAN counts refreshed to match current label state (building dropped 4→1 as #5474/#5390/#5063 closed out; review-requested 0→1 as #5482 opened; curated 12→8 as #5474/#5390/#5063/#4607 dropped off `loom:curated` on closure).
