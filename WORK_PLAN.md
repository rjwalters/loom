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

**Update (2026-08-10 ~09:18 UTC)**: #5629 released `loom:building` again (back
to Ready, still `loom:urgent`) — same flap pattern, closing PR #5636 unchanged
in the pileup. Re-ran the incumbency rule: set `{#5565, #5629}` unaffected (no
eviction, #5629 still qualifies). #5607 and #5779 re-checked against
`urgent-flip-guard.sh` and remain suppressed as flapping (5 and 4 events/24h).
All 10 `loom:blocked` issues checked: none have a mechanically-parseable
resolved dependency, and the two token-pool phase issues (#5608/#5609) and the
two implementation-done guard fixes (#5674/#5385) are correctly still blocked
on the same operator-held PRs in the pileup above. No orphaned issues found in
the last 5 non-docs merged PRs (all closing references verified CLOSED). Epic
#4489 unchanged (~30h since last update, below the 7-day staleness bar).

**Update (2026-08-10 ~10:08 UTC)**: Ran a full triage cycle. Orphan recovery
found nothing (`loom-recover-orphans --recover`); the last 3 non-docs merged
PRs (#5876/#5871/#5867) each correctly closed their linked issue
(#5874/#5818/#5865). `loom:blocked` queue re-checked: only #5609/#5608 have a
parseable body dependency (#5607), which remains open, so both stay blocked;
the rest (#5674/#5673/#5672/#5385/#4196/#4167/#4136/#3979) have no
mechanically-parseable dependency and were left untouched. Epic #4489
unchanged at 6/7 (Phase 7 #4496 still correctly parked on
`loom:operator-only`, ~31h since last update, below the 7-day staleness
bar). Incumbency rule: one free `loom:urgent` slot existed (`{#5629,
#5565}`). #5607 (rank 3, `tier:goal-advancing`) would have taken it but its
write is still guard-suppressed for flapping (5 events/24h); #5779 (rank 4,
`tier:goal-supporting`) cleared the flip guard (`no-recent-conflict`) and
filled the slot. Set is now `{#5629, #5565, #5779}` — full, no incumbent
displaced. The 9-PR merge-risk-hold pileup above is unchanged and remains the
actual blocker on the rest of the ready queue.

**Update (2026-08-10 ~10:48 UTC)**: Ran a full triage cycle. #5629 had
re-claimed `loom:building` again (same flap pattern, closing PR #5636
unchanged in the pileup above) — a mechanical eviction per the incumbency
rule, `urgent-flip-guard.sh` cleared the removal (`no-recent-conflict`), so
`loom:urgent` was removed. Set is now `{#5779, #5565}`, one slot open. #5607
(rank 3, `tier:goal-advancing`) is the only remaining non-blocked
`loom:issue` candidate, but its own closing PR #5619 is already
Judge-approved and sitting in the merge-risk-hold pileup above — promoting it
would not change what a Builder does next, so the slot was left open rather
than filled. Orphan recovery found nothing. `loom:blocked` queue re-checked:
only #5609/#5608 have a parseable body dependency (#5607), still open, so
both remain correctly blocked; the rest
(#5674/#5673/#5672/#5385/#4196/#4167/#4136/#3979) have no
mechanically-parseable dependency and were left untouched. Epic #4489
unchanged (Phase 7 #4496 still correctly parked on `loom:operator-only`). The
9-PR merge-risk-hold pileup above is unchanged and remains the actual
blocker on the rest of the ready queue.

**Update (2026-08-10 ~12:40 UTC)**: Ran a full triage cycle. #5607 released
`loom:building` (back to Ready). Incumbency rule: one free `loom:urgent` slot
existed (`{#5779, #5565}`). #5629 (rank 4, `tier:goal-supporting`) would have
taken it but is still guard-suppressed for flapping (4 events/24h); #5890
(rank 5, `tier:maintenance` — a newly-curated issue diagnosing this very
WORK_PLAN churn pattern) cleared the flip guard (`no-history`) and filled the
slot. Set is now `{#5779, #5565, #5890}` — full, no incumbent displaced.
Orphan recovery found nothing. `loom:blocked` queue re-checked: none of the
10 open `loom:blocked` issues have a mechanically-parseable resolved
dependency — #5674/#5673/#5672/#5385 each still have an open, unmerged
closing PR (#5684/#5683/#5681, and #5397 respectively, the last already
parked `loom:operator-only`/`loom:operator-decision` per #5397's own
CHAIN_NOT_CONVERGING history); #5609/#5608's body dependency (#5607) remains
open; #4196/#4167/#4136/#3979 have no parseable numeric dependency at all.
Epic #4489 unchanged at 6/7 (Phase 7 #4496 still correctly parked, now ~35h
since last update, below the 7-day staleness bar). The 9-PR merge-risk-hold
pileup above is unchanged (all 9 still `OPEN`/`loom:operator`) and remains
the actual blocker on most of the ready queue. Also note: PR #5892 ("feat:
debounce WORK_PLAN.md rewrites against rapid label-driven diffs", closes
#5890) is already Judge-approved (`loom:pr`) and — unlike the 9-PR pileup —
carries no `loom:operator` hold, so it is a normal Champion auto-merge
candidate; see the Approved (Awaiting Merge) section below.

**Update (2026-08-10 ~15:23 UTC)**: Ran a full triage cycle. PR #5892 (the
WORK_PLAN debounce fix) has now merged, closing #5890; PR #5781 has also
merged, closing #5779 — both leave the `loom:urgent`/ready queues (set is now
just `{#5565}`, two free slots). Their replacement candidates, #5607 (rank 3)
and #5629 (rank 4), both remain `urgent-flip-guard.sh`-suppressed for flapping
(4 events/24h each), so no `loom:urgent` writes were made this tick — this is
the expected "no writes" outcome, not an omission. The 9-PR merge-risk-hold
pileup above has shrunk to **6**: #5781 (merged) and #5778 (closed as
superseded — its fix was already resynced into the installed hooks via commit
`b708dee7`, and its issue #5772 is CLOSED) both dropped out via resolution
paths other than a human merge; #5681 lost its `loom:pr`/`loom:operator`
labels after Doctor rebased it onto latest `main` at 15:25 UTC and is back
under active `loom:treating` review, so it is no longer part of the pileup
either. Remaining stuck under `loom:operator`: #5684 (closes #5674), #5683
(closes #5673), #5636 (closes #5629), #5619 (closes #5607), #5569 (closes
#5565), #5485 (closes #5431) — still the actual blocker on the rest of the
ready queue. Orphan recovery found nothing. `loom:blocked` queue (10 issues)
re-checked: none have a mechanically-parseable resolved dependency; no
changes made. Epic #4489 unchanged at 6/7 (~36h since last update, below the
7-day staleness bar).

**Update (2026-08-10 ~19:29 UTC)**: Ran a full triage cycle. Urgent set is now
`{#5895, #5629, #5565}` (#5895 filled the second free slot noted above,
somewhere between the prior tick and this one — the set is full again). This
tick's only new candidate, #5607 (rank 3, `tier:goal-advancing`), strictly
outranks the incumbents' rank (4, `tier:goal-supporting`) — but all three
incumbents are tied with each other, so there is no uniquely-determined
"weakest holder" to displace without an arbitrary tie-break. Per the
incumbency rule's own caution against exactly this kind of judgment call
(the #5643 flapping this rule exists to prevent), no swap was made and no
`loom:urgent` writes occurred this tick. Orphan recovery found nothing.
#5673/#5672's `loom:issue`+`loom:blocked` dual-label anomaly was already
re-flagged by a prior tick ~1h ago as the known re-block pattern (open,
`loom:operator`-held closing PRs #5683/#5681); left untouched. The 6-PR
merge-risk-hold pileup above is unchanged (all 6 still `OPEN`/`loom:operator`)
and remains the actual blocker on most of the ready queue. Epic #4489
unchanged at 6/7 (Phase 7 #4496 still correctly parked, below the 7-day
staleness bar). WORK_LOG.md had no new merged PRs or closed issues since
#5910; WORK_PLAN.md below was stale (Urgent/In Progress sections lagged
label state) and is regenerated this tick, past the 1h debounce window since
the last docs-maintenance merge.

<!-- guide:plan-body:start -->
## Urgent

Issues flagged as highest priority (`loom:urgent`).

- **#5895**: loom-daemon clean fails on stale worktree registrations — no git worktree prune before worktree remove, and --dry-run cannot see it
- **#5629**: Role-spawn token selection (mode=random) hands out accounts marked in tokens-exhausted; monthly-spend-limit errors retried as RECOVERABLE
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## Ready

Human-approved issues ready for implementation (`loom:issue`).

- **#5895**: loom-daemon clean fails on stale worktree registrations — no git worktree prune before worktree remove, and --dry-run cannot see it
- **#5673**: Guard read-only fast path (#5274) still denies sql-ddl when the grep pattern argument itself contains an escaped/quoted pipe
- **#5672**: Guard false positive: loom:gh-pr-merge-redirect denies gh pr comment bodies that merely quote/discuss 'gh pr merge' in prose
- **#5629**: Role-spawn token selection (mode=random) hands out accounts marked in tokens-exhausted; monthly-spend-limit errors retried as RECOVERABLE
- **#5607**: tokens: record (provider, upstream account id) in the pool storage layer
- **#5565**: fleet add-worker idle-shutdown guard vetoes on bare daemon presence — --idle-shutdown-minutes is a no-op under the fleet's own Restart=on-success supervision

## In Progress

Issues currently being built (`loom:building`).

- **#5911**: Ready-pool keeps re-selecting issues whose PR is loom:pr + awaiting human merge (repeat sweep dispatch waste, seen on #5565)

## PRs Awaiting Review

PRs waiting on Judge (`loom:review-requested`).

_None._

## Approved (Awaiting Merge)

PRs that passed review and are queued for Champion auto-merge (`loom:pr`).

- **#5904**: fix(daemon): treat stale worktree registrations as already-removed in loom-daemon clean
- **#5899**: chore: resync installed Loom surfaces
- **#5684**: fix: correct BSD sed -i separate-suffix write-target resolution
- **#5683**: fix(guard): count only unescaped/unquoted pipes in read-only fast path (#5673)
- **#5681**: fix(guard-loom-workflow): mask unquoted-delimiter cat-heredoc bodies captured into text-data flags
- **#5636**: fix(tokens): propagate .ranking exhausted/blocked exclusions to the allowlist and random tiers
- **#5619**: tokens: record (provider, upstream account id) in the pool storage layer
- **#5569**: fix(fleet): idle-shutdown guard asks daemon eligibility instead of vetoing on bare process presence
- **#5485**: fix(daemon): wire remaining repo-targeted gh call sites through per-owner GH_CONFIG_DIR

## Proposed

Issues carrying `loom:curated`.

- **#5911**: Ready-pool keeps re-selecting issues whose PR is loom:pr + awaiting human merge (repeat sweep dispatch waste, seen on #5565) *(curated)*
- **#5897**: sweep skill: presumed-dead Builder Task can survive a session roll — verify task liveness before re-dispatch (duplicate-builder hazard) *(curated)*
- **#5895**: loom-daemon clean fails on stale worktree registrations — no git worktree prune before worktree remove, and --dry-run cannot see it *(curated)*
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
| Urgent | 3 |
| Ready (`loom:issue`) | 6 |
| In Progress (`loom:building`) | 1 |
| PRs awaiting review | 0 |
| Approved PRs awaiting merge | 9 |
| Curated | 13 |
| Architect / Hermit proposals | 3 |
| Active epics | 1 |
<!-- guide:plan-body:end -->
