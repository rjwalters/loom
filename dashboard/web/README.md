# Fleet dashboard UI

The rich fleet observability dashboard (Epic
[#4702](https://github.com/rjwalters/loom/issues/4702), Phase 3). A small
single-page app that reads the server-aggregated fleet snapshot from the
Phase-2 Workers backend in `../` and renders it.

**This file is the canonical scaffold for Phase 3.** Vite + plain TypeScript,
no UI framework. The Phase-3 issues that share it:

- **Fleet overview + host drill-down**
  ([#4749](https://github.com/rjwalters/loom/issues/4749)) — the app shell,
  router, and views described below.
- **Live event feed + per-sweep timeline**
  ([#4750](https://github.com/rjwalters/loom/issues/4750)) — over
  `GET /api/events`.
- **Historical charts**
  ([#4751](https://github.com/rjwalters/loom/issues/4751)) — sweep outcomes
  over time, success-rate trends, and duration percentiles over
  `GET /api/history` / `GET /public/history`.

The choices below are the ones sibling issues should follow, and the reasons
are recorded so a later issue can overturn one deliberately rather than by
accident.

---

## What it shows

| View | Route | Content |
|---|---|---|
| Fleet overview | `#/` | One card per host: the whole `host.health` field set (`daemon_version`, `uptime_sec`, `logical_cpus`, `cpu_idle_fraction`, `load_per_core`, `worktree_root_free_gb`), a `tokens.snapshot` summary (exhausted count + peak `usage_fraction`), a status badge, and that host's live sweeps. |
| Host drill-down | `#/hosts/<hostId>` | The same health fields in full, one row per token-pool account, and one row per in-flight sweep with `repo` / `issue` / `phase` / `model` / `effort` / how long it has been running (`startedAt`) / how long it has been in its current phase (`enteredPhaseAt`). |

Hosts needing attention sort first (stale, then token-degraded), then busiest,
then by id — so the list does not reshuffle between polls when nothing changed.

### What it replaces

`loom-daemon serve`'s `--peers` panel (`loom-daemon/src/dashboard.html`'s
`refreshPeers`), where the *browser* fans out to every peer's `/api/status`.
That requires each peer to be reachable from the viewer's network and degrades
per-peer. Here the Worker has already aggregated the fleet from pushed
telemetry, so the UI makes exactly one request and hosts never accept an
inbound connection. `loom-daemon/src/dashboard.html` is a read-only precedent
for the visual language — it is not extended, and nothing here is built on it.

---

## Data source and auth

**`GET /api/fleet-state`, and nothing else.** Response shape:
[`../docs/query-api.md`](../docs/query-api.md); the Durable Object that
produces it is [`../src/fleetState.ts`](../src/fleetState.ts).

There is **no auth code in this app**, by design. `/api/*` is gated by
Cloudflare Access at the edge and the Worker itself verifies no JWT (see
[`../docs/cloudflare-access.md`](../docs/cloudflare-access.md) §5), so the UI
simply sends `credentials: "same-origin"` and lets the Access session cookie
ride along. The only concession is that `src/api.ts` recognizes `401`/`403` and
tells the operator to reload to re-authenticate, instead of reporting a backend
fault.

**Sibling issues: keep it that way.** Do not add token handling, a login
screen, or an `Authorization` header. The public view (`/public/*`) is a
different *route*, not a different credential.

---

## Module map

### Live event feed + per-sweep timeline (#4750)

- `src/sseFeedClient.ts` — `SseStreamParser` (incremental SSE frame
  parsing: `retry:` directives, `:`-comments, `data:` frames) and
  `LiveFeedClient` (reconnecting client for `GET /api/events`: honors the
  `retry: 3000` preamble, ignores `: keepalive` comments, dedups frames
  across reconnects).
- `src/timelineBuilder.ts` — `SweepTimelineBuilder` / `buildSweepTimeline`:
  aggregates `sweep.phase` (+ `sweep.started` / `sweep.completed` /
  `sweep.outcome`) records for one `sweep_id` into a phase-progression
  timeline with computed per-phase durations.
- `src/liveFeedPanel.ts` — `LiveFeedPanel`: renders the live feed, with
  client-side `model`/`result` filtering (the live-tail endpoint only
  supports `host`/`repo` server-side).
- `src/sweepTimelineView.ts` — `SweepTimelineView`: renders one sweep's
  timeline, including the terminal `result` and PR link derived from
  `sweep.outcome`'s `pr_number`.

### Historical-charting data layer (#4751)

- `src/historyClient.ts` — pages through `/api/history` (or
  `/public/history`) via the keyset `nextCursor`, accumulating every record.
  Takes the route's base path as a parameter
  (`fetchAllHistory("/api/history", filter)` vs.
  `fetchAllHistory("/public/history", filter)`) — one function, both routes,
  no duplication (the last acceptance criterion of #4751).
- `src/charts/correlate.ts` — joins `sweep.completed` and `sweep.outcome`
  records by `sweepId` into one merged view per sweep (result + model +
  phase/total durations), since those are two separate telemetry events for
  the same sweep.
- `src/charts/timeBuckets.ts` — UTC daily/weekly bucketing helper.
- `src/charts/outcomes.ts` — outcomes-over-time: sweep counts by `result`,
  bucketed daily or weekly.
- `src/charts/successRate.ts` — success-rate trend, derived from the
  outcomes-over-time buckets (not re-queried independently, so the two charts
  can never disagree with each other).
- `src/charts/durations.ts` — p50/p90/p99 duration percentiles, overall and
  broken down by phase.
- `src/charts/outcomesChartView.ts` — `renderOutcomesChart`: renders
  `OutcomeBucket[]` as a stacked SVG bar chart, one bar per bucket segmented
  by result.
- `src/charts/successRateChartView.ts` — `renderSuccessRateChart`: renders
  `SuccessRatePoint[]` as an SVG line chart; a bucket with no completed
  sweeps (`successRate: null`) breaks the line into a gap rather than
  interpolating through it.
- `src/charts/durationsChartView.ts` — `renderDurationPercentilesChart`:
  renders `DurationPercentiles` as a horizontal grouped-bar chart (one row
  per `overall`/phase, one bar per percentile rank).
- `src/historicalChartsPanel.ts` — `HistoricalChartsPanel`: owns fetching
  `/api/history` (or `/public/history`) via `fetchAllHistory` and rendering
  all three charts from the result; `refresh(filter)` re-fetches (merging
  the new filter over the last-applied one) and re-renders, so a filter-input
  UI has one method to call.

Rendering is plain SVG via DOM APIs (no charting library), matching
`liveFeedPanel.ts` / `sweepTimelineView.ts`'s framework-agnostic approach —
issue #4751 calls the sibling Fleet-overview scaffold (#4749) a soft
dependency ("otherwise independently implementable"), the same precedent
#4750's panel/timeline views already established.

## Filters

`HistoryQueryFilter` (`src/historyClient.ts`) matches `GET /api/history`'s
query parameters exactly: `host`, `repo`, `model`, `result`, `since`,
`until`, `limit` (`cursor` is handled internally by `fetchAllHistory`'s
pagination loop, not exposed to callers).

## Usage

```ts
import {
  fetchAllHistory,
  buildOutcomesOverTime,
  buildSuccessRateTrend,
  buildDurationPercentiles,
} from "./src/index.js";

const records = await fetchAllHistory("/api/history", {
  repo: "rjwalters/loom",
  since: "2026-07-01T00:00:00Z",
});

const outcomeBuckets = buildOutcomesOverTime(records, "daily");
const successRate = buildSuccessRateTrend(outcomeBuckets);
const durationPercentiles = buildDurationPercentiles(records);
```

The exact same call with `"/public/history"` in place of `"/api/history"`
works unchanged against the redacted public dataset — every transform
tolerates the reduced `record` shape `/public/history` returns for
`visibility: "private"` rows (see `HistoryRecord`'s doc in `src/types.ts`).

Or let `HistoricalChartsPanel` own fetch + render end to end:

```ts
import { HistoricalChartsPanel } from "./src/index.js";

const panel = new HistoricalChartsPanel({
  basePath: "/api/history", // or "/public/history" for the redacted view
  outcomesContainer: document.querySelector("#outcomes")!,
  successRateContainer: document.querySelector("#success-rate")!,
  durationsContainer: document.querySelector("#durations")!,
  filter: { repo: "rjwalters/loom" },
});

await panel.refresh();
// Later, e.g. from a filter form's submit handler:
await panel.refresh({ since: "2026-07-01T00:00:00Z" });
```

## Setup

```bash
cd dashboard/web
npm install
npm run check     # typecheck (tsc --noEmit) + vitest run
npm run build     # -> dist/, which the Worker uploads as static assets
```

Live development needs both halves running — the Worker for the API, Vite for
the UI with hot reload:

```bash
cd dashboard && npm run dev        # wrangler dev on :8787
cd dashboard/web && npm run dev    # vite on :5173, proxying /api + /public to :8787
```

`vite.config.ts` proxies `/api` and `/public` to `127.0.0.1:8787`, so
`fetch("/api/fleet-state")` is same-origin in development exactly as it is in
production — no CORS, and no environment-conditional base URL.

To exercise the *built* bundle exactly as deployed, skip Vite: `npm run build`
here, then `npm run dev` in `../` and open <http://127.0.0.1:8787/>.

---

## Deployment: Workers Assets on the same Worker

The built `dist/` is uploaded by the sibling Worker, declared in
[`../wrangler.toml`](../wrangler.toml):

```toml
[assets]
directory = "./web/dist"
not_found_handling = "none"
```

`npm run deploy` in `../` runs this build first, then `wrangler deploy` — one
command, one Worker, one hostname.

**Why one Worker instead of Cloudflare Pages + a separate API Worker:**

- **One Access policy.** The epic's whole auth story is "Cloudflare Access
  gates the hostname". Two hostnames means two Access applications kept in
  sync, and a cross-origin session cookie.
- **No CORS.** The UI and `/api/*` are same-origin by construction.
- **One deploy artifact.** The UI and the API version it was written against
  ship together; there is no window where a new UI is talking to an old
  backend.

Two consequences worth knowing:

- **`not_found_handling = "none"` is load-bearing.** It makes any request that
  does not match a built file fall through to the Worker, which is what keeps
  `/ingest`, `/admin/*`, `/api/*`, and `/public/*` alive. The SPA setting
  (`"single-page-application"`) would rewrite every unmatched path to
  `index.html` and shadow the entire API. That is why routing is **hash-based**
  (`#/hosts/<id>`, `src/router.ts`): a hash is never sent to the server, so
  deep links survive a hard refresh with no server-side rewrite at all.
- **`GET /` now serves the UI**, not the Worker's old plain-text banner. The
  banner is still in `../src/index.ts` as the fallback when no assets are
  uploaded.
- **`web/dist/` must exist for Wrangler to parse `../wrangler.toml`** —
  including for `npm test` and `npm run preflight` in `../`, which have nothing
  to do with the UI. `../scripts/ensure-web-dist.sh` drops a labelled
  placeholder when it does not; the preflight warns if you are about to deploy
  that placeholder.

---

## Why vanilla TypeScript (no framework)

Vite for tooling, TypeScript everywhere, **no UI framework** and no runtime
dependencies — the production bundle is ~18 kB, and `npm install` pulls only
Vite, TypeScript, Vitest, and happy-dom.

The reasoning, so a sibling issue can revisit it on evidence:

- **There is no client-side state to reconcile.** Every view is a pure
  `(viewModel) => HTMLElement` and the whole page re-renders from a fresh
  snapshot on each poll. A virtual DOM's value is diffing away re-renders that
  are expensive or that would lose focus/scroll state; with a handful of host
  cards there is nothing to win.
- **Testability comes from the shape, not the framework.** The split below
  means the interesting logic (parsing, joining, formatting, the state machine)
  is tested with no DOM at all, and the views are tested by calling them and
  querying the returned element. No renderer harness, no `act()`, no
  framework-version churn in the test suite.
- **It matches what it replaces.** `loom-daemon/src/dashboard.html` is
  hand-written DOM with no build step; this is the same idiom with types, a
  bundler, and tests.

**When to overturn this:** a framework earns its place the moment a view needs
*durable local state* across re-renders — the live feed's scroll-follow and
pause-on-hover, or a chart with a brushed time range, are the plausible
candidates. If a Phase-3 sibling hits that, adding a framework is a contained
change (the views are pure functions with no shared runtime), but do it
**once**, in one issue, for the whole app — not per view.

Charting is orthogonal: the historical-charts issue will need a rendering
library regardless, and picking one (uPlot, Chart.js, …) does not imply
adopting a UI framework.

---

## Code layout

Split so that the parts worth testing hard need no DOM:

| Module | Responsibility |
|---|---|
| `src/types.ts` | Two contracts in one file: the `/api/fleet-state` snapshot shape, re-declared for the browser (the backend's own types depend on `@cloudflare/workers-types`), plus the raw telemetry-record / SSE-frame types the live feed and timeline consume. |
| `src/parse.ts` | Wire JSON → those types. Drops wrong-typed/absent fields; never throws on a malformed payload. |
| `src/api.ts` | The one fetch. Error classification (`FleetStateError`, the Access hint). |
| `src/fleet.ts` | Snapshot → view model: joins `hosts` with `activeSweeps`, derives host status, sorts. |
| `src/format.ts` | Display formatting. Owns the unknown-is-not-zero rule. |
| `src/dom.ts` | `el()` — a ~30-line DOM builder. Text always via `textContent`. |
| `src/router.ts` | Hash routing. |
| `src/views/*.ts` | Pure `(viewModel) => HTMLElement` renderers, including the loading/empty/error states. |
| `src/app.ts` | The controller: fetch → render, polling, routing, error-over-stale-data. All dependencies (fetch, clock, timers) injected. |
| `src/main.ts` | Browser wiring only — nothing testable lives here. |

### Live feed and per-sweep timeline (#4750)

Issue [#4750](https://github.com/rjwalters/loom/issues/4750) landed a second,
framework-independent module set in this same package, against the *raw*
telemetry stream (`GET /api/events`, `GET /api/history`) rather than the
aggregated snapshot. It follows the same split — DOM-free logic modules plus
plain-DOM views — and shares this package's tooling (`vite.config.ts`,
`tsconfig.json`, happy-dom):

| Module | Responsibility |
|---|---|
| `src/sseFeedClient.ts` | `SseStreamParser` (incremental SSE frame parsing: `retry:` directives, `:`-comments, `data:` frames) and `LiveFeedClient` (reconnecting client for `GET /api/events`: honors the `retry: 3000` preamble, ignores `: keepalive` comments, dedups frames across reconnects). |
| `src/timelineBuilder.ts` | `SweepTimelineBuilder` / `buildSweepTimeline`: aggregates `sweep.phase` (+ `sweep.started` / `sweep.completed` / `sweep.outcome`) records for one `sweep_id` into a phase-progression timeline with computed per-phase durations. |
| `src/liveFeedPanel.ts` | `LiveFeedPanel`: renders the live feed, with client-side `model`/`result` filtering (the live-tail endpoint only supports `host`/`repo` server-side). |
| `src/sweepTimelineView.ts` | `SweepTimelineView`: renders one sweep's timeline, including the terminal `result` and PR link derived from `sweep.outcome`'s `pr_number`. |
| `src/index.ts` | Barrel re-export of the four modules above plus `src/types.ts`. |

These are not yet mounted by `src/app.ts` / `src/router.ts` — wiring them into
a route is follow-up work; they are self-contained and tested standalone.

### Three invariants the tests pin

1. **Unknown is never zero.** `.loom/docs/telemetry-schema.md` requires that an
   unmeasurable probe be *absent*; rendering an absent `cpu_idle_fraction` as
   `0%` would show a pegged CPU on a host that simply has no CPU probe, and an
   absent `worktree_root_free_gb` as `0 GB` would show a full disk. `parse.ts`
   drops it, `format.ts` renders `—`.
2. **The host set is the union of `hosts` and `activeSweeps`.** The Durable
   Object creates a `hosts` entry only on `host.health`/`tokens.snapshot`, so a
   host whose first push was `sweep.started` has live sweeps and no `hosts`
   key. Keying off `hosts` alone silently hides a busy host.
3. **Anything that is not exactly `"public"` is private.** The same fail-safe
   decode the daemon and the Worker implement — it matters for the public view
   sibling issue, and is enforced here at parse time so no view can get it
   wrong.

### Empty and error states are first-class

They are the *common* states on a fresh deploy, not edge cases, so they are
views with their own tests (`src/views/states.ts`):

- **Loading** — only until the first response; a later poll never blanks the page.
- **Empty fleet** — the backend answered with zero hosts. Points at the deploy
  runbook's provisioning steps rather than looking broken.
- **Host with no `health`/`tokens` yet** — explains it is known from sweep
  activity alone.
- **Host with zero `activeSweeps`** — an idle host is healthy; the panel says
  so and points at `/api/history` for completed sweeps.
- **Fetch failure** — a full-page error only when there is no prior snapshot;
  otherwise a banner *above* the last good data, so a transient blip does not
  destroy the view an operator is reading.
- **Unknown host in the URL** — a stale bookmark, with a way back.

---

## Tests

```bash
npm test               # happy-dom, no network
npm run check          # typecheck + tests
```

From `../`: `npm run test:web`, or `npm run check:all` for backend + UI.

**One test environment, one config file.** `vite.config.ts` is the single
authoritative config: it drives the dev server, the production build, *and*
Vitest (via `vitest/config`'s widened `defineConfig`), with
`test.environment: "happy-dom"` for every file. Do not add a `vitest.config.ts`
— Vitest resolves `vitest.config.*` ahead of `vite.config.*`, so a second file
silently shadows the `test` block here and drops every DOM test back to
`environment: "node"`. Likewise, do not add per-file
`// @vitest-environment jsdom` pragmas: jsdom is not a dependency of this
package, and a mixed jsdom/happy-dom suite means two DOM implementations to
keep in agreement.

Fixtures (`test/fixtures.ts`) are **wire-shaped**, not pre-parsed view models,
so every test exercises `parse.ts` the same way a live response does. They
deliberately include a host with unmeasured health fields, a host known only
from `activeSweeps`, an idle host, a stale host, and a partially-reported sweep.
