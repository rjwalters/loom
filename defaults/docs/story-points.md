# Story points: the Fibonacci rubric

**Status: provisional — derived from the first measured clean-landing
distributions (windows cited below) and subject to the #9434 calibration
loop, which is this rubric's designated revision path.** The scale itself
(1 / 2 / 3 / 5 / 8 / 13) is fixed by the adjudicated epic #9429; only the
bound positions may move as calibration data accumulates.

Story points size **one issue's clean-landing cost**: what a sweep that lands
the issue in a single pass — no Doctor repair, no changes-requested retry —
consumes. They are assigned by the Curator at curation time, before any sweep
runs. The numbers below exist so the buckets mean something measured rather
than something felt: they were extracted from actual landings by the
committed question set
[`defaults/observability/story-point-questions.md`](https://github.com/rjwalters/loom/blob/main/defaults/observability/story-point-questions.md)
+ `story-point-queries.sql` (SP1–SP5), not invented.

**What points are not.** They are not additive (a 13 is not thirteen 1s —
measured bucket ratios are ~8× on tokens and ~70× on hand-written lines, not
13× on either); they are not wall-clock targets; and per the #9429
experiment, **rubric wording barely affects accuracy** (seven variants scored
within 0.04 of each other on a paired n=240 test) — this document's job is to
define the units, decisively, not to be poetic.

## The anchor, and why it is tokens

The **primary anchor is total tokens (`tokens_in + tokens_out`)**: it is
queue-independent by construction, so a completely clear dispatch queue
cannot bend the measure, and it prices everything the sweep actually did
(curator reading, builder writing, judge reviewing — the in side includes
cache traffic, which is the schema's definition of `tokens_in`).

Two caveats the experiment (#9429) established, which the cross-checks exist
to absorb:

- **A fixed floor dominates small work.** Tokens scale roughly as
  hand-written-lines^0.25 — every sweep pays ~10M+ tokens of
  prompt/context/protocol regardless of size, so token buckets are compressed
  at the low end relative to lines.
- **Tokens are model-dependent** (~1.5× between the fleet's two dominant
  models). The derivation window was ~96% one model, so the ranges carry
  that mix; expect at most one bucket of drift on the other, and see
  "Assigning points" step 3.

Wall-clock is reported for intuition only and **must never assign or adjust a
bucket**: it is queue-sensitive (sweep start → terminal includes queue wait),
and empirically nearly flat across the scale — live p50 per bucket ran
977…2 484 s while tokens moved ~9×.

## The scale

| Points | Tokens, total (anchor) | Hand-written lines (cross-check) | Files (cross-check) | Wall p50 (intuition only) |
| --- | --- | --- | --- | --- |
| 1 | ≤ 16 M | ≤ ~30 | 1 | ~20 min |
| 2 | 16–24 M | ~30–100 | 2 | ~20 min |
| 3 | 24–39 M | ~100–250 | 3–4 | ~35 min |
| 5 | 39–60 M | ~250–550 | 5–6 | ~35 min |
| 8 | 60–90 M | ~550–1 000 | 7–8 | ~40 min |
| 13 | > 90 M | > ~1 000 | 8+ / multi-area | ~40 min |

Token ranges are the assignment criterion; the lines and files columns are
cross-checks an estimator can actually reason about before any code exists
(nobody can predict tokens directly — they are what the buckets *mean*, lines
and files are how you *estimate*). All three columns come from measured
distributions; sources below.

## Assigning points to a fresh issue

1. **Estimate the hand-written change**: the files the issue will touch and
   the lines a competent agent will write or edit **by hand**. Exclude
   generated output the work is expected to produce (logs, JSON records,
   netlists, DEF, build artifacts) — in silicon-flavoured repos generated
   lines otherwise dominate the diff and inflate every bucket (the live
   window's largest clean landing changed 16 261 total lines for 655
   hand-written ones).
2. **Pick the bucket** whose lines range contains the estimate; use the files
   column to confirm. A "few files, light edits" issue is a 1–2; a "new
   module plus wiring plus tests" issue is a 3–5; "touches a core path in
   several places with real rework" is an 8.
3. **Sanity-check against the token range** for the model the issue will run
   on. The table assumes the sonnet-dominated mix of the derivation window;
   if the issue is known to run a heavier model, expect the same work to sit
   up to one bucket higher in tokens — adjust the bucket only when the lines
   estimate was already borderline.
4. **When measures disagree**: if lines and files suggest different buckets,
   take the **higher** — coordination cost grows with both, and
   underestimating size starves the sweep (too-small context, too-cheap
   model) more than overestimating wastes it. Never lower a bucket because
   wall-clock "looks short"; never raise one because total diff lines
   (including generated output) look large — re-estimate the hand-written
   part instead.
5. **Too big for one issue** ⇒ **split it.** Any of these means the issue is
   not a 13 but several issues: the hand-written estimate exceeds ~2 000
   lines or ~8 files of new logic; the work has more than one independently
   landable deliverable (two features, a feature **and** its rollout, code
   **and** its documentation campaign); or it must land in more than one
   repository. Split into separately-judgeable issues, then size each with
   this table. A monolithic 13+ does not merely risk overrun — it makes
   Judge feedback arrive too late to act on.

A Curator needs nothing beyond this table and the issue text to assign
points. If a case genuinely resists steps 1–5, default to the **higher** of
the two candidate buckets and say why in the curation comment.

## How these bounds were derived

Two independent measured sources, cross-validated; neither invented.

**(a) D1 tuning-set medians** (issue #9430 comment, 2026-09-29; the
story-points experiment's per-bucket synthesis over issues bucketed by
measured size — n≈175 clean landings out of 284 relaxed-filter landings,
fleet store 2026-08-15..09-29):

| points | tokens | hand-written lines | files |
| --- | --- | --- | --- |
| 1 | ~14 M | ~12 | 1 |
| 2 | ~19 M | ~110 | 2 |
| 3 | ~30 M | ~285 | 4 |
| 5 | ~50 M | ~610 | 6 |
| 8 | ~73 M | ~1 000 | 8 |
| 13 | ~110 M | ~2 000 | 8+ |

**(b) Live SigNoz extraction** (this change; the committed SP queries run
against the fleet SigNoz on the harness-ops host, read-only, on 2026-09-29;
window 2026-09-14 00:00 → 2026-09-30 00:00 UTC, 10 579 `sweep.outcome` rows /
10 578 distinct sweeps, 172 clean landings of which 138 token-measured —
80%, because clean landings self-select for a live registry entry at
terminal transition). SP2 verbatim:

```
tokens         138  11956069  27335482  75141054  158674285
wall_sec       172  691       1800      3981      14261
lines_changed  172  3         191       1979      138203
```

(columns: measure, n, p10, p50, p90, max; `lines_changed` is **total**
churn — generated output included — which is why its p90/max run far past
the hand-written cross-check column). The token distribution reproduces the
experiment's independent store almost exactly (p10/p50/p90 = 12.0 / 27.3 /
75.1 M here vs 12.1 / 26.8 / 70.9 M on D1).

**The cut points** are the geometric means of adjacent D1 bucket medians,
rounded to two significant figures:
√(14·19)=16.3 M, √(19·30)=23.9 M, √(30·50)=38.7 M, √(50·73)=60.4 M,
√(73·110)=89.6 M → **16 / 24 / 39 / 60 / 90 M**. Geometric means because the
medians are log-spaced; rounding to two figures because finer precision would
be false precision at n=138. Validation: SP5's live per-bucket token medians
all land inside their own ranges —

```
1   27  11876254   28   1127
2   27  18296416   75   977
3   42  29562090   152  2032
5   24  49384458   494  1884
8   8   76877723   520  2440
13  10  111596768  860  2484
```

(columns: points, landings, tokens p50, total-lines p50, wall p50) — and they
track the D1 medians (11.9 vs ~14, 18.3 vs ~19, 29.6 vs ~30, 49.4 vs ~50,
76.9 vs ~73, 111.6 vs ~110 M). The lines/files cross-check columns in the
scale table interpolate the two sources' line medians (hand-written where
available, live total-lines medians scaled by the observed hand-written
share); they are cross-checks, not cuts, so interpolation is honest there in
a way it would not be for the anchor.

Full verbatim outputs, the executed SQL, and the access path are recorded in
[`defaults/observability/signoz/evidence.md`](https://github.com/rjwalters/loom/blob/main/defaults/observability/signoz/evidence.md)
§ "Story-point calibration, executed live".

## The clean-landing filter (what the numbers describe)

`result = 'success'` **and** a PR was opened (`pr_number` present — exactly
`disposition = 'landed'` by #9441's invariant; the gateway strips
`loom.disposition` itself) **and** no Doctor repair (`doctor_cycles = 0`
where the timeline was read, else no `doctor` phase entry) **and** exactly
one `judge` phase entry. SP3's exclusion accounting over the same window,
verbatim:

```
10579  8473  1908  9213  1060  41  172  34  0
```

(columns: ships, not_success, success_without_pr, without_phase_breakdown,
judge_entries_not_one, doctor_engaged, clean_landings, clean_tokens_absent,
clean_tokens_suspect). Read that beside every number above: 172 of 10 579
ships are clean landings; 1 908 "successes" were no-op re-dispatches a bare
`result = 'success'` filter would have admitted (#9441); 9 213 ships carried
no phase breakdown and so cannot be verified clean at all.

Known gaps in this derivation, all one-directional and none silent: the
window is 16 days on one mostly-single-model mix; 34 of 172 clean landings
carry no token pair (excluded from token percentiles, kept in lines/wall
percentiles); `doctor_cycles` exists only since 2026-09-18 (the phase-array
fallback covers earlier records); per-phase token attribution (#9443) is
absent on these records, so "clean tokens" are the landing sweep's whole
usage including its trailing segment; and pre-#9442 records can carry
path-shaped `repo` values (one appears in SP1's live output), which affects
repo grouping, not cost measures.

## Revision path

#9434 owns calibration: re-run SP2/SP4/SP5 (bound cut points as parameters —
never edit SQL) over accumulating windows — on the D1 `sweep_facts` rollup
(#9446) once backfilled, which is where history survives raw retention — and
move the cut points if per-bucket medians drift out of their ranges.
Model-normalized tokens (#9466's landed-size index) is the designated
successor anchor if the model mix diversifies; until then the token table
carries the sonnet-mix caveat, not a hidden normalization.
