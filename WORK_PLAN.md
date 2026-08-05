# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

_None._

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#5232**: Guard: tee heredoc delimiter misparsed as write target, false worktree-isolation DENY *(also `loom:blocked` — superseding open PR #5233, duplicate-dispatch avoidance)*
- **#4889**: worktree.sh remove can't delete squash-merged branches — uses git branch -d while merge-pr.sh has a squash-aware path *(also `loom:blocked` — superseding open PR #4918, duplicate-dispatch avoidance)*

## In Progress

Issues currently being built (`loom:building`).

- **#5429**: test-loom-daemon-update.sh: intermittent CI failures unrelated to the PR under review

## PRs Awaiting Review

_None._

## Changes Requested

- **#5397**: fix(guard): allow for-loop-bound literal variables as write-target roots (`loom:changes-requested`)

## Approved (Awaiting Merge)

PRs that have passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5233**: fix(guard): exclude heredoc redirection tokens from tee/cp/mv/sed write-target scan
- **#4940**: feat(install): serialize concurrent installs with a per-target PID lock
- **#4918**: fix(worktree): make worktree.sh remove squash-aware when deleting the attached branch
- **#4770**: fix(codex-bridge): validate a model-chosen workdir before trusting it as GUARD_CWD

## Proposed

Issues awaiting Champion evaluation (`loom:curated`).

- **#5431**: Wire remaining daemon gh call sites (guards/quarantine/watchdog/worktree_ops) through per-owner GH_CONFIG_DIR *(curated)*
- **#5429**: test-loom-daemon-update.sh: intermittent CI failures unrelated to the PR under review *(curated)*
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
| Urgent | 0 |
| Ready (total loom:issue) | 2 |
| Building | 1 |
| Approved PRs awaiting merge | 4 |
| PRs awaiting review | 0 |
| Changes requested | 1 |
| Curated (awaiting Champion or already promoted) | 19 |
| Active epics | 2 |

**Assessment (2026-08-05, later pass):** `loom:urgent` is now empty — the prior cycle's three urgent issues all resolved: #5409 and #5401 closed (via PRs #5417/family), and #5385 lost `loom:urgent` at 09:04 UTC after Builder churn (four claim/release cycles in ~20 min) ended in `loom:blocked` rather than a merge. Nothing was promoted to fill the slot because the only two `loom:issue` items, #5232 and #4889, both still carry `loom:blocked` — same as last cycle, each superseded by its own open `loom:pr` fix (#5233, #4918, both Judge-approved, awaiting Champion auto-merge) — so per the Guide's own building/blocked-safety rule neither is eligible for `loom:urgent`, and there is no other ready candidate this tick. `In Progress` now holds #5429 (a flaky-CI issue that is also freshly `loom:curated`, picked up as a Builder claim after the last pass). `loom:review-requested` emptied out (#5426 merged); `loom:changes-requested` dropped to just #5397 (PR #5420 was fixed and merged). Orphan-recovery (`loom-recover-orphans --verbose`) found nothing to reclaim — the sole `loom:building` issue (#5429) is confirmed live in the sweep journal. All 11 open `loom:blocked` issues were checked for a resolvable numeric dependency (`Blocked by`/`Depends on`/`Requires` pattern in the body); none had one — all are blocked for external/operator-gated reasons (2AM deploy sequencing on #5329, superseding open PRs on #5232/#4889, operator-only canary/proposal gates on #4496/#4196/#4167, no parseable dependency on #5385/#4928/#4767/#4136/#3979) — so no unblocking action this cycle. Epic #4702 remains 14/15 complete (only operator-gated #4859 cutover remains, last updated 2026-08-03 — not stale); epic #4489 remains 6/6 phase-issues complete with only the operator-gated Phase 7 canary #4496 open (last updated 2026-08-04 — not stale). Recently merged PRs (#5427, #5426, #5424, #5423, #5422, #5420) all closed their linked issues via magic keywords — no orphaned closures found. #5232 and #4889 also currently carry `loom:issue` simultaneously with `loom:blocked` — an inconsistent dual-label state (both issues' label history shows a re-block event that did not strip `loom:issue`); this is outside the Guide's label-gate mandate (Guide never adds/removes `loom:issue`) and is noted here for Curator/Champion attention rather than acted on directly.
