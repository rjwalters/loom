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

- **#5516**: Guide WORK_LOG.md watermark misses out-of-order-merged PRs (number > last_pr assumes merge order == number order)
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
| In Progress (`loom:building`) | 2 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 1 |
| Curated | 9 |
| Architect / Hermit proposals | 3 |
| Active epics | 2 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-06, ~07:40 UTC pass):** WORK_LOG.md is current — no new merged PRs or closed issues above the existing watermark (last PR #5525, last issue #5515); everything merged since (#5527/#5529/etc.) is this phase's own docs PR and is correctly excluded. WORK_PLAN.md regenerated: Curator applied `loom:urgent` to newly-curated #5523 (a real, time-sensitive defect — 11h of silent narration loss from #5457's un-defaulted safehouse socket resolution) — this is within Curator's documented scope (curator.md:591) to flag urgency at curation time, independent of Guide's ready-queue urgent management, so left as-is; not a dual-label anomaly. **Backlog imbalance worth human/Champion attention: the `loom:issue` ready queue is completely empty (0) while 9 issues sit in `loom:curated` awaiting promotion** — no work is available for Builders to pick up until Champion reviews and approves some of the curated backlog. Checked all 7 `loom:blocked` issues for parseable `Blocked by/Depends on/Requires #N` references again: none had one, nothing to unblock (same 7 as last pass, all blocked for design/operator reasons, not coded dependencies). `loom-recover-orphans --verbose` found no orphans (2 live `loom:building` claims — #5516 at 11m, #5511 at 22m — both far inside the 4h reclaim threshold). Epic #4489 unchanged at 6/7 phases closed (Phase 7 = #4496, operator-gated, `loom:blocked`+`loom:operator-only`). Epic #5038's two filed phases (#5488, #5489) remain closed (~7-8h old, not stale); no Phase 1 (the Class-1 daemon-subsystem work) issue has been filed yet under this epic — informational only, decomposition is Champion's call, not Guide's. Recently merged PRs (#5525, #5522, #5521, #5519, #5509, #5507, #5503) all correctly closed their linked issues — no orphaned-open-issue cleanup needed this pass.
