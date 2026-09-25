# Cycle-time analytics: the canonical question set

The standing answer to *"what took long to ship, and where did the time go?"* —
defined **once**, here, so that the ClickStack and SigNoz artifacts answer the
same questions with the same definitions instead of each backend converging on
its own private meaning of "slow" (Issue #8665, parity requirement #8529).

Nothing below is a saved search you have to re-derive: every question `CT<n>` is
a committed SQL statement of the same number in
[`cycle-time-rollup.sql`](cycle-time-rollup.sql), and that file is
**backend-neutral** — it reads one table, `loom_analytics.ship_cycle_time`,
which both backends populate from their own logs table via their own extraction
artifact ([`clickstack/cycle-time-extract.sql`](clickstack/cycle-time-extract.sql),
[`signoz/cycle-time-extract.sql`](signoz/cycle-time-extract.sql)). Parity is
therefore a property of the design, not of two hand-synchronized query sets.

## Definitions (fix these before reading any number)

| Term | Definition |
| --- | --- |
| **ship** | One `sweep.outcome` record: a sweep that reached a terminal state. One row per `(repo, sweep_id)`. |
| **shipped successfully** | A ship with `result = 'success'`. `CT1`/`CT6` report these separately from failures; a failed sweep's duration is a different quantity, not a slow ship. |
| **ship cycle time** | `loom.total_duration_sec` — wall-clock seconds from sweep start to terminal state. **Not** time-since-issue-filed (see "What this question set cannot answer"). |
| **phase** | An entry of `loom.phase_durations`: `{phase, duration_sec}`. Phase names are whatever the lifecycle emitted (`curator`, `builder`, `judge`, `doctor`, `merge`, …); nothing here hardcodes the list. |
| **dominant phase** | The phase name with the largest **summed** duration in that ship. Summed, because a repair loop emits `judge` twice and the two attempts are one bottleneck, not two small ones. |
| **absent vs. zero** | An unmeasured field is `NULL`, never `0` or `''`. `phase_durations_present = 0` means the outcome carried no phase breakdown at all — distinct from a measured empty one. `CT7` exists to keep that distinction visible. |

## The questions

| ID | Question | Grouping | Reads |
| --- | --- | --- | --- |
| **CT1** | *Top N slowest ships in a window, and which phase dominated each.* | per ship | `total_duration_sec`, dominant phase |
| **CT2** | *Where does the time actually go?* — per-phase duration distribution (p50/p90/p95/max) and share of total. | per phase | phase array |
| **CT3** | *Which repos ship slowly?* — ship count, success rate, p50/p90/p95 cycle time. | per repo | `total_duration_sec`, `result` |
| **CT4** | *Which execution configuration is the bottleneck?* — cycle time per runtime × provider × model × effort. | per config | `runtime`, `provider`, `model`, `effort` |
| **CT5** | *What does repair cost?* — ships that engaged Doctor vs. those that did not: count, share, median and p90 cycle time, and `judge`/`doctor` seconds. | repair vs. clean | `doctor_cycles`, phase array |
| **CT6** | *Is the fleet getting slower?* — per-ISO-week ship count and p50/p90 cycle time over a multi-month window. **This is the question the 7-day raw retention makes unanswerable**, and the reason the rollup exists. | per week | `finished_at`, `total_duration_sec` |
| **CT7** | *Where is the data missing?* — ships with no phase breakdown, no runtime, no model, no PR number. Read this **beside** any grouped answer: an empty cell above is missing data, never a measured zero. | per window | presence flags |
| **CT8** | *Is the rollup faithful to the raw logs?* — reconciliation over the window where **both** still exist: raw `sweep.outcome` identities vs. rolled-up ships, and any ship whose stored total disagrees with the raw record. | per window | rollup vs. raw |

`CT4` deliberately groups by the *execution configuration* of the whole sweep,
not by role: `sweep.outcome` carries one model/runtime per sweep, so a per-role
attribution would be a fabrication. Per-role-attempt timing is a **trace**
question (`loom.role_attempt` spans, #8525) and is answered by
`signoz/fixture-queries.sql` query 3, not here.

## What this question set cannot answer (and why)

- **"How long from issue *filed* → curated → built → reviewed → merged?"** The
  telemetry stream starts at `sweep.started`; nothing in it carries the forge's
  `created_at` for the issue, nor the times a label moved. Answering the
  filed-to-merged leg needs forge timestamps joined in, which no exporter emits
  today. Anything computed here is **sweep** cycle time, not issue lead time.
- **Per-role cost attribution inside one sweep.** See the `CT4` note above.
- **Retroactive history.** The rollup is populated going forward (and by
  backfill over whatever raw window still exists). It cannot recover ships whose
  raw rows already expired — the first useful `CT6` window starts the day the
  rollup is created.

## Retention decision: persist a rollup, do **not** raise the raw TTL

The constraint: ClickStack's exporter sets `HYPERDX_OTEL_EXPORTER_TABLES_TTL`
(default `168h`, [`clickstack/compose.yaml`](clickstack/compose.yaml)), so raw
`sweep.outcome` rows die in seven days and `CT6` is unanswerable. Two ways out
were available; this is why the rollup won.

1. **Raising the raw TTL does not do what it looks like it does.** The TTL is
   applied *when a table is created*; changing the env value does not migrate
   existing tables ([`clickstack/README.md`](clickstack/README.md) §Retention,
   confirmed against `system.tables.create_table_query` in
   [`clickstack/evidence.md`](clickstack/evidence.md)). On an existing volume,
   raising it silently changes nothing — the worst possible failure mode for a
   retention control.
2. **It is global, and traces dominate the volume.** One knob covers logs,
   traces and every metric table. Keeping 400 days of trace spans to answer a
   question that needs one row per sweep is a storage decision nobody asked for.
3. **The rollup is small enough to be boring.** One row per ship, a few hundred
   bytes. A fleet shipping 200 sweeps a day for 400 days is on the order of
   tens of megabytes — so the rollup's own TTL can be generous (400 days, set in
   the DDL) without a capacity conversation.
4. **It is the only option with backend parity.** SigNoz's retention is
   application-managed (set through its API/UI, with auxiliary TTLs patched by
   [`signoz/retention.sql`](signoz/retention.sql)); there is no per-table knob
   equivalent to ClickStack's. The same `loom_analytics.ship_cycle_time` schema
   can be created on either backend's ClickHouse, so one rollup definition
   serves both.

**The drift objection, answered.** A derived table can disagree with its source
and nothing notices. Three properties keep that from being true here:

- the rollup is written by **one committed statement per backend**, not by hand;
- re-running that statement over an already-ingested window is **idempotent** —
  the target is a `ReplacingMergeTree` keyed on `(repo, sweep_id, finished_at)`
  and every analysis query reads it `FINAL`, so a duplicate delivery (the OTLP
  pipeline is at-least-once) and a re-run both collapse to one row;
- `CT8` **reconciles** the rollup against the raw table over the window where
  both still exist, and reports any identity or duration mismatch. Drift is
  therefore detectable with a committed query rather than assumed away.

## Verification status

| Artifact | Status |
| --- | --- |
| `cycle-time-rollup.sql`, `clickstack/cycle-time-extract.sql` | **Executed live** against a pinned ClickHouse, on rows written by the real OpenTelemetry ClickHouse exporter from real `loom-daemon telemetry-export` output — `loom-daemon/tests/cycle_time_clickhouse.rs`, run in CI with `--ignored`. The TTL-survival property (`CT6` after the raw rows are gone) is asserted there by deleting the raw rows. |
| `signoz/cycle-time-extract.sql` | **Not executed live.** The query vocabulary is contract-checked in CI (`loom-daemon/tests/cycle_time_artifacts.rs`), and the extraction reads both `attributes_string` and `attributes_number` so it does not depend on an unverified assumption about which map SigNoz files a numeric attribute in. Live execution needs the pinned SigNoz trial host and is tracked as follow-up work under #8529's comparison. |
| Saved dashboards/panels (HyperDX, SigNoz UI) | **Not included.** Both backends' panel exports embed installation-specific source/team IDs that this repo deliberately does not commit ([`clickstack/README.md`](clickstack/README.md) §Sources). The queries above are the exported-as-code artifact; turning each `CT<n>` into a panel is a UI step on a live trial host. |
