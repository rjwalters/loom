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

- **#5489**: [Epic #5038 Phase 3] Activate onIdle scheduling for auditor and guide roles
- **#5488**: [Epic #5038 Phase 2] Add CI gates for repo hygiene: dangling links, gitignore drift, README/doc accuracy

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR
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

- **#5488**: [Epic #5038 Phase 2] Add CI gates for repo hygiene: dangling links, gitignore drift, README/doc accuracy *(curated)*
- **#5385**: Worktree-isolation guard fails closed on variable-expanded write paths that resolve outside the main checkout *(curated)*
- **#5038**: Design: who owns continuous maintenance? Split by determinism and granularity, not topic — and why a janitor agent cannot own install repair *(curated)*
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
| In Progress (`loom:building`) | 2 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 9 |
| Curated | 5 |
| Architect / Hermit proposals | 3 |
| Active epics | 2 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-05, ~23:00 UTC pass):** `loom:urgent` remains empty — the ready queue's only 3 `loom:issue` items (#4767, #5232, #4889) are still each `loom:blocked`, superseded by their own open, Judge-approved `loom:pr` fixes (#4770, #5233, #4918) parked behind a Champion merge-risk hold; unchanged since the prior pass, so no urgent promotion/demotion made. `loom-recover-orphans --verbose` found zero orphaned claims — the two `loom:building` issues (#5488, #5489, epic #5038 Phases 2–3) were claimed ~9–11 minutes before this check, both under the 2h/4h staleness thresholds, so they moved from "Proposed (Architect/Hermit)" into "In Progress" this pass; #5488 also picked up `loom:curated` en route. Re-checked all 11 `loom:blocked` issues via their own most-recent Curator dependency re-check comments (all dated today); none show a newly-resolved dependency this pass — each still superseded by its own open implementing PR, or a long-parked architect/measurement/operator-gated proposal (#4496 stays `loom:operator-only`). Epic #4489 unchanged at 6/7 phases complete. Checked recent merged PRs against their referenced issues: all closed correctly, no orphans (the `docs: Guide document maintenance update` PRs merged this cycle are this phase's own output and self-excluded from the WORK_LOG watermark scan). No new merged PRs or closed issues above the WORK_LOG watermark (PR #5484, Issue #5474) once self-referential docs PRs are excluded, so WORK_LOG.md is unchanged this pass. WORK_PLAN: In Progress 0→2, Proposed (Architect/Hermit) 5→3, Curated 4→5; all other sections unchanged.
