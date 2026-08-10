# Work Plan

Prioritized roadmap of upcoming work, maintained by the Guide role.

<!-- Maintained automatically by the Guide triage agent. Manual edits are fine but may be overwritten. -->

## Operator Attention: Merge-Risk-Hold Pileup (2026-08-10)

**9 Judge-approved PRs are stuck under a `loom:operator` merge-risk hold**, all
`state:OPEN`, `mergeStateStatus:CLEAN`, `mergeable:MERGEABLE` — implementation
work is done, only a human merge decision is missing:

- #5781 (closes #5779), #5778 (closes #5772), #5684 (closes #5674),
  #5683 (closes #5673), #5681 (closes #5672), #5636 (closes #5629),
  #5619 (closes #5607), #5569 (closes #5565), #5485 (closes #5431)

(#5569 moved from `loom:changes-requested` to `loom:pr` since the 2026-08-09
count of 8 — it is now Judge-approved and joins the pileup.)

Each blocks its issue's implementation from actually landing even though the
issue may still show as `loom:issue`/`loom:urgent`/`loom:building` in the
sections below (the Guide/Curator cannot clear `loom:operator` — only a human
can). In particular #5607/#5619 blocks Phase 2 (#5608) and Phase 3 (#5609) of
the token-pool-provider-identity design, and #5674/#5684 blocks the
worktree-write-confinement fix. **Needs a human merge/hold-clear pass.**

**Update (2026-08-10 ~04:30 UTC)**: #5629 finished its Builder pass and
reverted from `loom:building` back to `loom:issue` (its implementing PR #5636
is Judge-approved and already in the pileup above) — the same
already-implemented-but-relabeled-`loom:issue` pattern #5607 and #5779 show.
The current `loom:urgent` set (#5607, #5565) is unchanged this tick: neither
#5629 nor #5779 (both `tier:goal-supporting`, rank 4, tied with each other)
strictly outranks the other for the one free urgent slot, so per the
incumbency rule (#5643 — a tie fills nothing, to avoid two hosts' ticks
picking different tie-breaks and flip-flopping the 3rd slot) this tick makes
no `loom:urgent` writes. All six open `loom:issue` items currently in `Ready`
below already have an approved, `loom:operator`-held closing PR except #5673
and #5672 (correctly `loom:blocked` pending their own PRs #5683/#5681 in the
same pileup) — i.e. **the entire visible ready backlog is implementation-done
and waiting solely on the merge-risk-hold queue**, not on Builder capacity.

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#5607**: tokens: record (provider, upstream account id) in the pool storage layer
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#5779**: Guard force-op ask fires on heredoc/prose text, not just executed commands
- **#5673**: Guard read-only fast path (#5274) still denies sql-ddl when the grep pattern argument itself contains an escaped/quoted pipe
- **#5672**: Guard false positive: loom:gh-pr-merge-redirect denies gh pr comment bodies that merely quote/discuss 'gh pr merge' in prose
- **#5629**: Role-spawn token selection (mode=random) hands out accounts marked in tokens-exhausted; monthly-spend-limit errors retried as RECOVERABLE
- **#5607**: tokens: record (provider, upstream account id) in the pool storage layer
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## In Progress

Issues currently being built (`loom:building`).

_None._

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5781**: fix: mask single-quoted heredoc bodies before ASK-tier force-op/stash-scope scan
- **#5778**: fix(guard): mirror force-op:detached reset exemption into installed hook copy
- **#5684**: fix: correct BSD sed -i separate-suffix write-target resolution
- **#5683**: fix(guard): count only unescaped/unquoted pipes in read-only fast path (#5673)
- **#5681**: fix(guard-loom-workflow): mask unquoted-delimiter cat-heredoc bodies captured into text-data flags
- **#5636**: fix(tokens): propagate .ranking exhausted/blocked exclusions to the allowlist and random tiers
- **#5619**: tokens: record (provider, upstream account id) in the pool storage layer
- **#5569**: fix(fleet): idle-shutdown guard asks daemon eligibility instead of vetoing on bare process presence
- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5779**: Guard force-op ask fires on heredoc/prose text, not just executed commands *(curated)*
- **#5729**: loom-daemon is DOWN on robb-studio and watchdog recovery is exhausted *(curated)*
- **#5674**: Guard false positive: worktree-write-confinement denies cp/mv writes to /tmp or fully in-repo tmp-then-rename, unrelated to the main checkout *(curated)*
- **#5660**: Vendored guard-destructive-generic.sh has drifted ~2,200 lines ahead of its upstream, and the single-marker capability probe makes partial reconciliation unsafe *(curated)*
- **#5629**: Role-spawn token selection (mode=random) hands out accounts marked in tokens-exhausted; monthly-spend-limit errors retried as RECOVERABLE *(curated)*
- **#5607**: tokens: record (provider, upstream account id) in the pool storage layer *(curated)*
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision *(curated)*
- **#5512**: Quarantine stashes accumulate with no lifecycle — 37 across one fleet, oldest 9 days, all referencing closed issues *(curated)*
- **#5385**: Worktree-isolation guard fails closed on variable-expanded write paths that resolve outside the main checkout *(curated)*
- **#4496**: [Epic #4489 Phase 7] Run a multi-account Codex daemon canary and define the production-readiness gate *(curated)*
- **#4136**: measure: every sweep phase re-reads the repo from scratch — quantify the duplicated-read cost *(curated)*

## Proposed (Architect / Hermit)

- **#4196**: Proposal: safehouse room as the primary Loom operator interface (narrate → workers speak → steer → parity) *(architect)*
- **#4167**: Proposal: first-class multi-runtime worker support (Claude Code, Codex, Amp, oh-my-pi) via a runtime adapter contract *(architect)*
- **#3979**: Architecture: elastic compute — expand sweep parallelism onto cloud worker hosts when local CPU saturates *(architect)*

## Epics

- **#4489**: [Epic #4167 Phase 4] Routinely deploy Codex through loom-daemon with provider-aware account management

## Backlog Balance

| Tier | Count |
|------|-------|
| Urgent | 2 |
| Ready (`loom:issue`) | 6 |
| In Progress (`loom:building`) | 0 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 9 |
| Curated | 11 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->
