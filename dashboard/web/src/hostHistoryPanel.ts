/**
 * Per-host history panel (issue #5355): a selectable-window (24h/7d/30d,
 * default 24h) set of trend charts on the host detail drill-down — CPU idle,
 * worktree-root free GB, and sweep throughput split by result.
 *
 * **Self-mounted, like the Phase-3 panel routes** (`panels.ts`), not a
 * projection of the fleet snapshot the app polls: it owns its own
 * `/api/history` (or `/public/history`) fetch, keyed by `hostId` + the
 * selected window, and re-fetches only on construction or a window change —
 * never on the fleet's 10s poll tick. `app.ts` is responsible for
 * constructing exactly one instance per `hostId` and reusing it across poll
 * ticks (see that module's `ensureHostHistoryContainer`) so this panel's
 * fetch/render cost is paid once per navigation, not once per poll.
 *
 * No schema change and no new daemon field were needed for this issue — see
 * issue #5355's "What already exists" section: the history table, the read
 * API's `host`/`since`/`limit` filters + `nextCursor` pagination, and the
 * chart infrastructure (`charts/outcomes.ts` for throughput,
 * `charts/hostHealthTrend.ts` for the two health metrics) all already
 * existed or are minimal additions on top of what did.
 */

import { el } from "./dom.js";
import type { FetchLike, HistoryQueryFilter } from "./historyClient.js";
import { fetchAllHistory } from "./historyClient.js";
import type { BucketGranularity } from "./charts/timeBuckets.js";
import { buildOutcomesOverTime } from "./charts/outcomes.js";
import { renderOutcomesChart } from "./charts/outcomesChartView.js";
import { buildHealthMetricTrend } from "./charts/hostHealthTrend.js";
import { renderMetricTrendChart } from "./charts/hostHealthTrendChartView.js";
import { formatGigabytes, formatPercent } from "./format.js";

export type HostHistoryWindow = "24h" | "7d" | "30d";

export const HOST_HISTORY_WINDOWS: readonly HostHistoryWindow[] = ["24h", "7d", "30d"] as const;

export const DEFAULT_HOST_HISTORY_WINDOW: HostHistoryWindow = "24h";

const WINDOW_HOURS: Readonly<Record<HostHistoryWindow, number>> = {
  "24h": 24,
  "7d": 24 * 7,
  "30d": 24 * 30,
};

/** 24h/7d windows bucket the throughput chart daily; 30d buckets weekly so a
 * month of daily bars does not overcrowd the chart width. */
function granularityForWindow(window: HostHistoryWindow): BucketGranularity {
  return window === "30d" ? "weekly" : "daily";
}

export interface HostHistoryPanelOptions {
  /** `/api/history` or `/public/history` — see `panels.ts`'s
   * `historyBasePath()`. */
  basePath: string;
  hostId: string;
  container: HTMLElement;
  window?: HostHistoryWindow;
  fetchImpl?: FetchLike;
  now?: () => Date;
}

export class HostHistoryPanel {
  private readonly basePath: string;
  private readonly hostId: string;
  private readonly fetchImpl: FetchLike | undefined;
  private readonly now: () => Date;
  private window: HostHistoryWindow;

  private readonly cpuContainer: HTMLElement;
  private readonly diskContainer: HTMLElement;
  private readonly throughputContainer: HTMLElement;
  private readonly emptyStateEl: HTMLElement;
  private readonly chartsEl: HTMLElement;
  private readonly windowButtons = new Map<HostHistoryWindow, HTMLButtonElement>();

  constructor(options: HostHistoryPanelOptions) {
    this.basePath = options.basePath;
    this.hostId = options.hostId;
    this.fetchImpl = options.fetchImpl;
    this.now = options.now ?? (() => new Date());
    this.window = options.window ?? DEFAULT_HOST_HISTORY_WINDOW;

    this.cpuContainer = el("div", { class: "chart-slot", data: { testid: "history-chart-cpu" } });
    this.diskContainer = el("div", { class: "chart-slot", data: { testid: "history-chart-disk" } });
    this.throughputContainer = el("div", { class: "chart-slot", data: { testid: "history-chart-throughput" } });

    this.emptyStateEl = el(
      "p",
      { class: "panel__notice", data: { testid: "history-empty" } },
      "This host has no host.health history in the selected window — nothing to chart yet.",
    );
    this.emptyStateEl.hidden = true;

    this.chartsEl = el(
      "div",
      { class: "history-panel__charts" },
      el(
        "div",
        {},
        el("h3", { class: "history-panel__chart-title" }, "CPU idle"),
        this.cpuContainer,
      ),
      el(
        "div",
        {},
        el("h3", { class: "history-panel__chart-title" }, "Worktree root free"),
        this.diskContainer,
      ),
      el(
        "div",
        {},
        el("h3", { class: "history-panel__chart-title" }, "Sweep throughput"),
        this.throughputContainer,
      ),
    );

    const windowRow = el(
      "div",
      { class: "history-panel__window", data: { testid: "history-window" } },
      ...HOST_HISTORY_WINDOWS.map((candidate) => this.buildWindowButton(candidate)),
    );

    options.container.replaceChildren(
      el(
        "section",
        { class: "panel", data: { testid: "host-history-panel" } },
        el("h2", { class: "panel__title" }, "History", windowRow),
        this.emptyStateEl,
        this.chartsEl,
      ),
    );

    this.updateWindowButtons();
  }

  /** The window currently applied (the constructor's `window`, merged with
   * every `refresh(window)` call since). */
  getWindow(): HostHistoryWindow {
    return this.window;
  }

  private buildWindowButton(window: HostHistoryWindow): HTMLButtonElement {
    const button = el(
      "button",
      {
        class: "history-panel__window-btn",
        type: "button",
        data: { testid: `history-window-${window}`, window },
        aria: { pressed: String(window === this.window) },
        onClick: () => void this.refresh(window),
      },
      window,
    );
    this.windowButtons.set(window, button);
    return button;
  }

  private updateWindowButtons(): void {
    for (const [window, button] of this.windowButtons) {
      const active = window === this.window;
      button.classList.toggle("history-panel__window-btn--active", active);
      button.setAttribute("aria-pressed", String(active));
    }
  }

  /**
   * Fetch the full (paginated) result set for the current window — merged
   * with `window` when provided — and re-render every chart from it.
   */
  async refresh(window?: HostHistoryWindow): Promise<void> {
    if (window) this.window = window;
    this.updateWindowButtons();

    const since = new Date(this.now().getTime() - WINDOW_HOURS[this.window] * 3_600_000).toISOString();
    const filter: HistoryQueryFilter = { host: this.hostId, since, limit: 500 };
    const records = await fetchAllHistory(this.basePath, filter, { fetchImpl: this.fetchImpl });

    const healthRecords = records.filter((record) => record.kind === "host.health");
    const hasHistory = healthRecords.length > 0;

    this.emptyStateEl.hidden = hasHistory;
    this.chartsEl.hidden = !hasHistory;
    if (!hasHistory) return;

    renderMetricTrendChart(this.cpuContainer, buildHealthMetricTrend(healthRecords, "cpu_idle_fraction"), {
      domainMax: 1,
      formatValue: (value) => formatPercent(value, 1),
    });
    renderMetricTrendChart(this.diskContainer, buildHealthMetricTrend(healthRecords, "worktree_root_free_gb"), {
      formatValue: (value) => formatGigabytes(value),
    });
    renderOutcomesChart(this.throughputContainer, buildOutcomesOverTime(records, granularityForWindow(this.window)));
  }
}
