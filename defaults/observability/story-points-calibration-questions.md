# Story-points calibration: the canonical question set

The standing answer to *"did we get the story points right?"* (Issue #9434,
epic #9429). The Curator assigns a `points:*` label before the work runs;
the sweep then lands and measures what the work actually cost. This question
set scores the estimate against the actual, so a wrong bucket bound becomes
a queryable fact and revising the rubric becomes a normal, expected change
instead of an argument.

Every question `CAL<n>` is a committed SQL statement of the same number in
[`story-points-calibration-queries.sql`](story-points-calibration-queries.sql)
— the same discipline as [`cycle-time-questions.md`](cycle-time-questions.md)
and the (since removed) ETA accuracy work it mirrored (#9289): the estimator
differed (Curator vs heuristic) but the scoring discipline — per-revision
provenance, scored-vs-counted separation, baselines stated up front — is the
same.

**Scope boundary with #9433** (kept disjoint on purpose): this file is the
*calibration* side — estimate vs actual per landing. The *throughput* side
(points landed per day/week) is SF8 in
[`sweep-facts/sweep-facts-questions.md`](sweep-facts/sweep-facts-questions.md).
Both read the same fact table and the same landed-size seam; neither answers
the other's question.

## Definitions (fix these before reading any number)

| Term | Definition |
| --- | --- |
| **landing** | One `sweep.outcome` fact with `disposition = 'landed'`, or the documented pre-#9441 fallback (`disposition IS NULL AND result = 'success' AND pr_number IS NOT NULL`). The SAME predicate as `landed-size.sql`'s `landings` CTE; a contract test fails if the copies diverge. |
| **assigned bucket** | The landing issue's single `points:*` label, as recorded on the fact row (`story_points`, #9432/#9536). Ordinal — 1/2/3/5/8/13 — never a unit, never summed. Absent means unsized (counted by CAL1), never zero. |
| **actual measures** | The landing sweep's own cost: **LOC** (`hw_lines` = hand-written added + deleted, and `hw_files`) — the rubric's primary anchor; **tokens** (`tokens_in + tokens_out`, per sweep not per phase, #9443) and **wall-clock** (`total_duration_sec`) — the rubric's cross-checks, reported but never the verdict; **LSI** (`landed-size.sql`) — the size axis of #9466, empty (NULL) until that file's parameter set is fitted. |
| **clean-landing filter** | Exactly one `judge` phase and no `doctor` phase in `phase_durations` (the relaxed filter of `story-points-queries.sql`'s `sp_landings`). Only clean landings are SCORED: churn cost is process cost, not estimate error. |
| **churn** | A sized landing the filter excluded, classified by `churn_class`: `no_phase_data` (no breakdown — filter unverifiable), `no_judge_phase` (never judged), `rejudge_loop` (>1 judge), `doctor_repair` (doctor phase), `doctor_cycles_only` (doctor_cycles > 0 with one judge and no doctor phase). Counted by CAL1/CAL6 and carried beside every score — never silently dropped. |
| **rubric revision** | The marker of the rubric a landing was sized under, from the `rubric_revisions` view, which mirrors `docs/story-points.md` § *Revision history*. Every view groups by it: a rubric change mid-window shows as two populations, never a silent average (the eta-queries provenance rule). Attribution keys on **landing time** as a proxy for sizing time — sizing happens before landing and sweeps carry no sizing timestamp yet, so a landing that crosses a revision boundary is attributed to the revision in force when it landed; when a sizing timestamp exists on the sweep records, key on that instead. |
| **implied class** | The class a landing's measured `hw_lines` maps to under the rubric's own rule — a class covers the range around its median, midway to the neighbours on a log scale, i.e. up to the geometric mean of adjacent medians. Computed in exact integer arithmetic (`hw_lines² ≤ m_k·m_{k+1}`). |
| **median / quartiles** | Median = mean of order statistics ⌊(n+1)/2⌋ and ⌊(n+2)/2⌋ (SP4's exact convention). Each quartile averages the pair of order statistics at ⌊q(n+1)⌋ and ⌈q(n+1)⌉, clamped to [1, n] — the same inclusive-pair convention as the median, degenerate to a single position whenever q(n+1) is an integer. |
| **Spearman ρ** | Pearson's r over ranks, with ties given TRUE average ranks on both sides — global position minus within-tie position plus the group's mean offset (k+1)/2 (points are discrete; ties are the norm). NULL when n < 2 or either side is constant — an honest absence, never a fabricated 0. The denominator's square root is computed by Newton's method in a recursive CTE so the file runs on every SQLite, not only those built with the math functions. |
| **drift tolerance** | A class is `held` while its measured median stays within **[0.5×, 2×]** of the rubric's claimed median — one Fibonacci step, the resolution the ordinal classes carry — with **n ≥ 20**; below that the verdict is `insufficient_n`, and a class the window never scored reads `not_scored`. |

## The questions

| ID | Question | Grouping | Reads |
| --- | --- | --- | --- |
| **CAL1** | *What is the joined population, and is the loop warm yet?* Landings, sized vs unsized, scored vs excluded-by-reason, per rubric revision — plus the revision-gap and out-of-vocabulary counts. Read this FIRST: an all-zero row is "not yet warm", not a broken query. | per revision | points presence, churn split |
| **CAL2** | *Per assigned bucket, what did the work actually cost?* Median, p25 and p75 of each actual measure, with the churn-excluded count beside the figures. | revision × bucket × measure | LOC, tokens, wall, LSI |
| **CAL3** | *Do bigger estimates actually cost more?* Spearman ρ between assigned points and each actual measure, plus the within-one-bucket rate — the one query that answers "how accurate are assigned points?" for any window. | revision × measure | ranks, implied-class gap |
| **CAL4** | *Is each bucket bound where the rubric claims?* Measured median vs claimed median per class, both ratio-to-class-1 ladders, and the held/drifted verdict. | revision × class | `rubric_classes` vs measured medians |
| **CAL5** | *Which issues were misassigned, and in which direction?* Named outliers ≥ 2 buckets off, both directions, with their actual measures. | per outlier | implied class, measures |
| **CAL6** | *What did churn exclude, and why?* The excluded sized landings per bucket and reason. | revision × bucket × reason | `churn_class` |

## Baselines CAL3 is read against

Stated up front, from the story-points experiment (#9429/#9466, quoted in
#9434's comments), because a correlation with nothing to compare it to is
just a number:

- **The bar**: the existing **complexity tier** holds holdout ρ ≈ **0.64**
  against landed size. "Points are accurate" means *better than what we
  already have*, not "ρ looks high".
- **The reference**: retrospective points-vs-landed-size ρ ≈ **0.82**
  [0.74, 0.88], with **91%** of landings within one bucket of their implied
  class.
- **The ceiling**: measurement noise alone caps attainable ρ at ≈ **0.90**;
  chasing it is chasing noise.

## The revision path (how the rubric changes)

Calibration output revising the rubric is the loop WORKING, not an incident.
The procedure, once CAL4 reports `drifted` for a class (or CAL5 shows a
persistent one-direction misassignment pattern):

1. Update the class table and append a row to `## Revision history` in
   [`../docs/story-points.md`](../docs/story-points.md) — marker `vN+1`,
   date, what moved, and the CAL4/CAL5 run that motivated it.
2. Add the new window row to `rubric_revisions` and the new class medians to
   `rubric_classes` in the queries file (`DROP VIEW` both first;
   `CREATE VIEW IF NOT EXISTS` will not replace). The contract test fails if
   the doc and the SQL ever name different markers or different medians.
3. Re-run. Every view now reports the two revisions as separate populations;
   nothing is silently averaged across the change.

Until a second revision exists, `rubric_revisions` carries the placeholder:
one `v1` row from the epoch of the store to the open present. The acceptance
criterion "a documented rubric revision has happened at least once as a
result of calibration output — or the report states the rubric held within
stated tolerance" is answered standing by CAL4's `verdict` column: `held`
within the tolerance above is the stated tolerance; every `drifted` is either
revised (step 1–3) or consciously accepted on the record.

One asymmetry: an UPWARD `drifted` at class 13 does not move a bound — the
rubric's own rule is that anything bigger than 13 must be SPLIT into
sub-issues, so that verdict is a curation fix, not a rubric change.

## Churn accounting (excluded is counted, never dropped)

The clean-landing filter exists because a landing that went through repair
or re-judge measures process cost, not estimate error — but excluding it
must not make it invisible:

- **CAL1** counts every exclusion reason, sized and unsized, per revision.
- **CAL2** and **CAL4** carry `excluded_churn` beside the per-bucket figures.
- **CAL3** and **CAL5** carry `excluded_from_score` beside the score.
- **CAL6** is the reason-level breakdown, per bucket.

## Honest empty is the expected initial state

`points:*` labels began landing 2026-09-29 (#9528). The joined population
(issues with a points label AND a completed sweep) is expected to be tiny or
empty for the first windows. CAL1 therefore emits one all-zero row per
rubric revision even over an empty window, and every other view states its
`n` beside every figure. **No sample data is committed anywhere in these
artifacts** — the synthetic population that proves the SQL correct lives
only in the Rust contract test.

## What this question set cannot answer (and why)

- **Bootstrap confidence intervals on ρ.** Committed SQL computes the point
  estimate; resampling is an operator-side step (the retrospective CI
  [0.74, 0.88] above came from exactly such a step). CAL3 reports n so the
  reader can see which window can carry one.
- **Proper scoring rules (log loss, ranked probability score).** Those need
  the Curator to record a *distribution* over buckets; today's telemetry
  carries a single point class (`loom.story_points`). If a distribution is
  ever emitted, add the scoring-rule view here — the join key
  (repo, issue, sweep_id) already exists.
- **Per-phase token cost.** Tokens are per sweep, not per phase (#9443), so
  the tokens measure is the landing sweep's total, clean landing or not.
- **Token plausibility.** The tokens arm reads sweep totals as published;
  ~19% of token values are implausible (#9454) and `suspect` flagging lives
  in SF6 — read that beside any tokens figure here.
- **LSI now that the landed-size fit has shipped.** `landed-size.sql` carries
  the `v1-2026-10-02` fit (#9934); the `lsi` arm fills as soon as the D1
  rollup (`sweep-facts-rollup.sql` + the landed-size view) runs against the
  production store — no change to this file. The fixture's NULL-LSI
  substitution below stands for a different reason: the bundled SQLite in CI
  lacks the math functions D1 provides.

## Verification status

| Artifact | Status |
| --- | --- |
| `story-points-calibration-queries.sql` | **Fixture-executed in ordinary CI**: `loom-daemon/tests/story_points_calibration/execution.rs` builds a synthetic `sweep_facts` through the committed rollup INSERT (`sweep-facts/sweep-facts-rollup.sql`) and executes this file against it, asserting hand-computed medians, quartiles, Spearman ρ (including tie handling), drift ratios, misassignment picks and churn counts. The fixture substitutes a shape-compatible `issue_landed_size` view whose LSI is NULL — the committed view is now fitted (`v1-2026-10-02`, #9934), but the bundled SQLite in CI still lacks the math functions D1 provides, so the substitution remains what makes the fixture executable. |
| Static contracts (question set, window binding, landing predicate, revision markers, doc mirror, churn-beside-every-score) | Same test file, derived from the committed artifacts (`story-points-calibration-queries.sql`, `../docs/story-points.md`, `sweep-facts/landed-size.sql`). |
| Live D1 execution | **Not run from the Builder environment** — `wrangler d1 execute --remote` needs `CLOUDFLARE_API_TOKEN`, which is not provisioned for this session (same discipline as [`story-points-evidence.md`](story-points-evidence.md); no credentials were sought elsewhere). The queries are correct and verifiable via the fixture; the first live run pastes CAL1's counts into the evidence trail. |
