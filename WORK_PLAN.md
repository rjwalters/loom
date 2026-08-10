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

**Update (2026-08-10 ~06:00 UTC)**: #5607 re-claimed `loom:building` (its
implementing PR #5619 is unchanged, Judge-approved and already in the pileup
above) while it still carried `loom:urgent` from an earlier pass — a stale
holder per the incumbency rule's eviction step (a issue that gains
`loom:building` is evicted outright, no ranking comparison needed). This tick
removed `loom:urgent` from #5607 (flip-guard-cleared, see the issue's
2026-08-10 comment); the set is now just `{#5565}`, one below the cap.

The two free slots were **not** filled this tick. Every remaining
`loom:issue` candidate (#5779, #5629, and the `loom:blocked` pair #5673/#5672)
already has an approved, `loom:operator`-held closing PR sitting in the same
pileup above (#5781, #5636, #5683, #5681 respectively) — i.e. **the entire
visible ready backlog is implementation-done and waiting solely on the
merge-risk-hold queue**, not on Builder capacity. Marking any of them urgent
would not change what a Builder does next (there is nothing left to build),
so this tick treats "no genuinely unbuilt candidate exists" as equivalent to
"no candidate strictly outranks leaving the slot empty" and makes no further
`loom:urgent` writes. The actual next action for all of this work is a human
merge/hold-clear pass on the 9 PRs above.

**Update (2026-08-10 ~06:31 UTC)**: #5607 released `loom:building` again
(back to `loom:issue`) since the prior update — its closing PR #5619 is
unchanged in the pileup above, so this is the same flap pattern, not new
work. #5607, #5629 and #5779 (all `tier`-ranked candidates for the two free
`loom:urgent` slots) were each checked against `urgent-flip-guard.sh` this
tick and suppressed — #5607 for flapping (5 events/24h), #5629 for a
reversal within the cooldown window, #5779 for flapping (4 events/24h). No
`loom:urgent` writes were made; the set remains `{#5565}`.

**Update (2026-08-10 ~07:31 UTC)**: re-ran the incumbency rule. #5629's prior
cooldown-suppressed reversal had aged out (`urgent-flip-guard.sh` now reports
`no-recent-conflict`, last event ~3.2h old), so it filled one of the two free
slots: `loom:urgent` added, set is now `{#5565, #5629}`. #5607 (rank 3,
tier:goal-advancing — would otherwise outrank #5629) and #5779 (rank 4, tied)
remain suppressed as flapping (5 and 4 events/24h respectively) and were left
untouched. One slot is still open; #5629's own closing PR #5636 is itself
sitting in the merge-risk-hold pileup above, so this promotion communicates
priority but — like the rest of the ready queue — cannot actually advance
past the pileup without a human merge/hold-clear pass.

**Update (2026-08-10 ~08:47 UTC)**: #5629 re-claimed `loom:building` again
(moved from Ready back to In Progress below) — its closing PR #5636 is
unchanged in the pileup above, so this is the same flap pattern as before, not
new work. Re-ran the incumbency rule: `loom:urgent` set `{#5565, #5629}` is
unaffected (a `loom:urgent` holder gaining `loom:building` is a mechanical
eviction only when it is *no longer* a `loom:urgent` candidate at all — #5629
still qualifies here since it retains `loom:urgent` while building). The two
remaining ready candidates, #5607 (rank 3, tier:goal-advancing) and #5779
(rank 4), were both re-checked against `urgent-flip-guard.sh` and remain
suppressed as flapping (5 and 4 events/24h respectively); no `loom:urgent`
writes were made this tick. No orphaned `loom:building` issues, no
mechanically-parseable resolved dependencies in the `loom:blocked` queue, and
epic #4489 remains 6/7 complete with Phase 7 (#4496) correctly parked pending
an operator decision. The 9-PR merge-risk-hold pileup described above remains
unchanged and is still the actual blocker on all of this ready work.

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#5629**: Role-spawn token selection (mode=random) hands out accounts marked in tokens-exhausted; monthly-spend-limit errors retried as RECOVERABLE
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#5779**: Guard force-op ask fires on heredoc/prose text, not just executed commands
- **#5673**: Guard read-only fast path (#5274) still denies sql-ddl when the grep pattern argument itself contains an escaped/quoted pipe
- **#5672**: Guard false positive: loom:gh-pr-merge-redirect denies gh pr comment bodies that merely quote/discuss 'gh pr merge' in prose
- **#5607**: tokens: record (provider, upstream account id) in the pool storage layer
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## In Progress

Issues currently being built (`loom:building`).

- **#5629**: Role-spawn token selection (mode=random) hands out accounts marked in tokens-exhausted; monthly-spend-limit errors retried as RECOVERABLE

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
| Ready (`loom:issue`) | 5 |
| In Progress (`loom:building`) | 1 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 9 |
| Curated | 11 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->
