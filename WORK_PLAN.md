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

- **#5474**: worktree.sh: root node_modules symlink (and .mcp.json) never get _append_worktree_exclude — untracked noise in every worktree
- **#5431**: Wire remaining daemon gh call sites (guards/quarantine/watchdog/worktree_ops) through per-owner GH_CONFIG_DIR
- **#5390**: auto-update drain exits 0 for a launchd relaunch that never comes (the #4011 failure mode)
- **#5063**: host_identity() is whatever `hostname` prints: three naming schemes across the fleet, $HOSTNAME makes it launch-context-dependent, and it drives peer-claim self-recognition

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

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

- **#5474**: worktree.sh: root node_modules symlink (and .mcp.json) never get _append_worktree_exclude — untracked noise in every worktree *(curated)*
- **#5431**: Wire remaining daemon gh call sites (guards/quarantine/watchdog/worktree_ops) through per-owner GH_CONFIG_DIR *(curated)*
- **#5390**: auto-update drain exits 0 for a launchd relaunch that never comes (the #4011 failure mode) *(curated)*
- **#5385**: Worktree-isolation guard fails closed on variable-expanded write paths that resolve outside the main checkout *(curated)*
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
| Urgent | 0 |
| Ready (`loom:issue`) | 3 |
| In Progress (`loom:building`) | 4 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 8 |
| Curated | 12 |
| Architect / Hermit proposals | 3 |
| Active epics | 2 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-05, ~20:21 UTC pass):** Removed `loom:urgent` from **#4767** — it had been re-blocked (Curator, 20:11:24Z: superseding PR #4770 is still open under a Champion merge-risk hold) without stripping `loom:issue` *or* `loom:urgent`, a new variant of the recurring dual-label re-block bug (previously seen stripping only `loom:issue`, see `project_dual_label_issue_blocked_reblock_bug` in Guide memory, now updated). `loom:urgent` is empty as a result — all 3 current `loom:issue` items (#4767, #5232, #4889) carry `loom:blocked` simultaneously, each superseded by its own open, Judge-approved `loom:pr` fix (#4770, #5233, #4918) parked behind a Champion merge-risk hold, so nothing in the ready queue is currently actionable by a Builder; flagged on #4767 for Curator/Champion. `loom-recover-orphans --verbose` found zero orphaned `loom:building` claims (all 5 building issues at the time were <15m old, well under the 4h reclaim threshold). Verified all issue-closing merged PRs (last 20) closed their target issues — no orphans. Checked all 11 `loom:blocked` issues for parseable dependencies — none have a `Blocked by`/`Depends on`/`Requires` reference, so mechanical unblock doesn't apply to any (5385/4928 are guard-bug reports; 4196/4167/3979/4136 are long-parked architect proposals; 4496 is the operator-gated Epic #4489 Phase 7 canary; 5329 is gated on `2AMLogic/2am`'s deploy workflow going green, per its own re-checked comment thread). Epic #4489 remains 6/6 phase-issues complete with only the operator-gated Phase 7 canary (#4496) open. Epic #5038 (Design) still has no phase-issues — not stale (touched today), just not yet decomposed. Newly recorded PRs this pass: #5475 (operator-session lane, closes #5353), #5476 (CODEOWNERS doc fix) — no orphaned issues from either. No new closed issues since the last watermark (#5467). Fleet is highly active right now (labels on #5474/#5440/#5393 all changed mid-pass); this snapshot reflects state as of the assessment timestamp above.
