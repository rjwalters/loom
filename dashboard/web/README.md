# loom-fleet-dashboard-web

Frontend for the Loom fleet observability dashboard
([Epic #4702](https://github.com/rjwalters/loom/issues/4702), Phase 3):
a live event feed panel and a per-sweep timeline view over the Phase 2
query API (`dashboard/src/query.ts`, documented in
[`../docs/query-api.md`](../docs/query-api.md)).

This is a minimal, self-contained scaffold — plain TypeScript, no UI
framework — sufficient to implement and test the two Phase 3 components
described in issue #4750. It is expected to be merged/rebased against
whatever scaffold the sibling Fleet-overview issue (#4749) introduces for
`dashboard/web/`; the logic modules here (`sseFeedClient.ts`,
`timelineBuilder.ts`) are framework-independent and the two view modules
(`liveFeedPanel.ts`, `sweepTimelineView.ts`) use plain DOM APIs so they can
be adapted into whatever component model #4749 lands with.

## Modules

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

## Development

```bash
npm install
npm run check   # typecheck + vitest run
```
