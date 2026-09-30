# Story points: verification note (Issues #9430, #9521)

Honest record of what was and was not run.

## Run: SP1–SP4 against the D1 cache snapshot (2026-09-30)

Executed by the #9521 session — **no Cloudflare credentials used**. Source:
the story-points experiment's local D1 cache dump
(`loom-experiments/story-points/d1/d1_sweeps.sqlite`, snapshot taken
2026-09-29 05:17 while the store was evicting; 26,260 `sweep.outcome`
records, emitted 2026-08-15 → 2026-09-29). The artifacts are
D1/SQLite-dialect by design, so the chain runs locally:

1. `sweep-facts-rollup.sql` applied to a scratch copy of the dump (builds
   `sweep_facts`; window 2026-08-15 → 2026-10-01 as committed).
2. `sweep-facts/landed-size.sql` loaded with its unfitted params (SP4's NULL
   class is its honest answer until the fit exists).
3. `story-points-queries.sql` **verbatim** — `sp_window` is already
   2026-08-15 → 2026-09-29, matching the snapshot.

**Local patch, disclosed**: the dump's `records` projection carries 9 columns
and lacks the `schema_version` envelope column the rollup SELECTs; it was
added with constant 1 (bookkeeping only — no SP query reads it, and the
payloads do not carry it either). Aggregate numbers only are reproduced here;
the experiment repo is private and no per-issue rows or issue text were
copied.

### SP1 — filter accounting (the quoted numbers, now extracted)

| measure | value | the doc said |
|---|---|---|
| landings (fallback predicate: `success` + `pr_number`) | 332 | 329 |
| relaxed-clean (exactly one judge, no doctor) | **284** | 284 ✓ |
| excluded: not exactly one judge | **42** | unknown ("the 45 split") |
| excluded: doctor phase | **6** | unknown |
| strict-clean (`doctor_cycles = 0`, one judge) | **119** | 119 ✓ |
| `doctor_cycles` absent | 196 | — |
| with tokens | 298 | 295 |
| with `hw_lines` | 0 | — (see limits) |

329 vs 332: the snapshot carries three landings the #9430 analysis predated;
all three fall in the excluded set, so relaxed-clean is unchanged at 284.

### SP2 — per-model clean-token medians (normalization factors)

| model | n | median tokens |
|---|---|---|
| (blank / unnamed) | 154 | 26.05M |
| `claude-sonnet-5` | 36 | 27.07M |
| `claude-opus-5` | 34 | 26.42M |
| `claude-opus-5-5` | 11 | 14.03M |
| `<unattributed>` | 13 | 46.29M |
| `claude-fable-5-1` / `claude-haiku-4-5` / `glm-5.3` | 1 each | 137.0M / 12.4M / 31.9M |

`claude-sonnet-5` and `claude-opus-5` sit within 3% of each other;
`claude-opus-5-5` runs at ~0.52× sonnet-5 — the rubric's "~1.5x between
Sonnet 5 and Opus 5" line understates the newest model's gap.

### SP3 / SP4 — the hand-written axes: honest NULL

SP3 returned zero rows; SP4 only the NULL class (n = 284, pooled median
tokens 24.1M). `hw_lines_*` / `hw_files` are absent from **every** record in
this snapshot: the #9466 emitters landed 2026-09-29/30 and the gateway only
admits the keys since #9586 (2026-09-30) — all after the dump was taken. The
columns become extractable once the emitters accumulate a window.

SP4's even-n median is already aligned with SP2's (both average row numbers
`(n+1)/2` and `(n+2)/2`) — verified, no query change needed.

### Class-median cross-check (experiment bucket table vs the doc)

`rubric_build/bucket_summary.csv` from the experiment (tuning set, n = 125;
aggregate figures only):

| class | n | tokens med | doc | hw-lines med | doc | files med | doc |
|---|---|---|---|---|---|---|---|
| 1 | 21 | 11.3M | ~14M | 69 | ~12 | 2.0 | 1 |
| 2 | 27 | 19.6M | ~19M | 115 | ~110 | 3.0 | 2 |
| 3 | 37 | 33.1M | ~30M | 311 | ~285 | 4.0 | 4 |
| 5 | 22 | 50.4M | ~50M | 1020 | ~610 | 7.5 | 6 |
| 8 | 12 | 79.5M | ~73M | 667 | ~1000 | 11.0 | 8 |
| 13 | 5 | 116.7M | ~110M | 549 | ~2000 | 8.0 | 8+ |

- **Tokens and files: confirmed** — every class inside the calibration
  tolerance ([0.5x, 2x], CAL4); class-1 files sits exactly at the 2x edge.
- **Hand-written lines: NOT confirmed — and left as issued.** The experiment
  is internally inconsistent on this axis: the bucket table above (and the
  8/13/21 profile: "LOC is uninformative (8 median 667 < 5 median 1020)")
  disagrees with the ratio table (1 : 8.5 : 21 : 47 : 82 : 197) the doc's
  lines column was anchored from — by 5.8x at class 1 — and is non-monotonic
  at 8/13 (n = 12 / 5). Re-anchoring fleet-wide sizing bounds on an
  internally-contradicted n = 125 table is not a confirmation; the lines
  column stays provisional pending CAL4 drift on the labeled population (the
  rubric's own revision protocol), which begins accumulating now that the
  emitters and the gateway admission have landed.

## Still not run

- SP1–SP4 against live SigNoz: the hand-written columns only flow since
  #9586/#9466 (2026-09-30); a meaningful window accumulates from that date.
- CAL1–CAL5 against the labeled population: `points:*` labels began
  2026-09-29 (#9528); the loop needs accumulated assigned-vs-actual pairs.
