# Story-points throughput: the canonical question set

The standing answer to *"how many story points land per day — and is the
pipeline keeping up?"* — defined **once**, here, so that the ClickStack and
SigNoz artifacts answer the same questions with the same definitions instead of
each backend converging on its own private meaning of "points landed" (Issue
#9433, epic #9429; the pattern and the parity discipline are the cycle-time
question set's, #8665).

Nothing below is a saved search you have to re-derive: every question `PT<n>` is
a committed SQL statement of the same number in
[`story-points-throughput-queries.sql`](story-points-throughput-queries.sql),
and that file is **backend-neutral** — it reads one table,
`loom_analytics.ship_points`, which both backends populate from their own logs
table via their own extraction artifact
([`clickstack/story-points-extract.sql`](clickstack/story-points-extract.sql),
[`signoz/story-points-extract.sql`](signoz/story-points-extract.sql)). Parity is
therefore a property of the design, not of two hand-synchronized query sets.

## Definitions (fix these before reading any number)

| Term | Definition |
| --- | --- |
| **ship** | One `sweep.outcome` record: a sweep that reached a terminal state. One row per `(repo, sweep_id)` — the cycle-time set's own `ship` definition, unchanged. |
| **landed** | A ship with `result = 'success'`. **A failed sweep lands nothing**: its estimate, if any, is work attempted, not work delivered, so no sum in this set may include it. The rollup stores failed sweeps anyway — the questions count them beside the landings (the ship-vs-fail separation is a column, not a footnote). |
| **story points** | The numeric value of the issue's single `points:*` label (`1`/`2`/`3`/`5`/`8`/`13`, the vocabulary `defaults/docs/story-points.md` defines), carried on the `sweep.outcome` record as the numeric attribute `loom.story_points` (#9432/#9536). It is the Curator's **a priori** estimate — a forecast, not a measurement of what landed. |
| **forecast points** | `sum(points_landed)`: the raw label values summed over labeled landings. **Labels are ordinal, not a unit** — the experiment measured bucket ratios of roughly 1 : 1.3 : 2.2 : 3.4 : 5.1 : 8.2 (tokens) and 1 : 8.5 : 21 : 47 : 82 : 197 (hand-written lines), so a "13" is *not* thirteen "1"s. The sum is a size-mix-weighted **volume index**, useful exactly because count alone explained R² 0.83 of daily delivered size while adding the label sum raised it to 0.94. Read it beside the landings count, never alone. |
| **measured point values** | Per-bucket values from those measured ratios (`measured_point_tokens`, `measured_point_lines` in the rollup's `ship_points` view). **Provisional**, and a deliberate mirror of `sweep-facts/landed-size.sql`'s `measured_points` (#9466): change both together until #9434's calibration collapses them into one point value per class. A label outside the vocabulary has no measured value — `NULL`, never an extrapolation. The *measured headline* for delivered size is LSI, which lives in the D1 sweep-facts set (SF2, #9466); this ClickHouse-visible set reports forecast points and the measured values beside it. |
| **points landed** | `points_landed`: the estimate **only** when the ship both succeeded and carried one — `NULL` otherwise. This is the one column any question may sum: a failed sweep contributes `NULL` (it landed nothing, not zero), and an unlabeled success contributes `NULL` (a gap, not a zero-point landing). |
| **data gap** | A landing with no `loom.story_points` attribute (`points_present = 0`). Missing is not zero: four situations omit the attribute — never sized (pre-epic, operator-filed, uncurated), one out-of-vocabulary label, stacked labels, or a skipped/failed label read — and telemetry alone cannot split them (the daemon logs the latter three at `warn`; that log is where the split lives). `PT6` counts the gap; read it beside every summed answer, because a rising unlabeled share silently undercounts every points-per-day number. |
| **absent vs. zero** | An unmeasured field is `NULL`, never `0` or `''` — the same contract the cycle-time set and the telemetry schema enforce. No artifact in this set coerces a missing `loom.story_points` to `0`, at extraction or at aggregation. |

## The questions

| ID | Question | Grouping | Reads |
| --- | --- | --- | --- |
| **PT1** | *How many points land per day?* — the headline the epic exists for: landings (issue count), forecast points, both measured point values, median label, and the unlabeled-landing gap, with failed sweeps counted separately. | per day | `points_landed`, `points_present` |
| **PT2** | *Points landed per ISO-week?* — per `toStartOfWeek(…, 1)`, with `points_per_day` over days **observed**, not calendar days, so partial edge weeks are not diluted. | per week | `finished_at`, `points_landed` |
| **PT3** | *Is throughput trending over months?* — per-month landings, forecast points and points/day, with the labeled share. **This is the question the 7-day raw retention makes unanswerable, and the reason the rollup exists.** | per month | `finished_at`, `points_landed` |
| **PT4** | *Which repos land the most sized work?* — per repo: landings, forecast/measured points, median label, points/day over the bound window, and the unlabeled gap. | per repo | `repo`, `points_landed` |
| **PT5** | *What is the size mix?* — landings per label value. The labels are ordinal, so the mix is the signal a single sum hides; an out-of-vocabulary value shows up as its own class, visible rather than folded onto a neighbour. | per size class | `story_points` |
| **PT6** | *Where is the data missing?* — the CT7 discipline: landings without points (the data-gap count), failed sweeps (stored, counted, never summed), unreadable keys, and out-of-vocabulary values. Read this **beside** any summed answer above. | per window | `points_present`, `result` |
| **PT7** | *Is the rollup faithful to the raw logs?* — reconciliation over the window where **both** still exist: raw `sweep.outcome` identities vs. rolled-up ships, and any ship whose stored points or result disagree with the raw record (the CT8 discipline). | per window | rollup vs. raw |

## What this question set cannot answer (and why)

- **Absolute delivered size.** Everything here is anchored on the Curator's
  *a priori* label. The measured landed size (LSI, #9466) is computed from
  hand-written lines, files and model-normalized tokens in the D1 sweep-facts
  set (`sweep-facts/landed-size.sql`, question SF2) — none of which this
  ClickHouse rollup carries. Until #9434's calibration lands, treat
  `forecast_points` as an index and the measured columns as provisional.
- **Why a landing is unlabeled.** Telemetry omits the attribute for both "never
  sized" and "defectively sized"; the daemon's `warn` log names the defective
  cases. PT6 reports the total gap, not the split.
- **Estimate vs. actual.** Comparing the forecast against what really landed is
  #9434's calibration loop, which joins this rollup's estimates to the D1
  facts — deliberately not a PT question.
- **Retroactive history.** The rollup is populated going forward (and by
  backfill over whatever raw window still exists). It cannot recover ships
  whose raw rows already expired; the first useful PT3 window starts the day
  the rollup is created.

## Retention decision: the cycle-time rollup's, inherited

The constraint is the cycle-time set's, unchanged: ClickStack's exporter sets
`HYPERDX_OTEL_EXPORTER_TABLES_TTL` (default `168h`,
[`clickstack/compose.yaml`](clickstack/compose.yaml)), so raw `sweep.outcome`
rows die in seven days and PT3 is unanswerable; SigNoz's retention is
application-managed with no per-table knob either. The full argument for
persisting a rollup rather than raising the raw TTL — a TTL edit does not
migrate existing tables, one knob would cover the trace-dominated volume, and a
one-row-per-ship table is small enough to be boring — is in
[`cycle-time-questions.md`](cycle-time-questions.md) §"Retention decision" and
is not restated here. `loom_analytics.ship_story_points` therefore carries the
same shape: one row per ship, `ReplacingMergeTree` keyed on
`(repo, sweep_id, finished_at)`, **TTL 400 days** set in the DDL.

The drift objection is answered the same three ways: the rollup is written by
one committed statement per backend, re-running that statement over an
already-ingested window is **idempotent** (the target is a
`ReplacingMergeTree`, every analysis query reads it through the `ship_points`
view which applies `FINAL`, so a duplicate delivery — the OTLP pipeline is
at-least-once — and a re-run both collapse to one row), and `PT7`
**reconciles** the rollup against the raw table over the window where both
still exist, reporting any identity, points or result mismatch. Drift is
detectable with a committed query rather than assumed away.

## Ops note: where to read the number for capacity tuning

The capacity question — *"the pipeline lands N points/day — do we need another
Judge?"* — is answered by **PT1** over a trailing window (landings/day and
points/day together: count is the volume signal, points the size-mix), and by
**PT3** for the trend, on either backend:

```console
clickhouse-client --param_since='2026-09-15 00:00:00' \
                  --param_until='2026-09-22 00:00:00' \
                  --queries-file story-points-throughput-queries.sql
```

Read **PT6 beside it before acting**: a high unlabeled share means N
**undercounts** (curation is not sizing issues — fix coverage, don't buy
capacity), and `failed_sweeps` retried later will land their points in a later
window. Only a sustained rise in points/day with a flat labeled share and flat
cycle times (CT6) is a capacity signal. The per-repo split (PT4) localizes it:
one repo driving the rise needs its own capacity, not the fleet's.

## Verification status

| Artifact | Status |
| --- | --- |
| `story-points-throughput-rollup.sql`, `clickstack/story-points-extract.sql`, `signoz/story-points-extract.sql` | **Contract-checked in CI** (`loom-daemon/tests/story_points_throughput_artifacts.rs`): column lists, question set, window binding, ship-vs-fail separation, data-gap counting, idempotence seams, and gateway survival of `loom.story_points`. **Not executed live** on either backend — the cycle-time ClickStack half is the live-verified precedent for these conventions (`loom-daemon/tests/cycle_time_clickhouse.rs`), and a live points-window execution is follow-up work under #8529's comparison. |
| Measured point values (`measured_point_*`) | **Provisional** — the experiment's bucket ratios, mirrored from `sweep-facts/landed-size.sql`; recalibrated by #9434, never by editing one file alone. |
| Saved dashboards/panels (HyperDX, SigNoz UI) | **Not included**, for the cycle-time set's reason: panel exports embed installation-specific source/team IDs this repo deliberately does not commit. The queries are the exported-as-code artifact; turning each `PT<n>` into a panel is a UI step on a live host. |
