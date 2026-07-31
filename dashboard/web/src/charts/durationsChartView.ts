/**
 * Duration percentile chart view (issue #4751, AC3): renders
 * `DurationPercentiles` (overall + per-phase p50/p90/p99) as a horizontal
 * grouped-bar chart — one row per series (`overall` first, when present,
 * then each phase present in `byPhase`), one bar per requested percentile
 * rank within the row.
 */

import type { DurationPercentiles, PercentileRank, PercentileResult } from "./durations.js";
import { DEFAULT_PERCENTILES } from "./durations.js";

const SVG_NS = "http://www.w3.org/2000/svg";

/** Stable per-rank color. Also stamped as `data-rank` on each bar so a
 * stylesheet can restyle without touching this module. */
export const PERCENTILE_COLORS: Record<PercentileRank, string> = {
  50: "#1565c0",
  90: "#f9a825",
  99: "#c62828",
};

export interface DurationsChartOptions {
  width?: number;
  rowHeight?: number;
  labelWidth?: number;
  ranks?: readonly PercentileRank[];
}

const DEFAULTS = { width: 640, rowHeight: 28, labelWidth: 100 };

function svgEl<K extends keyof SVGElementTagNameMap>(tag: K): SVGElementTagNameMap[K] {
  return document.createElementNS(SVG_NS, tag);
}

interface Row {
  label: string;
  percentiles: PercentileResult;
}

/**
 * Render `data` into `container` as a horizontal grouped-bar chart. Clears
 * any prior content on each call. A `data` with no `overall` and an empty
 * `byPhase` (no sweep in range has a known duration) still renders a
 * (contentless) `<svg>` — callers decide whether to show a "no data" message
 * around it.
 */
export function renderDurationPercentilesChart(
  container: HTMLElement,
  data: DurationPercentiles,
  options: DurationsChartOptions = {},
): void {
  const { width, rowHeight, labelWidth } = { ...DEFAULTS, ...options };
  const ranks = options.ranks ?? DEFAULT_PERCENTILES;
  container.innerHTML = "";

  const rows: Row[] = [];
  if (data.overall) rows.push({ label: "overall", percentiles: data.overall });
  for (const [phase, percentiles] of Object.entries(data.byPhase)) {
    rows.push({ label: phase, percentiles });
  }

  const height = Math.max(rows.length, 1) * rowHeight;
  const svg = svgEl("svg");
  svg.setAttribute("class", "durations-chart");
  svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", "Sweep duration percentiles");
  container.appendChild(svg);

  if (rows.length === 0) return;

  const maxValue = Math.max(1, ...rows.flatMap((row) => ranks.map((rank) => row.percentiles[rank] ?? 0)));
  const chartWidth = width - labelWidth;
  const rankHeight = rowHeight / ranks.length;

  rows.forEach((row, rowIndex) => {
    const rowY = rowIndex * rowHeight;

    const label = svgEl("text");
    label.setAttribute("x", "4");
    label.setAttribute("y", String(rowY + rowHeight / 2));
    label.setAttribute("dominant-baseline", "middle");
    label.setAttribute("font-size", "11");
    label.dataset.row = row.label;
    label.textContent = row.label;
    svg.appendChild(label);

    ranks.forEach((rank, rankIndex) => {
      const value = row.percentiles[rank];
      if (value === undefined) return;

      const barWidth = (value / maxValue) * chartWidth;
      const rect = svgEl("rect");
      rect.setAttribute("x", String(labelWidth));
      rect.setAttribute("y", String(rowY + rankIndex * rankHeight));
      rect.setAttribute("width", String(barWidth));
      rect.setAttribute("height", String(Math.max(rankHeight - 1, 0)));
      rect.setAttribute("fill", PERCENTILE_COLORS[rank]);
      rect.dataset.row = row.label;
      rect.dataset.rank = String(rank);
      rect.dataset.valueSec = String(value);
      svg.appendChild(rect);
    });
  });
}
