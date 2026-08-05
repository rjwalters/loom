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

_None._

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

- **#5385**: Worktree-isolation guard fails closed on variable-expanded write paths that resolve outside the main checkout *(curated)*
- **#5266**: Remaining stale Loom installs beyond #5184's eight — active tool repos (anvil, kicad-tools, claude-monitor, safehouse) still lack create-issue.sh *(curated)*
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
| In Progress (`loom:building`) | 0 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 9 |
| Curated | 5 |
| Architect / Hermit proposals | 3 |
| Active epics | 2 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-05, ~21:15 UTC pass):** `loom:urgent` remains empty — the ready queue's only 3 `loom:issue` items (#4767, #5232, #4889) are still each `loom:blocked`, superseded by their own open, Judge-approved `loom:pr` fixes (#4770, #5233, #4918) parked behind a Champion merge-risk hold; unchanged since the prior pass, so no urgent promotion/demotion made. `loom-recover-orphans --verbose` (liveness: sweep-journal, live issues `[]`) found zero orphaned claims — there is currently no `loom:building` issue at all (#5431 closed out via PR #5482, which itself merged this pass). Re-verified all 10 `loom:blocked` issues individually: #4767/#4889/#5232/#5385/#4928 are each superseded by their own open implementing PR (none carrying `loom:changes-requested`/`loom:blocked` except #5397 on #5385, which is explicitly parked pending human review after exhausting the Doctor-cycle cap); #4196/#4167/#3979/#4136 are long-parked architect/measurement proposals awaiting Champion/operator action; #4496 is the operator-gated Epic #4489 Phase 7 canary (single-Codex-seat constraint, reconfirmed by the operator today); #5329 is gated on `2AMLogic/2am`'s production deploy workflow going green, re-verified still unmet. None had a resolvable dependency, so no unblocks this pass. Epic #4489 unchanged at 6/7 phases complete. Epic #5038 (Design) still has no phase-issues — 2 days old, not stale. This pass recorded 2 new PRs (#5484, #5482) in WORK_LOG since the last watermark (PR #5481); no newly closed issues above the #5474 watermark. WORK_PLAN counts refreshed to match current label state (building 1→0, review-requested 1→0 as #5482 merged; approved-awaiting-merge 8→9 as #5485 newly opened; curated 8→5 as #5431/#5131/#5007 dropped off `loom:curated` on closure).
