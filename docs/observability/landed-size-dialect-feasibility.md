# Landed-size (LSI) scoring in a SigNoz/ClickHouse query: feasibility result

Issue #12548 (follow-up to #9934; consumer tracker
[loom-ui #1022](https://github.com/2AMLogic/loom-ui/issues/1022)).

## Result

**Capability-blocked, with a measured contract.** This work did not show that
the deployed SigNoz query surface can compute the score, and it did not show
that it cannot. What it did produce:

1. The exact scoring contract, pinned, with the places where the two existing
   implementations (upstream SQL, loom-ui client) already disagree.
2. An executable parity harness for those two implementations, with a synthetic
   corpus (`defaults/observability/sweep-facts/landed-size-parity/`).
3. A precise statement of why no candidate dialect was run, and what would
   unblock it.

No candidate-dialect parity is claimed. The current client path in loom-ui is
unchanged and remains the only working scorer outside D1.

## Pinned inputs

| Input | Pin |
|---|---|
| `defaults/observability/sweep-facts/landed-size.sql` | this repo, last changed at `25956aab7`, read at `1f7552d89` (params `v1-2026-10-02`) |
| `defaults/observability/sweep-facts/fit-landed-size.mjs` | same commit |
| loom-ui `src/signoz/queries/kpiLandings.ts`, `web/src/pages/kpis/landedSizeParams.ts`, `landingScore.ts`, `landingOutcome.ts`, `deliveredModel.ts` (`collectLandings`), `landingRows.ts` | `9f4d5f2a87e4ae9dc78de0cae81424dc08fee5a9` |
| loom-ui `docs/signoz.md` "Limits a question query has to respect" | same SHA; verified by its authors 2026-10-01, not re-verified here |

These are source observations. Nothing here was run against a live SigNoz.

## The contract

- **Landing predicate** (SQL `landings` CTE; loom-ui `isLandingOutcome`):
  `disposition = 'landed'`, or, when `disposition` is absent,
  `result = 'success'` and a `pr_number`.
- **Population.** SQL emits one row per landing sweep (no dedupe). loom-ui keeps
  two populations: `scored` (one per landing sweep, keyed by sweep id; the
  Σ LSI population) and `landings` (earliest landing per `repo#issue`; the count
  and effort-table population). The same issue number in two repos is two issues.
- **Components**, each `z = (ln(1 + x) - mean) / sd`:
  `hw_lines = hw_lines_added + hw_lines_deleted` (both fields required),
  `hw_files`, and `tokens_in + tokens_out` divided by the per-model factor
  (both counters required, `tokens_status` absent or `measured`, and a fitted
  factor for `models_used[0]`).
- **Combination.** `landed_size` is the mean of the present z values; none
  present gives NULL (never 0). `LSI = exp(landed_size)`, so a below-baseline
  landing has a negative `landed_size` and a positive LSI.
- **Class.** Fixed upper cuts on LSI `1, 2, 3, 5, 8, 13`, then `21`.
- **Rounding.** Neither side rounds the score. Parameters are 6-decimal literals.
  Measured agreement between the two implementations is about 2e-16.
- **Version/provenance.** SQL carries `params_version` on every row; the client
  echoes `paramsVersion` in a hint. The fit window is half-open
  `[start, end)` (checked against the real fitter).

## Where the implementations already differ

Each is a corpus case marked `expect: "diverge"`; the check fails if upstream
closes the gap without updating the case.

| Case | SQL reference | loom-ui client |
|---|---|---|
| `landed-without-pr-number` | scored | dropped (needs a numeric `pr_number`) |
| `negative-reading` (`hw_lines_deleted = -3`) | sums it | treats negatives as counter resets, so lines are one-sided and absent |
| `model-field-fallback` (no `models_used`, a `model` field) | tokens absent | falls back to `model` |
| `record-carried-lsi` | ignores a body `lsi` | prefers it |
| `bad-timestamp` | keeps the row | drops it (`Date.parse` NaN) |
| equal-timestamp tie in first-per-issue | no rule | keeps the first row processed (highest export id under the fetch order) |

Any server-side replacement has to choose a side for each of these before it can
claim parity; the choice is a score-definition question and out of scope here.

## Can the deployed query surface express it?

What the pinned sources establish (source evidence, not live):

- **Body-only fields.** `hw_*`, `tokens_*`, `models_used` and `tokens_status`
  exist only in the record body. `kpiLandings.ts` selects `body` verbatim and
  the client parses it.
- **JSON functions.** loom-ui's `docs/signoz.md` records that this instance's
  ClickHouse build rejects `JSON*` functions (`code: 48`). Reading the fields
  out of `body` server-side is therefore unsupported as documented.
- **First-per-issue and `exp(mean(z))`.** `kpiLandings.ts` states neither is a
  builder aggregation. Whether the instance accepts a raw ClickHouse SQL query
  type, and whether the bridge's `defineQuery` registry would admit one, is not
  established by anything read here.
- **A different seam exists.** `landingScore.ts` says the Worker's D1-archive
  export stamps `loom.outcome.landing` / `loom.outcome.lsi` as typed attributes,
  so SigNoz can already sum Σ LSI per host and UTC day without reading bodies.
  That is the TypeScript scorer run at export time, not a SQL dialect, and it
  fixes the parameter version at export time. Its behavior was not inspected
  beyond that comment.

Unsupported as documented: JSON extraction from `body`. Unknown: raw SQL query
type availability, window functions for dedupe, and `exp`/`ln` over aggregates
on the deployed instance.

## Candidate-dialect execution: blocked

No `clickhouse`/`clickhouse-local` is installed on the build host, and live
SigNoz access, new credentials, installation and live queries are outside this
issue's limits. A local ClickHouse would also not be the deployed query surface
(it would accept `JSON*` functions the instance rejects), so it could not
establish parity either way. `run-parity.mjs` prints `CANDIDATE BLOCKED` and
`--require-candidate` exits 3 so the blocker cannot read as a pass.

The smallest capability that would unblock a verdict: an authorized, reviewed
way to run a candidate statement through the deployed instance's registered
query path against the corpus (loaded as rows or typed attributes), with the
instance version recorded.

## Running the checks

```
node defaults/observability/sweep-facts/landed-size-parity/run-parity.mjs
```

Needs `node` and the `sqlite3` CLI (3.35+ for the math functions the view uses).
It runs `landed-size.sql` through real SQLite, scores the same corpus with a
port of loom-ui's scorer (`client-reference.mjs`), compares population, LSI and
class, checks the loom-ui parameter snapshot against the SQL, checks
first-per-issue selection, and runs `fit-landed-size.mjs` on a synthetic extract
to confirm the `[start, end)` window. Recorded result at this commit (sqlite
3.45.1, node 24): all checks hold, with the divergences above reproduced.

Limits of the evidence: the SQL side loads `sweep_facts` rows directly, so the
rollup's JSON extraction is not exercised; `client-reference.mjs` is a hand port
and drifts silently unless re-diffed against the pinned loom-ui files; the
corpus is invented and covers contract edges, not production distributions.

## Design notes for a later, separately authorized implementation

- **Parameters.** Keep one versioned artifact: `landed-size.sql`'s `params` CTE
  and `model_token_factors`, produced by `fit-landed-size.mjs`. loom-ui's
  `landedSizeParams.ts` is a verbatim copy. `loom-ui-params-snapshot.json` plus
  the harness fails when the copy drifts; any server-side consumer must read the
  same artifact rather than a third copy.
- **Handoff.** Review path is the normal Loom PR review of this change; the
  consumer decision stays with loom-ui #1022, which should stay held until a
  reviewed plan, query support and executed parity exist.
- **Failure behavior.** Absent inputs stay absent (NULL score, not 0), a model
  without a factor drops the token component, and a missing or unparseable
  timestamp needs an explicit rule (see the divergence table).
- **Query-id contract.** Not defined here; there is no demonstrated query to
  register. The existing `kpi-landing-repos` / `kpi-landing-rows` ids are
  unchanged.
