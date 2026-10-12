# Champion promotion throughput (#10753)

**Load when**: you are tuning how fast Champion promotes, posting or acting on
a `NEEDS REVISION` verdict, or Curator finds an issue labelled
`loom:needs-revision`.

On 2026-10-07, 17% of curated issues (118 of 683 over five days, fleet-wide)
had waited a day or more for approval, with a median wait of 30 h so far.
Three throttles caused that tail. Each is fixed below.

## 1. Promotion runs every pass, even while PRs remain

Champion used to promote only once no PRs remained. A repository with a steady
or held PR queue never reached promotion. On 2026-10-07, rjwalters/loom had 38
open `loom:pr` PRs, 30 of them held by `loom:operator`, so "no PRs remain" was
never true. A Champion session is also capped at 30 minutes
(`DEFAULT_ROLE_TIMEOUT`), so a long PR pass can end the session before
promotion starts.

Each pass now interleaves the two queues (`champion.md` → "Autonomous
Operation"):

1. Visit up to `LOOM_CHAMPION_PR_SLICE` PR rows, in `pr-queue` order.
2. Run promotion (Priorities 2 and 3), whether or not PRs remain held,
   sequenced, waiting on CI, or unvisited. While unvisited PR rows remain,
   stop after `LOOM_CHAMPION_PROMOTION_SLICE` fresh verdicts. A fresh verdict
   is a promotion, rejection, escalation or capacity deferral. Silent skips do
   not count, because they cost one read.
3. Resume the PR pass from its visited set, then finish promotion.

This does not cap the PR queue. Every row is still visited each pass, in the
same order, under the same safety criteria and holds. A cap on PR rows per
pass was rejected because `pr-queue` does not sort held PRs last. Old held
rows would fill the cap on every pass, and newer mergeable PRs behind them
would never be reached.

## 2. The slice knobs (the tier caps are retired)

| Env var | Default | Governs |
|---|---|---|
| `LOOM_CHAMPION_PR_SLICE` | 10 | PR rows visited before the pass pauses for promotion |
| `LOOM_CHAMPION_PROMOTION_SLICE` | 3 | Fresh promotion verdicts per pause while PR rows wait |
| `LOOM_MAX_UNREVISED_EVALUATIONS` | 2 | Revision rounds before the bound (existing knob, #4967) |

An unset or empty value falls back to the default.

**The tier caps were retired (#11512).** #10753 made three caps on
`tier:*` labels configurable: Tier 2 and Tier 3 promotions per pass, and a
Tier 3 backlog cap that counted promoted, unheld Tier 3 issues
(the earlier count of every open one pinned itself: 19 counted against 2
promoted in rjwalters/loom on 2026-10-07). #11103 retired the `tier:*`
labels with no mapping, so Champion now promotes every passing proposal in
priority order (`loom:very-important`, then `loom:important`, then the rest,
oldest first; `champion-issue-promo.md` → "Promotion Order"), and the Step 3c
capacity deferral and `classify-capacity-defer.sh` went with the caps. The
`tier2Cap`, `tier3Cap` and `tier3BacklogCap` hyperparameter keys and their
`LOOM_CHAMPION_TIER*` env vars are still accepted, so a config carrying them
starts, but nothing reads them.

**The slice defaults.** 10 PR rows takes roughly 10 minutes of a 30-minute
session, which leaves room for promotion and the rest of the PR pass. Priority
and interactive PRs lead the queue, so they still go first. 3 fresh verdicts
per pause bounds the merge delay to a few minutes. That is still well above
promotion demand: 683 curated issues over five days across the fleet is about
two per repository per day.

**Where a fleet sets them.** A role session inherits the daemon's environment.
`loom-daemon-start.sh` forwards the exported `LOOM_*` variables into the
service unit (launchd `EnvironmentVariables`, systemd `Environment=`), and
later re-renders keep them. To change a value on a host, export it and re-run
`loom-daemon-start.sh`. A plain `loom-daemon restart` keeps the old
environment. See "Changing daemon environment variables" and "Env keys carried
forward across a re-render" in `daemon-reference.md`. For a hand-run
`/loom:champion`, export the value in that shell. The knobs are
keys of the `champion` group of the `hyperparameters` config block
(`prSlice`, `promotionSlice`; defaults 10, 3), so the fleet store's machine tier
(`fleet/defaults.json`) can carry them and they enter the run digest
(`hyperparameters.md`). Precedence is env var > `LOOM_HYPERPARAMS` vector >
config block > default. Follow-on work: the daemon does not yet export the
resolved block values as `LOOM_CHAMPION_*` into role sessions, so Champion's
shell snippets honour only the env vars today.

## 3. "Needs revision" goes to Curator, not to the operator

A `NEEDS REVISION` verdict used to wait for somebody to edit the body. After
one unchanged re-check, Champion escalated to the operator with a bare
`loom:operator-only` hold. In example-org/tool-repo#202, the findings were
"split per leaf" and "finish the registry audit". Both are agent work, and the
issue was escalated an hour after its first verdict. Under #10001, the
operator is asked for product-level calls only.

### The loop

1. **Champion rejects** (`champion-issue-promo.md` Step 4). It posts the
   verdict and adds `loom:needs-revision` in the same edit that releases
   `loom:evaluating`. The findings are the comment's first bullet list, the
   shape `classify-dependency-block.sh` already reads. One exception: when the
   only failure is an open dependency, no label is added. There is nothing to
   revise, and the dependency-timing gate waits for the blocker.
2. **Champion's discovery skips `loom:needs-revision`**, so no silent-skip
   tally builds up while Curator holds the issue.
3. **Curator revises** (`curator.md` → "Revising `loom:needs-revision`"). It
   edits the body and appends a dated `## Revision` section that answers each
   finding: fixed, or refuted with evidence. Then it removes the label. Its
   other options are to close the issue, split it (the parent is then parked
   on its children with `park-record apply`, as a Builder does, so a tracking
   parent never comes back to Champion), or file a ranked decision for a
   product-level call.
4. **Champion re-evaluates.** The body edit changed the body hash, so the
   idempotency check finds no verdict for the new text and evaluates it fresh.
   This is the mechanism #7650 already relies on, and Champion keeps no other
   state for it.

### The bound

`PRIOR_REJECTIONS` counts every posted `NEEDS REVISION` verdict across all
revisions and never resets. Once `PRIOR_REJECTIONS + SKIP_STREAK` reaches
`LOOM_MAX_UNREVISED_EVALUATIONS`, Step 4 stops routing ordinary rounds. After
the dependency-timing and premise-false gates, it takes the first case that
fits:

| Case | Action |
|---|---|
| The gap is a product-level call | File a ranked decision |
| No trusted `<!-- champion:revision-exhausted -->` comment on the issue | One final Curator round, carrying that marker |
| The final round failed and an independently identified preference or authority question exists | File a ranked decision on that question |
| The final round failed and every remaining finding is factual | One disposition round (`<!-- champion:revision-disposition -->`): Curator closes, splits or files a decision, not another edit |
| Otherwise | Stand down: release the `loom:evaluating` claim, leave routing labels untouched, post no verdict, continue the batch; the silent-skip ladder holds the issue |

With the default of 2:

| Failing verdict | `PRIOR_REJECTIONS` before it | Outcome |
|---|---|---|
| 1st | 0 | Curator round 1 |
| 2nd (on the revised body) | 1 | Curator round 2 |
| 3rd | 2 | Final Curator round (marker), or a decision if the gap is product-level |
| 4th | 3 | Ranked decision if a preference or authority question is named; otherwise one disposition round for factual findings. The final-round marker is already on the issue |

**At most three Curator rounds, then one terminal step: a well-formed decision or a Curator disposition.** The marker
stays on the issue, so a final round is never granted twice. If Curator
removes the label without editing the body, or the verdict predates this
loop, the existing silent-skip ladder (#4967) reaches the same bound.

### When the operator is asked

The operator is asked only through
`loom-daemon operator-decision apply <N> --also-label loom:operator-only`,
with 2 to 4 ranked options ([`operator-decision.md`](operator-decision.md)).
Champion never applies a bare operator hold. There are two cases:

- **The gap is product-level from the start.** For example, whether the work
  is wanted at all, or a choice between two legitimate directions that facts
  cannot settle.
- **The final round came back and still fails on a preference or authority
  question.** The question must be identified independently of the failed
  rounds, by the falsifiability test in `label-state-machine.md`. The usual
  options are revise to a named scope, close as not planned, or accept as
  filed.

Exhausted rounds alone never make a factual finding a human call. If every
remaining finding is factual (an incorrect path, a missing registry audit),
the issue gets one disposition round, in which Curator must close, split or
file a decision. It does not get another edit.

`apply` rewrites the body. Champion therefore stamps the post-apply body hash
as the verdict marker in its escalation comment. If the operator un-parks the
issue, that reads as a ruling on the current body (`OPERATOR_RULED`, #8245),
and Champion does not escalate the same text again.

## Out of scope

- Recording each verdict in SigNoz is #10752.
- The dependency-cycle detector's own hold (`detect-dependency-cycle.sh`) and
  the epic flow (`champion-epic.md`) are unchanged.
