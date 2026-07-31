# loom-fleet-dashboard-web

Frontend for the Loom fleet observability dashboard
([Epic #4702](https://github.com/rjwalters/loom/issues/4702), Phase 3), built
on the Phase 2 query API (`dashboard/src/query.ts`, documented in
[`../docs/query-api.md`](../docs/query-api.md)):

- **Live view** ([#4750](https://github.com/rjwalters/loom/issues/4750)) — a
  live event feed panel and a per-sweep timeline view over `GET /api/events`.
- **Historical charts** ([#4751](https://github.com/rjwalters/loom/issues/4751))
  — sweep outcomes over time, success-rate trends, and duration percentiles
  over `GET /api/history` / `GET /public/history`.

This is a minimal, self-contained scaffold — plain TypeScript, no UI
framework — sufficient to implement and test the Phase 3 logic. It is
expected to be merged/rebased against whatever scaffold the sibling
Fleet-overview issue ([#4749](https://github.com/rjwalters/loom/issues/4749))
introduces for `dashboard/web/`; the logic modules here are
framework-independent and the view modules use plain DOM APIs, so they can be
adapted into whatever component model #4749 lands with.

## Modules

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

## Development

```bash
npm install
npm run check   # typecheck (tsc --noEmit) + vitest run
```
