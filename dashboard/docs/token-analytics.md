# Token/cost analytics (Epic #4702, Phase 3 — issue #4752)

Burn curves, limit-window forecasting, and per-repo attribution for the fleet
dashboard UI. Implementation: [`../web/src/analytics/`](../web/src/analytics/)
(`burn.ts`, `forecast.ts`, `attribution.ts`, `render.ts`); tests:
[`../web/test/`](../web/test/).

This document records the two questions the issue explicitly left open, and
the answers this implementation commits to.

## 1. Per-repo attribution: the join

`tokens.snapshot` has **no `repo` field**. It is fleet-wide token-pool state
(`.loom/docs/telemetry-schema.md`: "host-level — no `repo` / `visibility`"),
so "which repo burned this quota" is not read off a record — it is *derived*,
by joining the token history against the `sweep.*` records, which are the only
ones carrying `repo`.

### Join key: `hostId` + time overlap

| Component | Kind | Why |
|---|---|---|
| `hostId` | **Hard key** (exact match) | A token pool is a per-host resource (`.loom/tokens/` lives on the machine running `loom-daemon`). Usage observed on host A can only have been spent by sweeps on host A. |
| Time overlap | **Soft key** (interval overlap, not instant equality) | Nothing links a pool account to a sweep directly; co-occurrence in time is the only available evidence. |
| `account` | **Dimension, not a key** | `tokens.snapshot` accounts have no `model`, and no `sweep.*` record has an `account`. The account a delta came from is known exactly, so it is reported as a breakdown — but it cannot narrow *which sweep* spent it. |
| `model` | **Dimension, not a key** | Same reason, from the other side: `sweep.started.model` has no account to match against. `byModel` inherits whatever proportional split the sweeps got. |

The issue sketched a "per account/model" join. That is not derivable from the
telemetry as it exists — implementing it would mean inventing a correlation —
so both fields are carried as *breakdowns of an already-computed attribution*
instead.

### The unit of attribution is an interval, not an instant

Each consecutive pair of `tokens.snapshot` samples on one host defines a
**usage interval** `[prev.captured_at, next.captured_at)` and, per account, a
**usage delta** `next.usage_fraction - prev.usage_fraction`. That delta is the
fleet's measured consumption over exactly that interval. Attribution is the
question of who was running during it.

For each interval, with all same-host sweep windows that overlap it:

1. **Coverage** — the *union* of the overlapping windows (clipped to the
   interval) decides how much of the delta is attributable at all. Usage is
   assumed uniform across the interval, so an interval only 20% covered by
   sweeps yields 20% attributable usage. Union, not sum: two concurrent sweeps
   cover the same seconds once.
2. **Split** — the attributable portion is divided among the overlapping
   sweeps *in proportion to each sweep's own overlap duration* (sweep-seconds),
   then summed per repo. Two sweeps covering the whole interval split it 50/50.
3. **Remainder** — the uncovered share goes to **`unattributed`**.

### Tolerances

| Knob | Default | Rationale |
|---|---|---|
| `edgeToleranceMs` | **60 s** | Sweep windows are widened by this on both ends before overlap is tested. `captured_at` (pool sampler) and `started_at` (sweep) are independent daemon timestamps; without slack a sweep starting moments after a snapshot loses its first interval outright. Because coverage is proportional (above), a sweep that only touches an interval through this tolerance claims only those 60 seconds' worth — the slack cannot swallow a whole interval. |
| `maxSampleGapMs` | **60 min** | A pair of snapshots further apart than this is dropped entirely (counted as `droppedIntervals`). The usage in between is real but unobserved; smearing an hours-wide delta across whatever sweeps happen to overlap it would be fiction. |
| `openSweepMaxDurationMs` | **6 h** | A sweep with `sweep.started` but no terminal record is capped at this past its start (and at `now`), so one crashed sweep that never emitted `sweep.completed` cannot absorb the fleet's usage indefinitely. |

All three are parameters of `attributeUsageToRepos`, defaulted, and covered by
tests at and around their boundaries.

### What the model deliberately does not claim

- **A negative delta is a limit-window rollover, not negative usage.** That
  interval is skipped for that account (`rolloverIntervals`), never attributed
  and never subtracted.
