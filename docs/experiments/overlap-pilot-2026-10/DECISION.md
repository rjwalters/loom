# Decision: continue Augment in shadow mode; require valid evidence before enforcement

**Date:** 2026-10-01  
**Scope:** #9781, #9783–#9787  
**Status:** final recommendation for the next delivery stage  
**Audited experiment commit:** `e6d5fe545bca1fa8979958aae533ea748ca33d58`

**Executed follow-up:** [Round 3](round3/README.md) now supplies an independent
Git diagnostic, strict patch-preservation evidence, and a qualified historical
file-footprint comparison. It confirms the measurement concerns and finds a
modest exploratory retrieval increment over a cheap hybrid. The shadow-only
decision and requirement for prospective action evidence still stand.

## Decision in brief

**Proceed with the revision-pinned cache, inspectable footprints, and prospective
shadow measurements. Do not enable automatic serialization, combination, or
rebasing from the pilot's numeric thresholds.**

The earlier recommendation, `retrieval Jaccard >= 0.1 OR base skew >= 40`, is
withdrawn as a deployment rule. Its arithmetic can be reproduced on the saved
rows, but its labels, historical inputs, and evaluation population do not
support a scheduling decision. More runs of the same experiment would repeat
these defects.

The next work is bounded: repair the measurement pipeline, freeze a small set
of competing policies, collect a representative forward cohort, and evaluate
one frozen policy on a new temporal holdout. Keep the existing scheduling,
dependency, review, and merge checks as the production baseline throughout.

## 1. Evidence audit: what changed the decision

The audit recomputed counts from committed JSON, checked Git ancestry and
commit timestamps, and queried GitHub's `Issue.lastEditedAt` for all 242 v2
issues. It did not rerun provider retrieval or change the saved predictions.
The AWS runner was unreachable during this audit; artifact arithmetic and
read-only Git/forge inspection were performed locally. These are data-audit
results, not a new build/test verdict.

### A. Historical inputs leak information from after the cutoff

| Check | Verified result | Consequence |
|---|---|---|
| Indexed source timestamp versus declared cutoff | **116/121** v2 source commits have both author and committer timestamps after the cutoff; **24/24** in the retrieval subsample | The selected PR base SHA is not established as code available at prediction time. |
| Implementation already in the indexed source's ancestry | In **11/24** retrieval pairs, a member's final PR head is an ancestor of the indexed revision | Direct outcome leakage: the index's history already includes an implementation being predicted. |
| Issue content edits | **142/242** issues have a last edit after cutoff and before manifest creation, affecting **98/121** pairs; in the retrieval subset, **29/48** issues across **21/24** pairs | Current title/body cannot stand in for historical text without the earlier revision. |
| Purported common source topology | In **16/48** retrieval PR sides, the forced replay base is not an ancestor of the head | Merging raw heads against that base does not isolate the two issues' own patches. |

Git timestamps alone do not prove repository availability, but here they fail
even the basic temporal check. The ancestor checks provide stronger evidence:
for `q-9517-8884`, PR **#8893** head
`caf052f2dd14463fd5c214df53689552488f1402` is already an ancestor of indexed
revision `65634118b23e4db5f6171c13fdaf857badb39324`.

