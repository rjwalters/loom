# Sweep facts & issue effort: the canonical question set

The standing answer to *"what did it cost to land this issue — all-in, and
clean?"* — defined **once**, here, so the D1 rollup, the ClickStack extraction
and the SigNoz extraction answer the same questions with the same definitions
instead of each store converging on its own private meaning of "a sweep's
effort" (Issues #9446, #9466, #9433; the pattern and the parity discipline are
the cycle-time question set's, #8665).

Like the cycle-time set, every question `SF<n>` is a committed SQL statement of
the same number in [`sweep-facts-queries.sql`](sweep-facts-queries.sql). The
canonical store is different, and the difference is the point: D1
(`loom-fleet-telemetry`) is the only place per-issue sweep tokens older than a
week still exist, it is at its 500,000-row cap and evicting now, and SigNoz raw
logs die in ~7 days. [`sweep-facts-rollup.sql`](sweep-facts-rollup.sql)
therefore copies one row per terminal sweep out of the raw `records` table into
`sweep_facts` **in D1 itself** — the raw store and the durable store share a
database, so the rollup outlives the 90-day/500k-row eviction without a second
deployment. The two ClickHouse extraction views
([`clickstack/sweep-facts-extract-clickstack.sql`](sweep-facts-extract-clickstack.sql),
[`signoz/sweep-facts-extract-signoz.sql`](sweep-facts-extract-signoz.sql)
normalize the same `sweep.outcome` payload into the same fact shape on the
observability backends, for the windows their retention still holds.

## Definitions (fix these before reading any number)

| Term | Definition |
| --- | --- |
| **sweep fact** | One row of `sweep_facts`: one terminal sweep, keyed `(repo, issue, sweep_id)`. Sourced from `records` rows with `kind = 'sweep.outcome'`; payload fields are read with `json_extract(payload, '$.…')` (D1's SQLite JSON1 dialect). Rows whose `repo`, `issue` or `sweep_id` the envelope left absent are out of the keyed grain — #9442 makes absent-`repo` a countable *unattributed* bucket, and SF7 counts it, so the exclusion is visible rather than silent. |
| **disposition** | `sweep.outcome.disposition` (#9441): what the sweep actually did — `landed`, `noop_already_done`, `curator_closed`, `curator_rescoped`, `env_failure`, `substantive_failure`, `cancelled`, `unknown`. `result = 'success'` deliberately does **not** answer "did this sweep land work?"; `disposition = 'landed'` does. Records written before #9441 carry no disposition (absent, never `'unknown'`). |
| **tokens_status** | `sweep.outcome.tokens_status` (#9440): `measured` \| `not_spawned` (a true zero — nothing spawned, nothing consumed) \| `unattributable` (spawned; usage unreadable) \| `suspect` (#9454: the plausibility guard refused to publish the raw number as measured). Absent on pre-#9440 records. |
| **attempt lineage** | `attempt_index`, `previous_sweep_id`, `trigger`, and the in-sweep `rework_events` list (#9444): why this sweep was dispatched and what rework happened inside it. |
| **lifecycle effort** | Per issue: **every** attempt — count of sweeps, Σ `total_duration_sec`, Σ `tokens_in`/`tokens_out`, plus a completeness flag (1 when every attempt's `tokens_status` ∈ {`measured`, `suspect`, `not_spawned`} — i.e. no attempt's tokens are merely unknown). |
| **clean effort** | The landing sweep only, through the first judge: curator + builder + first `judge` phase wall seconds (walked off `phase_durations`), and the landing sweep's own tokens. Tokens are per sweep, not per phase (#9443), so the token half is the whole landing sweep until per-phase attribution lands; the wall genuinely stops at the first `judge` entry. |
| **rework, substantive** | Rework caused by the work itself: a judge `changes-requested`, a Doctor cycle, `trigger = 'retry_after_substantive_failure'` (or `doctor_after_changes_requested`), or a `rework_events` entry classified `substantive` (#9444). |
| **rework, environmental** | Rework caused by the surroundings: `trigger` ∈ {`retry_after_env_failure`, `rebase_main_moved`, `merge_conflict`, `stale_base_rejudge`, `ci_failure_fix`}, or a `rework_events` entry classified `environmental` (#9444). |
| **effort split (seconds)** | #9507. Per issue, the lifecycle wall partitioned into `clean_sec` / `substantive_rework_sec` / `environmental_rework_sec` / `unattributed_sec` (`issue_effort`). Each attempt's **measured** in-sweep rework (`rework_substantive_sec` / `rework_environmental_sec`, Σ of the `rework_events[]` entries' `duration_sec` per class) is charged to its class; the attempt's residual (total − measured rework, clamped ≥ 0) is charged to its `trigger`'s bucket — `first` → clean, the substantive/environmental triggers above → their rework bucket, and `operator_redispatch`, `unknown` and a triggerless pre-#9444 record → **unattributed, never a rework bucket**. The four buckets sum to `lifecycle_wall_sec + overaccounted_sec`; `overaccounted_sec` (measured rework exceeding its attempt's wall — forge vs. daemon clock) is 0 on a sane timeline and reported, not absorbed. The arithmetic and the trigger → bucket table are IE1's in [`../issue-effort-queries.sql`](../issue-effort-queries.sql) (which reads raw `records`); `sweep_facts_artifacts.rs` fails if the two tables disagree. `clean_sec` is **not** `clean_wall_sec` (the landing sweep through its first judge). |
| **coverage** | `attributed_attempts` / `attributed_wall_pct` on `issue_effort`, and SF1's window coverage row: the share of attempts and of partitioned seconds the trigger classification can attribute. Read it **before** the split. On history before the #9506/#9514 writers reached the fleet it is ~0% — the rollout boundary, not a defect; no backfill is possible. |
| **absent vs. zero** | An unmeasured field is `NULL`, never `0` and never `''` — the telemetry schema's own contract (.loom/docs/telemetry-schema.md, "unknown != zero"): an absent key is not an observation. Every rollup column read from the payload stays `NULL` when the key is absent; `rework_substantive`/`rework_environmental` — and their `_sec`/`_open` siblings (#9507) — are `NULL` when the record carried no `rework_events` at all and `0` only when the list was read and held none. A rework event with no `duration_sec` is **open** (its clearing forge event was never observed): it adds 0 to `rework_*_sec` and 1 to `rework_*_open`, so "unmeasured" stays visible and is never smoothed into a measured 0. SF1's completeness flag exists so an aggregate built over absent values is marked, not silently averaged. |
| **landed size (LSI)** | #9466. Per landing: the mean of the available standardized components — z-score of `log1p(hw_lines)`, of `log1p(hw_files)`, and of model-normalized `log1p(tokens)` (see [`landed-size.sql`](landed-size.sql)). `LSI = exp(landed_size)`, so **1.0 is a median landing** and sums/ratios of LSI are defined. `hw_lines` = `hw_lines_added + hw_lines_deleted` (hand-written churn); tokens = `tokens_in + tokens_out` normalized per model. The tokens component is optional until #9440/#9454 land; when it is used the row is flagged `landed_size_tokens_used = 1`. |
| **size class** | The Fibonacci label of a landing, via fixed cuts on LSI (1, 2, 3, 5, 8, 13 as upper bounds; the top class is 21). Labels are **not** additive: the experiment measured bucket ratios of 1 : 1.3 : 2.2 : 3.4 : 5.1 : 8.2 for tokens and 1 : 8.5 : 21 : 47 : 82 : 197 for lines (#9466) — a "13" landing is not thirteen "1" landings on any component. Sums over landings must use LSI or the measured point values, **never** the raw labels. |
| **measured point value** | The per-class point table those bucket ratios define, held in exactly one place: the `measured_point_values` view in [`landed-size.sql`](landed-size.sql). SF2 reaches it through `issue_landed_size` for a landing's **measured** class; SF8 joins it directly on the **assigned** class. Two copies of the ratios would be two meanings of "a point", so there is one view and both sides read it. Class `21` is deliberately absent — no measured landing yet, so a landing there joins to `NULL` rather than to an extrapolation. Still **provisional** until #9434 collapses the two components into one point value per class. |
| **story points (forecast)** | `sweep.outcome.story_points` (#9432): the Curator's *a priori* size for the issue — the numeric value of its single `points:*` label, one of `1`/`2`/`3`/`5`/`8`/`13`. The only **forecast** column on `sweep_facts`; every other column is a measurement. `NULL`, never `0`, in all four situations telemetry-schema.md §`story_points` enumerates (no label, an out-of-vocabulary label, more than one label, a failed or skipped label read), so "unsized" and "sized at nothing" can never be confused — SF8 counts the `NULL`s as an explicit data gap. **Ordinal, not a unit**: never `sum()` it as a size. |

## The questions

| ID | Question | Grouping | Reads |
| --- | --- | --- | --- |
| **SF1** | *Per issue, what did the whole lifecycle cost versus the clean landing — and can we trust the lifecycle sum?* Attempts, Σ wall, Σ tokens (lifecycle) beside the landing sweep's clean wall/tokens, plus `token_completeness`; and (#9507) the **seconds** split clean / substantive / environmental / unattributed with its open-event counts, followed by a one-row **window coverage** statement to read first. | per issue landing; per window (coverage) | `issue_effort` view |
| **SF2** | *How much work is landing per day?* — Σ LSI over landings per day. The throughput KPI (#9466): size-weighted, so ten trivial landings do not outproduce one substantial one. The **measured** half of the pair it forms with SF8. | per day | `issue_landed_size` |
| **SF3** | *Which hosts are attributing tokens a sweep could not have consumed?* — token-bearing outcomes with `tokens_in / total_duration_sec > 100000`, count and share per host per day (#9454's committed monitor). | per host, per day | `tokens_in`, `total_duration_sec`, `tokens_status` |
| **SF4** | *What do sweeps actually do?* — disposition distribution over a window, with the absent (pre-#9441) bucket kept separate from `unknown`, and the unknown share checked against the < 5% bar (#9441's acceptance). | per window | `disposition` |
| **SF5** | *How many attempts does an issue take, and why?* — attempts-per-issue split into first-dispatch, substantive, environmental and unknown, per #9444's classification; rework events folded in from `issue_effort`. | per issue | `trigger`, `issue_effort` |
| **SF6** | *Are tokens measured, or just missing?* — share of outcomes by `tokens_status`, the ≥ 95% coverage check (#9440). Read beside SF1: an incomplete lifecycle sum is these rows, not a small number. | per window | `tokens_status` |
| **SF7** | *Is the rollup faithful to the raw records?* — reconciliation over a window: raw in-grain `records` vs. `sweep_facts` rows, the out-of-grain population counted separately, and **unexplained drops = 0** in every window (#9446's acceptance criterion, the SF analogue of CT8). | per window | `sweep_facts` vs. `records` |
| **SF8** | *How many points land per day, and was the day sized at all?* — per day **and** per ISO week over landings only: `landings`, the measured point value of each landing's **assigned** class summed (two columns, tokens and lines), the `points_missing` data gap, and SF2's `lsi_landed` beside them. The **forecast** half of the pair it forms with SF2 (#9433). Raw Fibonacci labels are reported only as `labels_summed_ordinal_do_not_use_as_size`. | per day, per ISO week | `story_points`, `measured_point_values`, `issue_landed_size` |

## Ops note: reading throughput for capacity tuning

*"The pipeline lands N points/day — do we need another Judge?"* is answered by
**SF2 and SF8 together**, never by either alone. Run
`sweep-facts-queries.sql` with the window you care about in `sf_window` and read,
in this order:

1. **SF8's `iso_week` rows first, for the trend; the `day` rows for the detail.**
   A single day is small-N — one 13-class landing moves it — so a capacity
   decision is a multi-week read, and SF8 emits both grains from one definition
   so those two readings cannot drift apart. `landings` is the first number to
   look at: over a measured 34-day window issue count alone explains R² 0.83 of
   daily delivered size and summed points raise that only to 0.94, so **most
   day-to-day variation is volume, not size mix**.
2. **SF8's `points_missing` before any points column.** It is the coverage
   denominator: if it is a large share of `landings`, the points columns describe
   the sized minority and nothing else, and the action is to fix curation
   coverage, not to add a Judge. `points_sized + points_missing = landings`
   always, which is how you check.
3. **SF2's `lsi_landed` as the headline** (fitted since `v1-2026-10-02`, #9934 —
   read `params_version` beside it). SF2 is what actually landed; SF8's
   measured-point columns are what the Curator *predicted* would land. The two
   read side by side are the estimate-vs-actual signal #9434's calibration loop
   consumes — a persistent gap is a rubric problem, not a capacity problem.
4. **Add capacity when volume is flat while the queue grows**, not when points
   dip: SF8's `landings` flat against a growing `loom:issue` backlog is a
   throughput ceiling; a dip in points at steady `landings` is a size-mix
   change. Pair either with SF5 (attempts per issue) before concluding the
   ceiling is Judge capacity rather than rework.

Do not read `labels_summed_ordinal_do_not_use_as_size` as a size under any
circumstance — it exists to make a mislabelled window visible, and its name is
the whole warning.

## What this question set cannot answer (and why)

- **Per-phase token attribution inside one sweep.** Tokens ride the sweep, not
  the phase (#9443). "Clean" tokens are therefore the landing sweep's whole
  usage, and every reading of SF1's clean-token columns must say so until
  #9443 lands per-phase counters.
- **Did a closed issue later gain a queue label?** Not answerable from this
  store, and deliberately not an SF question: label movements are forge state,
  not telemetry, and no exporter carries them. Forge-side questions need the
  forge's own event stream.
- **Retroactive history before the backfill.** The rollup is populated going
  forward and by one backfill over whatever D1 still holds (≥ 2026-08-15,
  #9446). Sweeps evicted before the backfill ran are gone; no query here can
  recover them, and SF7 only reconciles windows where the raw rows still
  exist.
- **Spend.** Tokens are raw counts, deliberately not cost-weighted
  (telemetry-schema.md); pricing belongs to the consumer's pricing table, not
  to these facts.

## Idempotence and reconciliation

A backfill may be re-run over an already-ingested window, and the ingest
pipeline is at-least-once. `sweep_facts` is keyed `(repo, issue, sweep_id)` and
written by a single `INSERT OR REPLACE` — the D1 analogue of the cycle-time
rollup's `ReplacingMergeTree` (`cycle-time-questions.md` §Retention): a re-run
overwrites the row instead of duplicating it, and a corrected extraction wins
on re-run. `records` itself keeps its own #5084 guarantee (partial UNIQUE index
plus `INSERT OR IGNORE`), so the raw side the reconciliation reads is already
deduplicated by construction. SF7 closes the loop the same way CT8 does: any
window where raw in-grain rows and fact rows disagree is drift, and only the
counted out-of-grain population may explain a difference.

**Adding a column does not change either property**, which is why `story_points`
(#9433) needed no rollup of its own and no second reconciliation query: the key
and the grain are untouched, so a re-run still overwrites the row rather than
duplicating it, a corrected `story_points` wins on re-run exactly as a corrected
token count does, and SF7 already reconciles the whole row. A new *rollup* — a
second pipeline keyed differently — would have needed its own CT8 analogue; an
extra column on this one does not.

**But an added column has to reach the installed table (#9507).** The rollup's
`CREATE TABLE IF NOT EXISTS` is a no-op on a database that ran an earlier
version of it, and `sweep_facts` is durable — it holds facts whose `records`
rows D1 has already evicted — so it is upgraded in place, never dropped and
rebuilt. SQLite has no `ADD COLUMN IF NOT EXISTS`, so
[`sweep-facts-migrate.sql`](sweep-facts-migrate.sql) is one read-only `SELECT`
that prints exactly the `ALTER TABLE sweep_facts ADD COLUMN …` statements still
missing (none on an up-to-date or not-yet-created table). That makes the upgrade
re-runnable, even from a half-applied state. Run it before the rollup on any
existing database (the exact `wrangler` + `jq` recipe is in the file's header),
apply what it prints, then run `sweep-facts-rollup.sql` and `issue-effort.sql`.
Historical rows keep every stored value and read `NULL` in the new columns (not
measured, never `0`). `issue-effort.sql` drops and recreates its view, so a
re-install always replaces an older definition. Adding a rollup column means
adding its row to the migration's list too; a CI test fails if the two disagree.

## Verification status

| Artifact | Status |
| --- | --- |
| `sweep-facts-rollup.sql`, `issue-effort.sql`, `landed-size.sql`, `sweep-facts-queries.sql` | **Contract-checked** in CI (`loom-daemon/tests/sweep_facts_artifacts.rs`): column lists, question set, window binding and the reconciliation seam are asserted statically. `landed-size.sql` is additionally **fixture-executed** in CI (`loom-daemon/tests/landed_size_sqlite.rs`, #9934): the view runs verbatim on bundled SQLite (with D1's `ln`/`exp` registered by the harness) against a known `sweep_facts` fixture. D1 execution itself is operator-side — the one-time backfill run before more history is evicted is tracked in the operator-side D1 backfill (see #9446's backfill note); the landed-size fit has since shipped (`v1-2026-10-02`, #9934), so the first live rollup run pastes the fitted constants in by re-running this file as committed. |
| `landed-size.sql` parameters | **Fitted: `v1-2026-10-02` (#9934).** The component means/SDs and per-model token factors were fit by `fit-landed-size.mjs` (beside the SQL) on the baseline window [2026-09-29, 2026-10-02) — the #9466 component emitters' first four days: 379 landings, 111 with hw_* components, 374 with a clean token reading. The window is young and the mixture uneven (111 three-component landings against ~263 token-only ones), so the refit is part of the artifact: rerun the script over a longer window once hw_* coverage matures, paste its `params` block, bump `params_version`. The tokens component scores only clean verdicts (`measured`, or the absent verdict of legacy rows — #9440/#9454). The measured point values are the experiment's bucket ratios (#9466), marked provisional until #9434's calibration collapses them into one point value per class. |
| `sweep_facts.story_points` + SF8 | **Contract-checked** in CI alongside SF1–SF7, and **exercised locally** against SQLite over synthetic `records` (#9433): the rollup ingests the column, SF8 excludes non-landed sweeps, reports the `NULL` population as `points_missing`, sums only measured point values, and a triple rollup re-run leaves 7 fact rows with SF7 `unexplained_drops = 0` while a corrected payload updates the column in place. `story_points` is `NULL` in bulk on records written before #9432's emitter landed — that is the absent-vs-zero contract holding, not a gap in the rollup. Not yet executed against live D1. |
| `issue-effort.sql` seconds partition (#9507) | **Fixture-executed** in CI (`loom-daemon/tests/issue_effort_sqlite.rs`): the committed `sweep-facts-rollup.sql` and `issue-effort.sql` run verbatim on bundled SQLite over synthetic `records`, asserting the partition identity, the clamp + `overaccounted_sec`, open events, the unattributed triggers, absent-vs-`[]`, and SF1's coverage row (including an empty window). The **upgrade** from the previous installed table and view is fixture-executed too (`loom-daemon/tests/sweep_facts_upgrade_sqlite.rs`). It starts from the merge-base definitions with retained rows, including a fact whose raw records were evicted, and runs `sweep-facts-migrate.sql`, the rollup and the view. It asserts that no stored value changes, that the new columns are `NULL` on unrecoverable history, that the replaced view answers as a fresh install does, and that a re-run (or resuming a half-applied upgrade) is a no-op. The `wrangler --json` / `jq` recipe and D1's `pragma_table_info` support have not yet been exercised against live D1. The ClickHouse extractions' new `rework_*_sec`/`rework_*_open` columns are only **parity-checked by name** — no in-repo test executes ClickHouse SQL. |
| `sweep-facts-extract-clickstack.sql` | Mirrors the live-verified `clickstack/cycle-time-extract.sql` conventions (`default.otel_logs`, `Body` filter, attribute-map reads) but is **not yet executed live** against a sweep-facts window. |
| `sweep-facts-extract-signoz.sql` | **Not executed live.** Contract-checked like its cycle-time counterpart; reads both `attributes_string` and `attributes_number` so it does not depend on an unverified assumption about which map the pinned ingester files a numeric attribute in (#8529 follow-up). |