- **Unattributed usage is reported, never redistributed.** Support-role crons
  (Judge/Champion/Curator/Guide), manual sessions, and anything else that does
  not emit `sweep.*` telemetry burn real tokens. Spreading that across the
  repos that *did* run sweeps would inflate every number on the page. A large
  unattributed row is the signal that the attribution is not to be trusted —
  which is exactly what it should be. **One narrow slice of this bucket has a
  separate, local breakdown**: Guide's Document Maintenance phase (doc
  maintenance / WORK_LOG.md-WORK_PLAN.md-README.md PRs) records its own
  per-PR telemetry — PR count and phase-duration-as-spend-proxy over a
  configurable window — via `./.loom/scripts/guide-docs-telemetry.sh report`,
  entirely outside this `sweep.*`-attribution model and this dashboard (issue
  #6136; see `.loom/docs/observability.md` §5b for the full mechanism). It
  does not shrink the `unattributed` number this page reports — that model is
  still `sweep.*`-only — it is a second, independent way to answer "how much
  went to doc maintenance specifically" without touching this attribution
  pipeline at all.
- **Units are limit-window fractions**, not tokens or dollars: `1.00` = one
  account's entire limit window. No absolute token count or price exists
  anywhere in the telemetry, so none is synthesized.
- **Attribution is evidential, not causal.** It says "these sweeps were running
  while this quota was consumed", weighted by time. It is a good estimate at
  fleet scale and a poor one for a single short interval; the coverage
  footnote under the table exists so a reader can see which they are looking
  at.

## 2. Public-exposure decision: pool-level aggregate, not per-account detail

**Original decision (issue #4752, pre-#4795):** the token/cost analytics
rendered on the authenticated route only — the whole panel was withheld from
the public surface.

**Current decision (2026-07-31, issue #4847): the signed-in dashboard shows
per-account token detail; the public view shows pool-level aggregate stats
instead of a withheld notice.** Phase 2's redaction policy
([`../src/redaction.ts`](../src/redaction.ts)) already drew the line this
panel now follows: `/public/history`'s `tokens.snapshot` carries no
`accounts[]` at all — `deriveTokenPoolAggregate` replaces it with a
non-identifying `account_count` / `exhausted_count` / `mean_usage_fraction` /
`max_usage_fraction` / `next_limit_window_reset_at` summary. A public render
built from that shape can never surface an account identifier; the work this
issue added was a **new pool-level computation** (a burn series over the
aggregate, segmented at rollovers the same way the per-account curves are —
see [`web/src/analytics/burn.ts`](../web/src/analytics/burn.ts)'s
`buildPoolBurnCurves`), not a redaction change.

What still does **not** render publicly, and why:

1. **Per-repo attribution is a repo-name table by construction.** Its entire
   output is "which repositories consumed the fleet's quota" — and repo names
   are precisely what Phase 2 strips from `/public/history` for
   private-visibility sweeps. Publishing this panel would reconstruct, by
   inference from timing, the exact fact the redaction layer removes. Also
   noted in [`render.ts`](../web/src/analytics/render.ts)'s module doc:
   attributing usage to time windows can reconstruct private-sweep timing by
   inference even without repo names — a second, independent reason both the
   table and the sweep-window join stay behind the Access gate.
2. **Per-account exhaustion forecasts are a scheduling signal keyed to an
   identity.** "This fleet runs dry in 40 minutes" is one thing; "*agent-3*
   runs dry in 40 minutes" ties that to operator infrastructure. The
   pool-level summary reports the same risk signal (`exhausted_count`,
   `max_usage_fraction`) without naming which account it is — and,
   deliberately, without projecting an ETA from it either: a pool-wide mean
   can sit comfortably mid-range while one account is a sample away from
   exhaustion, so a trend line through the mean would be actively misleading.
   See [`web/src/analytics/forecast.ts`](../web/src/analytics/forecast.ts)'s
   `summarizePoolHealth`.

### How it is enforced

Two independent points, neither of which is a redaction change:

- [`web/src/analytics/render.ts`](../web/src/analytics/render.ts) —
  `renderTokenAnalytics` renders the pool-level burn/health blocks on a
  `"public"` surface (never the per-account burn curves, forecast table, or
  attribution table), with a short notice naming only the two blocks that
  stay operator-only.
- [`web/src/analytics/api.ts`](../web/src/analytics/api.ts) — `fetchHistory`'s
  `surface` option selects `/api/history` or `/public/history` explicitly;
  `mountTokenAnalytics` always passes the same surface it renders with, so a
  public render can only ever have requested `/public`. Even if a caller got
  that wrong, the backend does not trust the client's choice of prefix:
  `/public/history` redacts `tokens.snapshot` down to the aggregate
  server-side regardless, so per-account detail cannot reach the browser
  through this path no matter which surface a caller asks for.

The surface itself is derived from the server-injected auth state
(`bootstrap.ts`'s `currentSurface`, issue #4795), mirroring the backend's own
route-based auth split — so a link cannot talk the panel into requesting
`/api` publicly.

### Coordination with the public view page (#4753)

The public page **should** embed this panel — the pool-level burn/health
blocks are the purpose-built, non-identifying aggregate the original
`token-analytics.md` speculated about ("fleet capacity: healthy / degraded",
carrying no account names, no repo names, no per-account timing). It should
**not** embed per-repo attribution or per-account forecasts; those remain
authenticated-only per the decision above.
