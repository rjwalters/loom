# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#5515**: Guard: extract_write_targets() misreads bash arithmetic >/>= comparisons as redirection, manufacturing phantom write targets
- **#5508**: Role-runner spawned sessions (Judge/Champion/etc.) inherit the daemon's own GH_CONFIG_DIR instead of the per-owner one — 404s on every non-default-owner repo

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#5515**: Guard: extract_write_targets() misreads bash arithmetic >/>= comparisons as redirection, manufacturing phantom write targets
- **#5508**: Role-runner spawned sessions (Judge/Champion/etc.) inherit the daemon's own GH_CONFIG_DIR instead of the per-owner one — 404s on every non-default-owner repo
- **#5502**: Model "a human is needed" as a first-class state (loom:operator), not a comment marker

## In Progress

Issues currently being built (`loom:building`).

_None._

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5515**: Guard: extract_write_targets() misreads bash arithmetic >/>= comparisons as redirection, manufacturing phantom write targets *(curated)*
- **#5508**: Role-runner spawned sessions (Judge/Champion/etc.) inherit the daemon's own GH_CONFIG_DIR instead of the per-owner one — 404s on every non-default-owner repo *(curated)*
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
| Urgent | 2 |
| Ready (`loom:issue`) | 3 |
| In Progress (`loom:building`) | 0 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 1 |
| Curated | 7 |
| Architect / Hermit proposals | 3 |
| Active epics | 2 |
<!-- guide:plan-body:end -->

**Assessment (2026-08-06, ~05:41 UTC pass):** WORK_LOG.md gained one entry pair (Issue #5501 closed, PR #5507 merged — sandbox supervisor-identity guard fix) that the number-based watermark check would have missed: PR #5507 (number 5507) merged *after* the prior watermark PR #5509 despite its lower number — an out-of-order merge. Added by direct file-presence check rather than trusting `number > last_pr`; **filed #5516 to track fixing `update_work_log()`'s watermark logic to compare merge timestamps or reconcile against `closingIssuesReferences`, not raw PR number order, so this doesn't require manual correction every time it recurs.** Marked #5515 `loom:urgent` (guard false-positive: `extract_write_targets()` misreads `>`/`>=` inside `((...))`/`[[...]]` as redirection, manufacturing phantom write targets — 10+ documented denials in ~2 days, blocking a routine bash idiom fleet-wide). `loom:building` and `loom:review-requested` are both empty this pass — no in-flight work. Re-verified all 7 `loom:blocked` issues (#5385, #5329, #4496, #4196, #4167, #4136, #3979): none have a resolvable dependency; #4496 remains gated on operator action (live accounts/budget/owner), not a code dependency. Epic #4489 unchanged at 6/7 phases closed (Phase 7 = #4496, operator-gated). Epic #5038's three filed phases (#5035, #5488, #5489) are all closed; Phase 4 (`janitor` role) is explicitly conditional on residue from phases 1-3 and has not been filed — flagged for Champion/Curator to assess whether to close the epic or file Phase 4; Guide does not close epics or file phase issues.
