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
 *
 * **What it asks the API for, and why.** The three charts consume exactly
 * two record kinds — `sweep.completed` and `sweep.outcome` (see
 * `charts/correlate.ts`). The history table is dominated by everything
 * else: every host pushes a `host.health` + `tokens.snapshot` pair every few
 * minutes, so on a four-host fleet the table passes a quarter of a million
 * rows within weeks and the sweep records are a few percent of it. The
 * first version of this panel fetched *all* of it, unfiltered, at the
 * server's default 50-row page — thousands of sequential round trips, with
 * nothing drawn until the last one landed — which is why the Charts tab
 * looked permanently empty. Now: one query per kind (the API matches one
 * `kind` per request), at the 500-row cap, bounded to a rolling window,
 * with a loading state up front and a no-data state at the end.
 */

import { el } from "./dom.js";
import type { FetchLike, HistoryQueryFilter } from "./historyClient.js";
import { fetchAllHistory } from "./historyClient.js";
import type { HistoryRecord } from "./types.js";
import type { BucketGranularity } from "./charts/timeBuckets.js";
import { buildOutcomesOverTime } from "./charts/outcomes.js";
import { buildSuccessRateTrend } from "./charts/successRate.js";
import { buildDurationPercentiles } from "./charts/durations.js";
import { renderOutcomesChart } from "./charts/outcomesChartView.js";
import { renderSuccessRateChart } from "./charts/successRateChartView.js";
import { renderDurationPercentilesChart } from "./charts/durationsChartView.js";

/** The record kinds the charts are built from — one API query each. */
export const CHART_RECORD_KINDS = ["sweep.completed", "sweep.outcome"] as const;

/** Page size for every history query: the server's cap. */
export const CHART_PAGE_SIZE = 500;

/** How far back the charts look when the caller has not set `since`. Daily
 * buckets over a month is what the outcomes chart is sized for; a wider
 * window is a caller's choice via `filter.since`, not the default. */
export const DEFAULT_WINDOW_DAYS = 30;

export interface HistoricalChartsPanelOptions {
  /** `/api/history` or `/public/history` — see module doc. */
  basePath: string;
  outcomesContainer: HTMLElement;
  successRateContainer: HTMLElement;
  durationsContainer: HTMLElement;
  granularity?: BucketGranularity;
  filter?: HistoryQueryFilter;
  fetchImpl?: FetchLike;
  /** Rolling window applied when `filter.since` is unset. */
  windowDays?: number;
  /** Clock, injectable for tests. */
  now?: () => Date;
}

export class HistoricalChartsPanel {
  private readonly basePath: string;
  private readonly outcomesContainer: HTMLElement;
  private readonly successRateContainer: HTMLElement;
  private readonly durationsContainer: HTMLElement;
  private readonly granularity: BucketGranularity;
  private readonly fetchImpl: FetchLike | undefined;
  private readonly windowDays: number;
  private readonly now: () => Date;
  private filter: HistoryQueryFilter;
  /** The last fetched records, kept so a container resize re-renders the
   * charts at the new pixel width without another round trip (#8546: the
   * charts render at fixed pixel size rather than scaling a viewBox). */
  private records: HistoryRecord[] = [];
  private resizeObserver: ResizeObserver | undefined;
  private lastWidth = 0;

  constructor(options: HistoricalChartsPanelOptions) {
    this.basePath = options.basePath;
    this.outcomesContainer = options.outcomesContainer;
    this.successRateContainer = options.successRateContainer;
    this.durationsContainer = options.durationsContainer;
    this.granularity = options.granularity ?? "daily";
    this.filter = options.filter ?? {};
    this.fetchImpl = options.fetchImpl;
    this.windowDays = options.windowDays ?? DEFAULT_WINDOW_DAYS;
    this.now = options.now ?? (() => new Date());
  }

  /** The filter currently applied (the constructor's `filter`, merged with
   * every `refresh(filter)` call since). */
  getFilter(): HistoryQueryFilter {
    return this.filter;
  }

  /** The filter each API query is issued with: the applied filter plus the
   * rolling-window `since` (when the caller set none) and the page-size
   * cap. `kind` is added per query. */
  effectiveFilter(): HistoryQueryFilter {
    const since =
      this.filter.since ?? new Date(this.now().getTime() - this.windowDays * 24 * 60 * 60 * 1000).toISOString();
    return { ...this.filter, since, limit: CHART_PAGE_SIZE };
  }

  /**
   * Fetch the sweep records for the current filter — merged with `filter`
   * when provided — and re-render all three charts from them.
   */
  async refresh(filter?: HistoryQueryFilter): Promise<void> {
    if (filter) this.filter = { ...this.filter, ...filter };

    this.renderNote(this.outcomesContainer, "Loading sweep history…", "charts-loading");
    this.successRateContainer.replaceChildren();
    this.durationsContainer.replaceChildren();

    const base = this.effectiveFilter();
    const pages = await Promise.all(
      CHART_RECORD_KINDS.map((kind) =>
        fetchAllHistory(this.basePath, { ...base, kind }, { fetchImpl: this.fetchImpl }),
      ),
    );
    const records: HistoryRecord[] = pages.flat();

    if (records.length === 0) {
      this.records = [];
      this.renderNote(
        this.outcomesContainer,
        `No completed sweeps ${this.filter.since ? "in the selected range" : `in the last ${this.windowDays} days`}.`,
        "charts-empty",
      );
      return;
    }

    this.records = records;
    this.renderCharts();
    this.observeResize();
  }

  /** Draw all three charts from the cached records at the containers'
   * current width. Safe to call again on resize — each view clears and
   * re-renders its container. */
  private renderCharts(): void {
    const granularityLabel = this.granularity === "weekly" ? "week" : "day";
    const buckets = buildOutcomesOverTime(this.records, this.granularity);
    renderOutcomesChart(this.outcomesContainer, buckets, { granularityLabel });
    renderSuccessRateChart(this.successRateContainer, buildSuccessRateTrend(buckets), { granularityLabel });
    renderDurationPercentilesChart(this.durationsContainer, buildDurationPercentiles(this.records));
    this.lastWidth = this.outcomesContainer.clientWidth;
  }

  /** Re-render on a container width change (window resize, sidebar toggle).
   * Height changes and the observer's own initial callback are ignored via
   * the width comparison, so this never loops on its own render. */
  private observeResize(): void {
    if (this.resizeObserver || typeof ResizeObserver === "undefined") return;
    this.resizeObserver = new ResizeObserver(() => {
      if (this.records.length === 0) return;
      const width = this.outcomesContainer.clientWidth;
      if (width > 0 && width !== this.lastWidth) this.renderCharts();
    });
    this.resizeObserver.observe(this.outcomesContainer);
  }

  /** Release the resize observer. Panels are torn down on every route change
   * (see `panels.ts`), so this is the mount's teardown. */
  dispose(): void {
    this.resizeObserver?.disconnect();
    this.resizeObserver = undefined;
  }

  private renderNote(container: HTMLElement, text: string, testid: string): void {
    container.replaceChildren(el("p", { class: "panel-route__note", data: { testid } }, text));
  }
}
