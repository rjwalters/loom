# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#5523**: #5457 left safehouse socket resolution with no default — every sweep silently stopped narrating, froze the public pulse for 11h

## Ready

Human-approved issues ready for implementation (`loom:issue`).

_None._

## In Progress

Issues currently being built (`loom:building`).

- **#5523**: #5457 left safehouse socket resolution with no default — every sweep silently stopped narrating, froze the public pulse for 11h
- **#5517**: Installer contract: empty VERSION file, and install.sh has no --dry-run
- **#5511**: loom-recover-orphans (or similar) reset loom:building -> loom:issue on #5501 despite an open, Closes-referencing PR

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5523**: #5457 left safehouse socket resolution with no default — every sweep silently stopped narrating, froze the public pulse for 11h *(curated)*
- **#5517**: Installer contract: empty VERSION file, and install.sh has no --dry-run *(curated)*
- **#5516**: Guide WORK_LOG.md watermark misses out-of-order-merged PRs (number > last_pr assumes merge order == number order) *(curated)*
- **#5512**: Quarantine stashes accumulate with no lifecycle — 37 across one fleet, oldest 9 days, all referencing closed issues *(curated)*
- **#5511**: loom-recover-orphans (or similar) reset loom:building -> loom:issue on #5501 despite an open, Closes-referencing PR *(curated)*
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
| Urgent | 1 |
| Ready (`loom:issue`) | 0 |
| In Progress (`loom:building`) | 3 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 1 |
| Curated | 8 |
| Architect / Hermit proposals | 3 |
| Active epics | 2 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-06, ~08:22 UTC pass):** WORK_LOG.md is current — no new merged PRs or closed issues above the existing watermark (last PR #5525, last issue #5515); PRs merged since (#5527/#5529/#5530) are this phase's own docs PRs and are correctly excluded. The previous pass (~07:40 UTC) flagged the empty `loom:issue` ready queue against 9 curated issues as needing Champion attention — that has resolved itself: Champion promoted both #5523 and #5517 to `loom:issue` and Builder claimed each within ~1-2 minutes (label-event timestamps 08:20:29-08:22:42 UTC), so the ready queue reads 0 simply because approval→claim is currently happening faster than any polling cadence can observe, not because Champion has stalled. `loom-recover-orphans --verbose` found no orphans (3 live `loom:building` claims, all well inside the 4h reclaim threshold — #5516/#5511 from the prior pass plus the two freshly claimed above). Re-checked all `loom:blocked` issues (#5385, #5329, #4496, #4196, #4167, #4136, #3979) for parseable `Blocked by/Depends on/Requires #N` references: none had a closed numeric dependency to act on — all remain correctly blocked for design/operator reasons. Epics unchanged: #4489 at 6/7 phases closed (Phase 7 = #4496, operator-gated); #5038 has no new phase issues filed. No orphaned-open-issue cleanup needed this pass (no merged PRs above the watermark to check).
