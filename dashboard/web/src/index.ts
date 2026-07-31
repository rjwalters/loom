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