The previous “zero edited timeline events” test did not establish that issues
were never edited. For example, issue **#9585** has `lastEditedAt =
2026-09-30T03:09:12Z`, after pair cutoff `2026-09-30T02:34:25Z`. Those edits
already predated the v2 manifest's creation at
`2026-10-01T07:43:14.553839+00:00`; they are not later changes introduced by
this audit. Also, selecting issues created before both PRs *merged* does not
establish a cutoff before either implementation *started*.

Matching a prediction's content hash and revision string to a manifest proves
internal consistency. It does **not** prove that either input was historically
available. Consequently, none of the 24 retrieval pairs is currently validated
as a leakage-controlled historical prediction. The other five v2 pairs passing
the timestamp check are not automatically valid; their complete provenance
still needs verification.

### B. Several published counts and labels were wrong

| Earlier statement | Correction from saved rows |
|---|---|
| “54 of 69 conflicts were hub-only” | **54** is the number of conflict pairs with **no shared own-changed files**. The saved path-based classifier labels only **2/121** pairs `mechanical-hub`, **67** `substantive`, and **52** `clean`. |
| “18 substantive conflicts caught by the combined rule” | The retrieval subset has **13** path-labeled `substantive` positives, **2** `mechanical-hub`, and **9** `clean`. The combined rule **flags 18**, containing **13 positives + 5 false positives**. |
| “13/121 are proven direct collisions” | Thirteen pairs have both a non-hub replay conflict and some shared own-changed file. That conjunction does not establish that the shared file caused the conflict, or that either issue caused the other to need repair. |
| “False positives were benign” | Clean textual merge or a path on a hub list does not establish harmlessness. No component/combined runtime controls or intervention-cost measurements were collected. |

The existing word `substantive` means only “a conflicted path is outside a
small filename list.” It is **not** an adjudication of semantic conflict or
engineering effort. In particular, `Cargo.toml` and `package.json` may contain
either a version bump or a real dependency/API change. A filename is
insufficient to decide which.

### C. Threshold fitting and apparent ablations overstate the evidence

- The 24 retrieval pairs were selected **after outcomes were known**: 22/24
  have own-file overlap, compared with 22/121 in the full sample. This is a
  useful enriched case study, not an estimate of dispatch-queue precision or
  alert volume. Splitting it 17/7 afterwards does not remove the selection bias.
- The combined thresholds were explored on those same 24 rows. They have no
  untouched policy-level holdout. The reported AUC 0.800 uses only **five
  positive and two negative** raw conflict labels; Spearman 0.906 on seven
  rows does not establish calibration or scheduling benefit.
- **No non-default hub weights were supplied.** `hub_weighted` therefore
  equals raw file Jaccard. This is not evidence that weighting helped.
- The `blend` weight names do not match the score-map keys for hub, line,
  or symbol features. It effectively repeats the file feature here, with
  occasional floating-point roundoff. It is not an independently tested model.
- Only **one** retrieval pair has a non-null `edit_edit` score. The runner
  labels edits from a body-only affected-file list and does not invoke the
  footprint classifier's complete artifact. Neither intent classification nor
  producer/consumer evidence has demonstrated incremental predictive value.
- The threshold CSV omits the outcome name. Rows for file overlap and for
  conflict share the same `(signal, threshold)` identity; the renderer mixes
  them. It is not a reliable policy-selection table without an explicit endpoint.

### D. Line-level measurements are invalid

The committed retrieval-arm-v2 report has recall above 1 for issues **#8796**
(`1.3364`), **#8895** (`1.0290`), and **#8827** (`1.0111`). This is a
measurement defect, not a useful retrieval result.

The code sums intersections of overlapping spans, duplicates a file's first
commit contribution, and compares historical retrieval coordinates to
per-commit/new-head coordinates without a mapping guard in per-issue scoring.
The driver also collapses separated snippets into one minimum-to-maximum
interval, filling gaps with unsupported evidence. Pair line-union calculations
use a maximum rather than a set union. **Withdraw all existing line precision,
recall, and line-overlap conclusions until corrected.**

The saved file precision/recall (v2: 0.450/0.425) describe the saved sets, subject
to the provenance problems above. An unedited retrieved file might be useful
context, irrelevant retrieval, or a scope change; these numbers alone do not
prove it was a context-only read or predict pair-conflict false positives.

## 2. Correct arithmetic, with the limits attached

This table reproduces the existing **non-hub-path replay label** on the enriched
24-pair sample. It is included for traceability, not deployment selection.

| Rule | TP / FP / FN / TN | Precision | Recall | Pairs flagged |
|---|---|---|---|---|
| Retrieval Jaccard > 0 (same as >= 0.01 in these rows) | 8 / 2 / 5 / 9 | 80.0% | 61.5% | 10/24 |
| Retrieval Jaccard >= 0.1 | 4 / 1 / 9 / 10 | 80.0% | 30.8% | 5/24 |
| Base skew >= 40 | 9 / 5 / 4 / 6 | 64.3% | 69.2% | 14/24 |
| Jaccard >= 0.1 OR skew >= 40 | 13 / 5 / 0 / 6 | 72.2% | 100% | 18/24 |
| Jaccard >= 0.01 AND skew >= 25 | 5 / 1 / 8 / 10 | 83.3% | 38.5% | 6/24 |

Even ignoring the systematic errors, the nominal 95% Wilson interval for
the OR rule is **49.1–87.5% precision** and **77.2–100% recall**. These intervals
do not adjust for outcome selection, threshold search, or correlated episodes.
The five false-positive pair IDs are `q-9321-9410`, `q-8992-8923`,
`q-9588-9605`, `q-9028-9014`, and `q-8824-8827`.

Raising Jaccard from any overlap to 0.1 lost half the true positives while
retaining the same observed precision. There is no demonstrated “sweet spot”
at 0.1. Base skew is particularly unsuitable for a dispatch gate: the current
feature uses eventual PR bases, which do not yet exist for unimplemented
dispatch candidates, and the forced-base replay can manufacture its correlation
with conflict. No post-rebase experiment established that the proposed action
dissolves conflicts or improves throughput.

## 3. Final operating recommendations

| Situation | Recommended behavior now | Why |
|---|---|---|
| Ready issues considered together | Retain existing scheduling/dependency rules; record the shared dispatch base and advisory evidence | Preserves the production baseline while collecting the actual decision population. |
| Raw retrieval overlaps | Inspect/rank, and record separate raw, intended-edit, and context-only scores | Relevance does not establish an edit, and recall gaps make zero overlap insufficient to declare safety. |
| Explicit intended edits to the same symbol/contract | Surface a high-priority advisory with evidence and requirement links | Gives the operator an actionable reason, rather than a pseudo-probability. |
| Missing, stale, truncated, or unproven evidence | Report `unknown` with reason; do not turn it into zero risk or an automatic hold | Missingness must remain visible in metrics and decisions. |
| Existing PR has diverged | Use actual Git ancestry and the existing merge/reconciliation checks | Commit count alone neither proves conflict nor justifies rebasing someone else's work. |
| Shared generated or frequently edited path | Classify the actual change and report raw plus weighted evidence | Do not globally exclude lockfiles, manifests, changelogs, or registry/configuration paths; important interactions can occur there. |
| Candidates appear related | Prefer independently reviewable PRs; combine only after explicit requirements/scope review | Similarity alone does not establish a shared deliverable or justify changing issue ownership. |

**No learned threshold is approved for enforcement.** Keep `J > 0`,
`J >= 0.1`, skew 25/40, and the OR rule as named hypotheses to compare in
shadow mode. They must not cause holds, dependencies, automatic combinations,
or unrequested branch mutations.

The final system should expose four distinct decisions rather than one blended
risk number: **evidence freshness**, **predicted edit/contract interaction**,
**actual Git integration result**, and **requirements-based consolidation**.
Each has different evidence and a different owner.

## 4. Required experiments, in delivery order

### A. Repair and qualify the measurement pipeline — #9785, supported by #9783/#9784

**Question:** can we measure an issue-pair prediction without later information
or inherited base changes entering either side?

Before buying another broad retrieval batch:

1. Archive authoritative title/body and selected comments with IDs, revisions,
   timestamps, and hashes. Check actual edit history or a pre-existing snapshot;
   API failures and unavailable earlier text are exclusions, never “no edits.”
2. Choose a source commit evidenced available at cutoff, before either builder
   starts. Publish the complete indexed-file manifest and blob hashes, exclusions,
   provider/SDK versions, query options, and output limits. Reject mixed-revision
   indexes and indexes containing an implementation under evaluation.
3. Freeze queries, raw responses, parsed evidence, classifications, feature
   versions, and coverage **before** the evaluator consumes later PR data.
   Key predictions by issue content *and revision/index/policy*, not just issue ID.
   Preserve query failures as partial/unavailable, with cost and timing recorded.
4. Pin each PR's original pre-repair base/head and landed endpoint separately.
   Attribute its own patch, excluding upstream merges and superseded work.
   Measure net changes as well as edit history; don't count repeated edits twice.
5. Reconstruct A-alone and B-alone on one declared common base. Verify each
   reconstruction preserves that PR's patch. Then evaluate A→B and B→A with
   proper ancestry. A failure to reconstruct or integrate a component alone is
   **unknown for pair-caused conflict**, not a positive label against the other.
6. Distinguish original textual collision, final-landed overlap, upstream drift,
   mechanical reconciliation, and semantic interaction. The last requires
   A-alone/B-alone/combined component controls or adjudicated repair evidence.
7. Implement true interval unions/intersections and revision/rename/insertion
   mapping; leave line outcomes unknown where mapping is unavailable. Require
   [0,1] metric invariants, missingness checks, and exact replay against fixtures
   covering drift-only, true collision, shared-context clean merge, and semantic
   coupling across disjoint files. Confirm against a real Git oracle on AWS.

**Exit:** a small audited replay set with all seven controls passing, explicit
exclusions, reproducible checksums, and no unexplained metrics. Keep this pilot
as an exploratory/debugging set. Recompute any reusable rows; do not relabel
the old artifacts as validated just because their schemas pass.

### B. Collect a representative prospective shadow cohort — #9787

**Question:** which evidence adds value for issues that really could be
dispatched together?

- Capture **every candidate pair within actual dispatch decision windows**,
  including clean, disjoint, cancelled, combined, and uncompleted work. Record
  explicit dependencies, co-dispatch eligibility, queue order, concurrency,
  host/repository, and episode ID. Do not select pairs from later conflicts.
- Cache retrieval once per issue/revision and reuse it across pairs. Keep a
  separate conflict-enriched challenge set for rare shapes; never mix its
  prevalence, precision, or flag rate into the representative cohort.
- Pilot planning target: **200 eligible pair observations across at least
  50 non-overlapping execution groups**, spanning a minimum of two weeks.
  Report a second repository separately before claiming fleet-wide transfer.
  These are collection targets, not a sufficiency claim: if material conflicts
  or flagged decisions are too rare, the result remains inconclusive.
- Initial study budget: **four weeks or 1,000 eligible pair observations,
  whichever comes first**. Plan the first two weeks for development and the
  next two for evaluation; freeze the policy at that boundary. If the budget
  ends before an adequate holdout exists, publish an inconclusive result and
  explicitly plan a new collection window. Do not extend a run merely because
  its current scores are disappointing.
- Freeze an early development window, then a later evaluation window. Group
  by shared issues, PRs, and execution episodes; purge groups spanning the
  boundary so later members never enter training. Published test results may
  not tune the next version on that same holdout.

**Gather for every pair:** the prediction bundle above; exact split assignment;
original/final PR endpoints; patch mapping and reconstruction status; both-order
Git outputs; standalone/combined test evidence where applicable; repair commits,
reviewer rationale, repair time, extra agent tokens/cost, queue delay, critical
path time, and whether an intervention was actually applied. Missing repair
evidence is unknown. Include missing strata and all denominators in the report.

### C. Compare a small frozen policy family on those same observations

**Question:** does Augment improve the existing baseline at an acceptable cost?

Pre-register these arms before the holdout:

1. Existing Curator affected-file baseline and unchanged scheduling.
2. Raw retrieval Jaccard, both containment ratios, and any shared-file indicator.
3. Intended-edit overlap and strongest evidenced symbol/contract interaction;
   context-only evidence stays separate.
4. Arm 3 plus hub-frequency weighting learned from the **training window only**.
5. Actual ancestry/freshness indicators as a separate integration baseline, plus
   the previous OR rule as a falsification control. Recompute features on a
   common dispatch base; never use eventual PR-base skew as an early feature.

Use a modest frozen grid (for example Jaccard 0, 0.01, 0.02, 0.05, 0.1,
0.2; for the zero row explicitly use `> 0`, not `>= 0`). Name the event,
feature version, cohort, split, comparator, and missingness policy on **every
sweep row**. Report TP/FP/FN/TN, abstentions, precision/recall, flag rate,
precision-recall curves, uncertainty resampled by execution group, and incremental
results versus Curator. If intent/weighting does not help, omit that complexity.

Prefer the simplest policy that improves measured decision cost. Define
`loss = unnecessary-delay cost + missed-repair cost + retrieval/operation cost`,
recording queue/critical-path effects rather than summing overlapping pair delays.
Sweep false-negative:false-positive costs at **1:1, 3:1, and 10:1**; publish the
trade-off curve. Those ratios are sensitivity assumptions, not inferred prices.
Use measured costs to pick one policy in development, then freeze it.

### D. Test the action, then decide enforcement — #9781 after #9787

**Question:** does acting on the signal improve delivery, rather than merely
predict a textual merge result?

Shadow accuracy cannot establish that rebasing or serialization saves time.
Once a frozen policy qualifies, run a limited, reversible experiment over
independent **dispatch groups** comparing unchanged scheduling with the proposed
action. Separate source-base synchronization from pair serialization; leave
automatic issue combination out of this trial. Pin policy versions and log
deviations and rollback triggers. This needs the operator's rollout decision.

**Recommended promotion criteria (product targets, not achievements):**

- All provenance/replay controls above pass; performance is measured on the
  representative untouched temporal holdout with unknowns reported.
- For an automatic serialization policy, target **precision >= 90%**, with a
  **95% lower bound >= 80%**, plus **recall >= 70%** on material pair-caused
  reconciliation and a lower bound >= 50%. Require at least **50 resolved
  flagged decisions and 30 positive events** across sufficient independent
  groups; clustered uncertainty can demand more. A flag cap or poor coverage
  may not silently discard hard cases to pass these gates.
- Measured expected cost improves over Curator and the unchanged schedule;
  the action trial shows positive net benefit with uncertainty excluding no
  improvement, and **no more than 5% p95 critical-path regression**. Inspect
  both aggregate and repository-level results. Set the trial's sample plan
  from the observed variance before running it.
- If the sample cap/time window ends without these conditions, remain advisory.
  Do not keep searching thresholds on the holdout until one passes.

The precision preference is intentional: unnecessary serialization taxes
healthy parallel work, while existing review/integration checks are already the
backstop for missed collisions. A different measured repair cost can justify a
different operating point, but that trade-off must be stated explicitly.

## 5. Move-forward checklist and ownership

1. **#9785:** withdraw the invalid deployment claims; qualify provenance,
   patch reconstruction, line math, label definitions, and endpoint-aware tables.
2. **#9783:** use the supported revision-bound provider path; preserve raw
   responses/index manifests, partial statuses, credential-free replay, and
   per-issue cache reuse. Validate the production CLI, not only an ad-hoc driver.
3. **#9784:** implement evidence-backed edit/contract intent and freshness;
   do not equate a missing Curator path with a proven context-only role.
4. **#9786:** publish the captured denominators, provenance, latency/cost,
   missingness, and action outcomes, with durable raw artifacts linked.
5. **#9787:** run the representative forward study and locked policy comparison.
6. **#9781:** propose enforcement only after the qualification and action gates.

These steps can continue on stacked branches. The immediate next deliverable
is **trusted measurement plus a live shadow recorder**, not another large
outcome-selected retrieval batch. That is the shortest defensible route to a
sensible production threshold.

## Audit references

- Saved counts: [v2 classified pairs](v2-cohort/v2-pairs-classified.json),
  [retrieval pairs](retrieval-arm-v2/per_pair.jsonl),
  [retrieval manifest](retrieval-arm-v2/manifest-retrieval.json).
- Machine-readable corrected totals and rule counts:
  [decision-audit.json](decision-audit.json).
- Inputs and sampling: [v2 manifest](v2-cohort/manifest-v2.json),
  [selected retrieval cases](retrieval-arm-v2/retrieval-subsample.json),
  [runner](retrieval-arm-v2/run_retrieval_v2.py).
- Flawed measurements to repair:
  [`conflict.rs`](../../../loom-daemon/src/overlap_replay/conflict.rs),
  [`outcome.rs`](../../../loom-daemon/src/overlap_replay/outcome.rs),
  [`patch.rs`](../../../loom-daemon/src/overlap_replay/patch.rs),
  [`overlap.rs`](../../../loom-daemon/src/overlap_replay/overlap.rs),
  [`score.rs`](../../../loom-daemon/src/overlap_replay/score.rs), and
  [threshold renderer](v2-cohort/harmonize.py), at the audited commit above.

For each manifest pair, reproduce the chronological check with
`git show -s --format='%aI%n%cI' <historical_commit>` and compare as UTC with
`cutoff`. Check `git merge-base --is-ancestor <pr_head> <historical_commit>`
for outcome leakage and the reverse direction for replay-base ancestry.
Read `createdAt` and `lastEditedAt` through GitHub GraphQL's `issue(number:N)`;
an edit after cutoff needs a genuine earlier snapshot. These checks are
necessary controls, not sufficient proof by themselves.

Frozen file SHA-256 references:

| Input | SHA-256 |
|---|---|
| `v2-cohort/v2-pairs-classified.json` | `54233141b4cec0fa7d34766bb93b4c2c66557312749c40137607e218c0faac3f` |
| `retrieval-arm-v2/per_pair.jsonl` | `f1b225116a751a6f103c588037ad368f59b2baeeba8cd287b44937e1cc587371` |
| `retrieval-arm-v2/manifest-retrieval.json` | `73a8083f6b709edb745a24e61d6e9e084775d3798a2e07066908ea803950c24c` |
