# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#5515**: Guard: extract_write_targets() misreads bash arithmetic >/>= comparisons as redirection, manufacturing phantom write targets
- **#5508**: Role-runner spawned sessions (Judge/Champion/etc.) inherit the daemon's own GH_CONFIG_DIR instead of the per-owner one — 404s on every non-default-owner repo
- **#5502**: Model "a human is needed" as a first-class state (loom:operator), not a comment marker

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#5510**: resync-installed.sh inside the loom repo modifies tracked files at a clean checkout — is that supported?
- **#5508**: Role-runner spawned sessions (Judge/Champion/etc.) inherit the daemon's own GH_CONFIG_DIR instead of the per-owner one — 404s on every non-default-owner repo

## In Progress

Issues currently being built (`loom:building`).

- **#5515**: Guard: extract_write_targets() misreads bash arithmetic >/>= comparisons as redirection, manufacturing phantom write targets
- **#5508**: Role-runner spawned sessions (Judge/Champion/etc.) inherit the daemon's own GH_CONFIG_DIR instead of the per-owner one — 404s on every non-default-owner repo
- **#5504**: loom-daemon fleet has no roll subcommand — and a roll needs a measured verdict, not --version
- **#5502**: Model "a human is needed" as a first-class state (loom:operator), not a comment marker

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

- **#5519**: feat(labels,champion): add loom:operator state, wire into merge-risk hold

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5515**: Guard: extract_write_targets() misreads bash arithmetic >/>= comparisons as redirection, manufacturing phantom write targets *(curated)*
- **#5510**: resync-installed.sh inside the loom repo modifies tracked files at a clean checkout — is that supported? *(curated)*
- **#5508**: Role-runner spawned sessions (Judge/Champion/etc.) inherit the daemon's own GH_CONFIG_DIR instead of the per-owner one — 404s on every non-default-owner repo *(curated)*
- **#5504**: loom-daemon fleet has no roll subcommand — and a roll needs a measured verdict, not --version *(curated)*
- **#5502**: Model "a human is needed" as a first-class state (loom:operator), not a comment marker *(curated)*
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
| Urgent | 3 |
| Ready (`loom:issue`) | 2 |
| In Progress (`loom:building`) | 4 |
| PRs awaiting review | 1 |
| Approved PRs awaiting merge | 1 |
| Curated | 9 |
| Architect / Hermit proposals | 3 |
| Active epics | 2 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-06, ~06:12 UTC pass):** WORK_LOG.md unchanged — no new merged PRs or closed issues above the watermark (last PR #5509, last issue #5501). WORK_PLAN.md's generated region regenerated: the fleet moved 4 issues into `loom:building` since the prior pass (#5515, #5508, #5504, #5502 — all also carry `loom:urgent`, left untouched per "never touch labels on active work"), and PR #5519 entered review. **Flagged a live label anomaly on #5508**: at 06:02:29Z `loom:issue` was re-added without removing `loom:building` (event history shows the 06:01:44Z transition correctly swapped the two, but the 06:02:29Z one didn't), producing an invalid dual-label state with no worktree or PR yet behind it. Commented on the issue for Champion/operator reconciliation rather than removing either label myself — general label hygiene outside urgent/blocked/orphan-verification isn't in this role's scope. Re-verified all 7 `loom:blocked` issues (#5385, #5329, #4496, #4196, #4167, #4136, #3979): none unblock — #4496 stays gated on the live operator-authenticated Codex canary (already correctly double-labeled `loom:blocked` + `loom:operator-only`; all its coded prerequisites are closed but the remaining blocker is explicitly non-mechanical), #5329 waits on an external `2AMLogic/2am` deploy-workflow gate that still returns no workflows, #4136/#4167/#4196/#3979 require explicit operator sign-off or have no parseable dependency. `loom-recover-orphans --verbose` found no orphans (all 4 building claims are <10 minutes old). Epic #4489 unchanged at 6/7 phases closed (Phase 7 = #4496, operator-gated). Epic #5038's two filed phases (#5488, #5489) are both closed; Phase 4 (`janitor` role) remains conditional and unfiled — flagged for Champion/Curator, Guide does not file phase issues or close epics.
