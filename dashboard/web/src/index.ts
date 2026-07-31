/**
 * Public entry point for `loom-fleet-dashboard-web` (Epic #4702, Phase 3).
 *
 * Two feature areas share one package because they share one wire contract
 * (`types.ts`, mirroring `dashboard/src/query.ts`):
 *  - **Live view** (#4750) — SSE feed client, timeline builder, and the panel
 *    /timeline view components over `GET /api/events`.
 *  - **Historical charts** (#4751) — a keyset-paginating `GET /api/history` /
 *    `GET /public/history` client plus framework-agnostic transforms that turn
 *    those records into chart-ready datasets.
 */

// Shared wire-format types.
export * from "./types.js";

// Live event feed + per-sweep timeline (#4750).
export * from "./sseFeedClient.js";
export * from "./timelineBuilder.js";
export * from "./liveFeedPanel.js";
export * from "./sweepTimelineView.js";

// Historical-charting data layer + chart views (#4751).
export * from "./historyClient.js";
export * from "./charts/timeBuckets.js";
export * from "./charts/correlate.js";
export * from "./charts/outcomes.js";
export * from "./charts/successRate.js";
export * from "./charts/durations.js";
export * from "./charts/outcomesChartView.js";
export * from "./charts/successRateChartView.js";
export * from "./charts/durationsChartView.js";
export * from "./historicalChartsPanel.js";

// Token/cost analytics (issue #4752). `types.js` is exported first above, so
// the analytics modules are re-exported individually rather than via a nested
// barrel — `analytics/types.js` deliberately layers its own domain shapes on
// the shared wire types and only the domain shapes belong on this surface.
export type {
  AccountReading,
  HistoryEnvelope,
  SweepWindow,
  TokenAccountPayload,
  TokenSample,
  TokensSnapshotPayload,
} from "./analytics/types.js";
export * from "./analytics/parse.js";
export * from "./analytics/burn.js";
export * from "./analytics/forecast.js";
export * from "./analytics/format.js";
// `attribution.ts` and `burn.ts` each own a `DEFAULT_MAX_SAMPLE_GAP_MS`. They
// are genuinely two independent knobs — one decides where a burn curve is cut
// into segments, the other decides which snapshot pairs are too far apart to
// attribute at all — so they are not merged; the barrel disambiguates instead.
export {
  attributeUsageToRepos,
  DEFAULT_EDGE_TOLERANCE_MS,
  DEFAULT_MAX_SAMPLE_GAP_MS as DEFAULT_ATTRIBUTION_MAX_SAMPLE_GAP_MS,
  DEFAULT_OPEN_SWEEP_MAX_DURATION_MS,
} from "./analytics/attribution.js";
export type {
  AttributionOptions,
  AttributionResult,
  NamedUsage,
  RepoAttribution,
} from "./analytics/attribution.js";
export * from "./analytics/api.js";
export * from "./analytics/render.js";
export * from "./analytics/bootstrap.js";
