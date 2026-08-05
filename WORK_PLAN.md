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
| Curated | 4 |
| Architect / Hermit proposals | 3 |
| Active epics | 2 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-05, ~22:58 UTC pass):** `loom:urgent` remains empty — the ready queue's `loom:issue` items are still each `loom:blocked` or (new this pass) `loom:building`, so nothing is actually free to promote. #4767/#5232/#4889 are unchanged: each `loom:blocked`, superseded by its own open, Judge-approved `loom:pr` fix (#4770, #5233, #4918) parked behind a Champion merge-risk hold — flagged (again) for Curator/Champion via a comment on #5232 and #4889 as a live instance of the recurring `loom:issue`+`loom:blocked` dual-label bug (#4767 was already flagged in a prior pass). **New this pass:** #5488 was promoted `loom:architect` → `loom:issue` → claimed `loom:building` since the last pass, but `loom:issue` was never stripped on claim — it now carries `loom:issue` **and** `loom:building` simultaneously, the exact invalid state CLAUDE.md's Label-Based Workflow section warns against; flagged via comment, left as-is (Guide's mandate never touches either label). `loom-recover-orphans --verbose` (liveness: sweep-journal, live issues `[]`) found #5488/#5489 claimed only 5-7 minutes ago — not orphaned. Re-checked all 11 `loom:blocked` issues for resolved dependencies (none parseable as `Depends on #N`); #4496's own Dependencies checklist is fully closed but it explicitly requires operator action (live accounts, canary budget, runtime owner), so it correctly stays `loom:blocked`/operator-gated, not auto-unblocked. Epic #4489 unchanged at 6/7 phases complete (Phase 7 = #4496, operator-gated canary). Epic #5038 (Design) both phase issues (#5488, #5489) moved from `loom:architect`/proposed into active `loom:building` this pass. Checked all merged PRs since the last watermark (PR #5484, Issue #5474): none — the only PRs above that number are this phase's own `docs: Guide document maintenance update` PRs (self-excluded), so WORK_LOG.md is unchanged. WORK_PLAN Ready/In Progress/Curated/Backlog-Balance counts updated to reflect #5488's promotion; Architect/Hermit dropped 5→3 as #5488/#5489 moved out of that section.
