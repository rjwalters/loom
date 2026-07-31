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
  (Judge/Champion/Curator), manual sessions, and anything else that does not
  emit `sweep.*` telemetry burn real tokens. Spreading that across the repos
  that *did* run sweeps would inflate every number on the page. A large
  unattributed row is the signal that the attribution is not to be trusted —
  which is exactly what it should be.
- **Units are limit-window fractions**, not tokens or dollars: `1.00` = one
  account's entire limit window. No absolute token count or price exists
  anywhere in the telemetry, so none is synthesized.
- **Attribution is evidential, not causal.** It says "these sweeps were running
  while this quota was consumed", weighted by time. It is a good estimate at
  fleet scale and a poor one for a single short interval; the coverage
  footnote under the table exists so a reader can see which they are looking
  at.

## 2. Public-exposure decision: authenticated surface only

**Decision: the token/cost analytics render on the authenticated route only.**

Phase 2's redaction policy ([`../src/redaction.ts`](../src/redaction.ts))
passes `tokens.snapshot` through **unredacted on both `/api` and `/public`** —
the kind has no `repo` to key visibility off, so the private/public split has
nothing to act on. That remains correct backend behavior and **this issue
changes nothing in `redaction.ts`**. But "the API returns it" is not "the page
should show it", and this is where that distinction is drawn:

1. **Per-repo attribution is a repo-name table by construction.** Its entire
   output is "which repositories consumed the fleet's quota" — and repo names
   are precisely what Phase 2 strips from `/public/history` for
   private-visibility sweeps. Publishing this panel would reconstruct, by
   inference from timing, the exact fact the redaction layer removes.
2. **Account identifiers are operator infrastructure.** `agent-3` +
   `usage_fraction` + `limit_window_reset_at` is a live capacity map of the
   operator's account pool: how many accounts exist, which are near their cap,
   and when each recovers.
3. **Exhaustion forecasts are a scheduling signal.** "This fleet runs dry in 40
   minutes" should be a deliberate publication choice, not a side effect of
   which route happens to permit it.

### How it is enforced

Two independent UI-layer points, neither of which is a redaction change:

- [`web/src/analytics/render.ts`](../web/src/analytics/render.ts) —
  `renderTokenAnalytics` / `mountTokenAnalytics` refuse to render on a
  `"public"` surface and show a short "withheld" notice instead. On the public
  surface `mountTokenAnalytics` **makes no request at all**; it does not fetch
  and then hide.
- [`web/src/analytics/api.ts`](../web/src/analytics/api.ts) — the fetch prefix
  is the constant `/api`, not a parameter, so the view cannot be repointed at
  `/public` by configuration.

The surface itself is derived from the served path
(`bootstrap.ts`'s `surfaceFromPath`), mirroring the backend's own route-based
auth split — so a link cannot talk the panel into rendering publicly.

### Coordination with the public view page (#4753)

The public page should **not** embed these widgets. If a public capacity
summary is wanted later, the right shape is a purpose-built aggregate (e.g.
"fleet capacity: healthy / degraded") carrying no account names, no repo names,
and no per-account timing — a new component, not a flag on this one.
