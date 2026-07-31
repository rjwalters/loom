/**
 * Historical charts panel (issue #4751): owns fetching `/api/history` (or
 * `/public/history`) and rendering the three charts — outcomes-over-time,
 * success-rate trend, and duration percentiles — into three containers.
 * Framework-agnostic, matching `liveFeedPanel.ts`'s plain-DOM ownership
 * pattern: this class holds the last-applied filter and re-fetches/
 * re-renders on `refresh`, so a filter-control UI (host/repo/model/date-range
 * inputs) can be wired to it without knowing anything about chart internals.
 *
 * The same instance works against either route purely by passing a
 * different `basePath` ("Charts read from /api/history ... confirm the same
 * component can point at /public/history", #4751's last acceptance
 * criterion) — nothing else changes, since `fetchAllHistory` and every
 * `charts/*` transform already tolerate the redacted `/public/history`
 * payload shape.
 */

import type { FetchLike, HistoryQueryFilter } from "./historyClient.js";
import { fetchAllHistory } from "./historyClient.js";
import type { BucketGranularity } from "./charts/timeBuckets.js";
import { buildOutcomesOverTime } from "./charts/outcomes.js";
import { buildSuccessRateTrend } from "./charts/successRate.js";
import { buildDurationPercentiles } from "./charts/durations.js";
import { renderOutcomesChart } from "./charts/outcomesChartView.js";
import { renderSuccessRateChart } from "./charts/successRateChartView.js";
import { renderDurationPercentilesChart } from "./charts/durationsChartView.js";

export interface HistoricalChartsPanelOptions {
  /** `/api/history` or `/public/history` — see module doc. */
  basePath: string;
  outcomesContainer: HTMLElement;
  successRateContainer: HTMLElement;
  durationsContainer: HTMLElement;
  granularity?: BucketGranularity;
  filter?: HistoryQueryFilter;
  fetchImpl?: FetchLike;
}

export class HistoricalChartsPanel {
  private readonly basePath: string;
  private readonly outcomesContainer: HTMLElement;
  private readonly successRateContainer: HTMLElement;
  private readonly durationsContainer: HTMLElement;
  private readonly granularity: BucketGranularity;
  private readonly fetchImpl: FetchLike | undefined;
  private filter: HistoryQueryFilter;

  constructor(options: HistoricalChartsPanelOptions) {
    this.basePath = options.basePath;
    this.outcomesContainer = options.outcomesContainer;
    this.successRateContainer = options.successRateContainer;
    this.durationsContainer = options.durationsContainer;
    this.granularity = options.granularity ?? "daily";
    this.filter = options.filter ?? {};
    this.fetchImpl = options.fetchImpl;
  }

  /** The filter currently applied (the constructor's `filter`, merged with
   * every `refresh(filter)` call since). */
  getFilter(): HistoryQueryFilter {
    return this.filter;
  }

  /**
   * Fetch the full (paginated) result set for the current filter — merged
   * with `filter` when provided — and re-render all three charts from it.
   */
  async refresh(filter?: HistoryQueryFilter): Promise<void> {
    if (filter) this.filter = { ...this.filter, ...filter };

    const records = await fetchAllHistory(this.basePath, this.filter, {
      fetchImpl: this.fetchImpl,
    });

    const buckets = buildOutcomesOverTime(records, this.granularity);
    renderOutcomesChart(this.outcomesContainer, buckets);
    renderSuccessRateChart(this.successRateContainer, buildSuccessRateTrend(buckets));
    renderDurationPercentilesChart(this.durationsContainer, buildDurationPercentiles(records));
  }
}
