# Sweep facts & issue effort: the canonical question set

The standing answer to *"what did it cost to land this issue — all-in, and
clean?"* — defined **once**, here, so the D1 rollup, the ClickStack extraction
and the SigNoz extraction answer the same questions with the same definitions
instead of each store converging on its own private meaning of "a sweep's
effort" (Issues #9446, #9466; the pattern and the parity discipline are the
cycle-time question set's, #8665).

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
| **absent vs. zero** | An unmeasured field is `NULL`, never `0` and never `''` — the telemetry schema's own contract (.loom/docs/telemetry-schema.md, "unknown != zero"): an absent key is not an observation. Every rollup column read from the payload stays `NULL` when the key is absent; `rework_substantive`/`rework_environmental` are `NULL` when the record carried no `rework_events` at all and `0` only when the list was read and held none. SF1's completeness flag exists so an aggregate built over absent values is marked, not silently averaged. |
| **landed size (LSI)** | #9466. Per landing: the mean of the available standardized components — z-score of `log1p(hw_lines)`, of `log1p(hw_files)`, and of model-normalized `log1p(tokens)` (see [`landed-size.sql`](landed-size.sql)). `LSI = exp(landed_size)`, so **1.0 is a median landing** and sums/ratios of LSI are defined. `hw_lines` = `hw_lines_added + hw_lines_deleted` (hand-written churn); tokens = `tokens_in + tokens_out` normalized per model. The tokens component is optional until #9440/#9454 land; when it is used the row is flagged `landed_size_tokens_used = 1`. |
| **size class** | The Fibonacci label of a landing, via fixed cuts on LSI (1, 2, 3, 5, 8, 13 as upper bounds; the top class is 21). Labels are **not** additive: the experiment measured bucket ratios of 1 : 1.3 : 2.2 : 3.4 : 5.1 : 8.2 for tokens and 1 : 8.5 : 21 : 47 : 82 : 197 for lines (#9466) — a "13" landing is not thirteen "1" landings on any component. Sums over landings must use LSI or the measured point values, **never** the raw labels. |

## The questions

| ID | Question | Grouping | Reads |
| --- | --- | --- | --- |
| **SF1** | *Per issue, what did the whole lifecycle cost versus the clean landing — and can we trust the lifecycle sum?* Attempts, Σ wall, Σ tokens (lifecycle) beside the landing sweep's clean wall/tokens, plus `token_completeness`. | per issue landing | `issue_effort` view |
| **SF2** | *How much work is landing per day?* — Σ LSI over landings per day. The throughput KPI (#9466, #9433): size-weighted, so ten trivial landings do not outproduce one substantial one. | per day | `issue_landed_size` |
| **SF3** | *Which hosts are attributing tokens a sweep could not have consumed?* — token-bearing outcomes with `tokens_in / total_duration_sec > 100000`, count and share per host per day (#9454's committed monitor). | per host, per day | `tokens_in`, `total_duration_sec`, `tokens_status` |
| **SF4** | *What do sweeps actually do?* — disposition distribution over a window, with the absent (pre-#9441) bucket kept separate from `unknown`, and the unknown share checked against the < 5% bar (#9441's acceptance). | per window | `disposition` |
| **SF5** | *How many attempts does an issue take, and why?* — attempts-per-issue split into first-dispatch, substantive, environmental and unknown, per #9444's classification; rework events folded in from `issue_effort`. | per issue | `trigger`, `issue_effort` |
| **SF6** | *Are tokens measured, or just missing?* — share of outcomes by `tokens_status`, the ≥ 95% coverage check (#9440). Read beside SF1: an incomplete lifecycle sum is these rows, not a small number. | per window | `tokens_status` |
| **SF7** | *Is the rollup faithful to the raw records?* — reconciliation over a window: raw in-grain `records` vs. `sweep_facts` rows, the out-of-grain population counted separately, and **unexplained drops = 0** in every window (#9446's acceptance criterion, the SF analogue of CT8). | per window | `sweep_facts` vs. `records` |

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

## Verification status

| Artifact | Status |
| --- | --- |
| `sweep-facts-rollup.sql`, `issue-effort.sql`, `landed-size.sql`, `sweep-facts-queries.sql` | **Contract-checked** in CI (`loom-daemon/tests/sweep_facts_artifacts.rs`): column lists, question set, window binding and the reconciliation seam are asserted statically. D1 execution is operator-side — the one-time backfill run before more history is evicted is tracked in the operator-side D1 backfill (see #9446's backfill note), which also fits `landed-size.sql`'s parameters and bumps `params_version`. |
| `landed-size.sql` parameters | **`v0-unfitted` by design.** The component means/SDs and per-model token factors are `NULL` until fitted on the documented baseline window during the backfill run (the operator-side D1 backfill (see #9446's backfill note)); an unfitted standardization reports `NULL`, never a raw value dressed up as a z-score. The measured point values are the experiment's bucket ratios (#9466), marked provisional until #9434's calibration collapses them into one point value per class. |
| `sweep-facts-extract-clickstack.sql` | Mirrors the live-verified `clickstack/cycle-time-extract.sql` conventions (`default.otel_logs`, `Body` filter, attribute-map reads) but is **not yet executed live** against a sweep-facts window. |
| `sweep-facts-extract-signoz.sql` | **Not executed live.** Contract-checked like its cycle-time counterpart; reads both `attributes_string` and `attributes_number` so it does not depend on an unverified assumption about which map the pinned ingester files a numeric attribute in (#8529 follow-up). |
