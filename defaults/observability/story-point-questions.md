# Story-point calibration: the canonical question set

The standing answer to *"what did a clean landing actually cost?"* — defined
**once**, here, so that the rubric in [`defaults/docs/story-points.md`](../docs/story-points.md)
is anchored in extracted distributions rather than invented numbers (Issue
#9430, the first phase of the Fibonacci story-points epic #9429). Like the
cycle-time set (#8665), whose files this mirrors, every question `SP<n>` is a
committed SQL statement of the same number in
[`story-point-queries.sql`](story-point-queries.sql), and that file is
**backend-neutral** — it reads one view, `loom_analytics.raw_landing_cost`,
which both backends populate from their own logs table via their own
extraction artifact ([`signoz/story-point-extract.sql`](signoz/story-point-extract.sql),
[`clickstack/story-point-extract.sql`](clickstack/story-point-extract.sql)).
Parity is a property of the design, not of two hand-synchronized query sets.

## Definitions (fix these before reading any number)

| Term | Definition |
| --- | --- |
| **ship** | One `sweep.outcome` record: a sweep that reached a terminal state. One row per `(repo, sweep_id)` here — the extraction view collapses at-least-once redeliveries, because this question set has no rollup of its own yet (the retention-safe home is #9446's `sweep_facts`; the throughput rollup is #9433's pattern). |
| **clean landing** | The filter that makes cost comparable across sweeps (Issue #9430's deliverable, validated by the analysis): `result = 'success'` **and** a PR was opened **and** no Doctor repair **and** exactly one `judge` phase entry. Concretely: `result = 'success' AND pr_number IS NOT NULL AND NOT doctor_engaged AND judge_entries = 1`, defined once in the extraction views as the `clean_landing` column. A ship with no phase breakdown cannot be verified clean (`judge_entries` reads 0) and is excluded — counted by SP3, never silently admitted. |
| **landed** | `pr_number` is present. `loom.disposition` would be the nicer test (#9441) but the gateway's `keep_keys` allowlist strips it, so it reads NULL on any gateway-fed backend; `pr_number` presence is exactly equivalent by the telemetry schema's invariant `disposition = 'landed' ⇔ pr_number present`. This is also why these queries do not read the sweep-facts view (`raw_sweep_fact`): as of 2026-09-29 its `disposition` column depends on that same stripped key. |
| **doctor_engaged** | `doctor_cycles > 0` when the forge timeline was read (the field exists only since 2026-09-18), otherwise the presence of a `doctor` phase entry — the same coalesce the cycle-time `ship` view uses, so the two question sets cannot disagree about what a repair is. |
| **tokens** | `tokens_in + tokens_out`, raw counts, the **primary anchor**: queue-independent by construction, so a completely clear queue cannot bend the measure. The in side includes cache reads and writes — that is the schema's definition of `tokens_in`, not a choice made here. |
| **lines changed** | `lines_added + lines_deleted` from `git diff --numstat`, a secondary cross-check. It counts **total** churn: generated output (EDA logs, JSON records, netlists, DEF) is included, which in silicon-flavoured repos can dominate the diff. |
| **wall-clock** | `loom.total_duration_sec`, a secondary cross-check with the **queue-depth caveat**: it spans sweep start to terminal state, so queue wait and fixed per-sweep overhead land in it. SP5's live run measured it nearly flat across buckets (p50 972 s in bucket 2 vs 2 484 s in bucket 13 while tokens move ~8×) — it must never anchor a bound. |
| **measured subset** | Clean landings where the token pair is present and `tokens_status` is `measured` **or absent** (pre-#9440 records carry no status but a present pair; `suspect` values are refused by the plausibility guard, #9454, and excluded). Token coverage is a live issue (#9440: ~10 % of all outcomes fleet-wide; the clean-landing population self-selects for it — 138 of 172 in the derivation window). |
| **absent vs. zero** | An unmeasured field is `NULL`, never `0`. SP3 exists to keep that distinction visible beside every number SP2 reports. |

## The questions

| ID | Question | Grouping | Reads |
| --- | --- | --- | --- |
| **SP1** | *What did this window's clean landings cost?* — each landing with its three measures, largest first. | per landing | `tokens`, `lines_changed`, `wall_sec` |
| **SP2** | *What is the cost distribution?* — p10/p50/p90/max per measure over the measured clean landings. **This is the rubric-derivation query**: the bounds in `defaults/docs/story-points.md` cite its output. | per measure | all three measures |
| **SP3** | *What did the filter exclude, and why?* — ships, non-successes, success-without-PR (the #9441 no-op shape), missing phase breakdowns, judge retries, Doctor repairs, and the token-measurement hole among clean landings. Read **beside** SP2: the distribution describes survivors, and a blank there is one of these, never a measured zero. | per window | filter components |
| **SP4** | *How do the cuts slice the distribution?* — bucket occupancy under candidate token cut points, passed as parameters. The #9434 calibration hook: bounds are re-derived from SP2 and re-checked here, never edited into the SQL. | per bucket | `tokens` vs cut params |
| **SP5** | *Do the anchor and the cross-checks agree?* — per bucket, the median of each measure beside the others; where lines or wall separate buckets far less than tokens do, they are weaker discriminators, and the rubric's "measures disagree" instructions consume this table. | per bucket | all three measures |

## What this question set cannot answer (and why)

- **History older than the raw retention.** These queries read the raw log
  table through the extraction view; raw logs die in ~7 days (ClickStack TTL)
  or whatever retention the SigNoz operator set. Durable history is #9446's
  `sweep_facts` rollup in D1 — once its backfill has run, calibration belongs
  there, and SP2/SP4/SP5 are the pattern it should answer to.
- **Per-phase token attribution inside one sweep.** Tokens ride the sweep, not
  the phase (#9443 — per-phase counters exist in the schema but the fleet's
  records in the derivation window predate them). "Clean" tokens are therefore
  the landing sweep's whole usage: curator + builder + the one judge, and the
  trailing in-flight segment too. On clean landings the difference is the
  un-reworked tail, which is small by construction — but it is a caveat, not a
  measured zero.
- **Model-normalized tokens.** Token cost differs by model (~1.5× between the
  fleet's two dominant models, per the #9429 experiment). The window these
  bounds were derived on was ~96 % one model, so the rubric's token ranges
  carry that mix; normalization is #9434's calibration work and #9466's
  landed-size index, not something a raw token sum can do.
- **Queue-adjusted wall-clock.** Queue state lives on `loom.dispatch.*` spans,
  not in `sweep.outcome` records; no join is attempted here. Wall-clock is
  reported with its caveat and never anchors a bound.
- **Cost in currency.** Tokens are raw counts by the schema's own contract;
  pricing belongs to the consumer's pricing table.

## Verification status

| Artifact | Status |
| --- | --- |
| `story-point-queries.sql`, `signoz/story-point-extract.sql` | **Executed live** against the fleet SigNoz on the harness-ops host (read-only access; the view's SELECT body inlined, the CREATE not run) over 2026-09-14 00:00 → 2026-09-30 00:00 UTC: 10 579 `sweep.outcome` rows / 10 578 distinct sweeps, 172 clean landings, 138 token-measured. Full outputs, verbatim, in [`signoz/evidence.md`](signoz/evidence.md) "Story-point calibration, executed live". |
| `clickstack/story-point-extract.sql` | **Not executed live.** Contract-checked in CI (`loom-daemon/tests/story_point_artifacts.rs`), mirrors the live-verified ClickStack conventions of `clickstack/cycle-time-extract.sql`. |
| Contract (`loom-daemon/tests/story_point_artifacts.rs`) | Runs in ordinary CI, no Docker/network/credential: both extraction views expose the same columns in the same order; every attribute key they read survives the gateway's `keep_keys` allowlist (the failure mode that returns zero rows silently); the documented question set is exactly the implemented one; every question binds its window; queries read only `loom_analytics.*`; the clean-landing filter is defined once, in the views. |
| `defaults/docs/story-points.md` (the rubric) | Bounds derived from SP2's live output cross-checked against the D1 tuning-set medians (issue #9430 comment, 2026-09-29); marked provisional, subject to the #9434 calibration loop. |
