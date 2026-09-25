/**
 * Duration percentile chart view (issue #4751, AC3; readable chrome #8546):
 * renders `DurationPercentiles` (overall + per-phase p50/p90/p99) as a
 * horizontal grouped-bar chart — one row per series (`overall` first, when
 * present, then each phase present in `byPhase`), one bar per requested
 * percentile rank within the row — with a duration axis, a legend naming the
 * ranks, a p50 direct label per row, per-bar tooltips, and a table view.
 *
 * Row labels are HTML-sized text in the SVG at a fixed pixel size (the chart
 * renders at pixel width, never scaled), so they no longer grow with the
 * viewport.
 */

import { formatDuration } from "../format.js";
import type { DurationPercentiles, PercentileRank, PercentileResult } from "./durations.js";
import { DEFAULT_PERCENTILES } from "./durations.js";
import {
  CHROME,
  createTooltip,
  drawXValueAxis,
  localPoint,
  measureWidth,
  niceDurationTicks,
  renderChartHeader,
  renderLegend,
  renderTableView,
  roundedRightRect,
  svgEl,
  type Margins,
} from "./chartChrome.js";

/** Stable per-rank color: p50 → p99 is an *ordered* scale, so one hue stepped
 * light → dark (validated as an ordinal ramp on the dark surface), never
 * three unrelated colors. Also stamped as `data-rank` on each bar so a
 * stylesheet can restyle without touching this module. */
export const PERCENTILE_COLORS: Record<PercentileRank, string> = {
  50: "#9ec5f4",
  90: "#5598e7",
  99: "#256abf",
};

export interface DurationsChartOptions {
  width?: number;
  /** Vertical space per row (all ranks + air), px. */
  rowHeight?: number;
  /** Space reserved for the row label, px. */
  labelWidth?: number;
  ranks?: readonly PercentileRank[];
}

const DEFAULTS = { width: 640, rowHeight: 44, labelWidth: 100 };
const MARGINS: Margins = { top: 8, right: 56, bottom: 26, left: 0 };
const BAR_GAP = 2;
const MAX_BAR_THICKNESS = 10;
const DATA_END_RADIUS = 4;
const DIRECT_LABEL_RANK: PercentileRank = 50;

interface Row {
  label: string;
  percentiles: PercentileResult;
}

/**
 * Render `data` into `container`. Clears any prior content on each call. A
 * `data` with no `overall` and an empty `byPhase` (no sweep in range has a
 * known duration) still renders a (contentless) `<svg>` — callers decide
 * whether to show a "no data" message around it.
 */
export function renderDurationPercentilesChart(
  container: HTMLElement,
  data: DurationPercentiles,
  options: DurationsChartOptions = {},
): void {
  const width = options.width ?? measureWidth(container, DEFAULTS.width);
  const rowHeight = options.rowHeight ?? DEFAULTS.rowHeight;
  const labelWidth = options.labelWidth ?? DEFAULTS.labelWidth;
  const ranks = options.ranks ?? DEFAULT_PERCENTILES;
  container.innerHTML = "";

  const rows: Row[] = [];
  if (data.overall) rows.push({ label: "overall", percentiles: data.overall });
  for (const [phase, percentiles] of Object.entries(data.byPhase)) {
    rows.push({ label: phase, percentiles });
  }

  const plotHeight = Math.max(rows.length, 1) * rowHeight;
  const height = MARGINS.top + plotHeight + (rows.length === 0 ? 0 : MARGINS.bottom);
  const svg = svgEl("svg");
  svg.setAttribute("class", "durations-chart chart-svg");
  svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
  svg.setAttribute("width", String(width));
  svg.setAttribute("height", String(height));
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", "Sweep duration percentiles");

  if (rows.length === 0) {
    container.appendChild(svg);
    return;
  }

  renderChartHeader(
    container,
    "Sweep duration by phase",
    `p50 / p90 / p99 of wall-clock duration over the sweeps in range — half of sweeps finish within p50, 99 in 100 within p99.`,
  );
  container.appendChild(svg);

  const plotLeft = labelWidth;
  const plotWidth = width - labelWidth - MARGINS.right;
  const maxValue = Math.max(0, ...rows.flatMap((row) => ranks.map((rank) => row.percentiles[rank] ?? 0)));
  const ticks = niceDurationTicks(maxValue, 4);
  const domainMax = ticks[ticks.length - 1]!;
  const xFor = (seconds: number): number => plotLeft + (seconds / domainMax) * plotWidth;
  const thickness = Math.min(MAX_BAR_THICKNESS, (rowHeight - 12) / ranks.length - BAR_GAP);
  const groupHeight = ranks.length * thickness + (ranks.length - 1) * BAR_GAP;

  drawXValueAxis(svg, ticks, xFor, { ...MARGINS, left: plotLeft }, plotWidth, plotHeight, (value) => formatDuration(value));

  const tooltip = createTooltip(container);

  rows.forEach((row, rowIndex) => {
    const rowTop = MARGINS.top + rowIndex * rowHeight;
    const groupTop = rowTop + (rowHeight - groupHeight) / 2;

    const label = svgEl("text");
    label.setAttribute("x", String(plotLeft - 8));
    label.setAttribute("y", String(rowTop + rowHeight / 2));
    label.setAttribute("text-anchor", "end");
    label.setAttribute("dominant-baseline", "middle");
    label.setAttribute("font-size", "12");
    label.setAttribute("class", "durations-chart__row-label");
    label.setAttribute("style", "fill: var(--text)");
    label.dataset.row = row.label;
    label.textContent = row.label;
    svg.appendChild(label);

    ranks.forEach((rank, rankIndex) => {
      const value = row.percentiles[rank];
      if (value === undefined) return;

      const y = groupTop + rankIndex * (thickness + BAR_GAP);
      const barWidth = Math.max((value / domainMax) * plotWidth, 0);
      const rect = svgEl("rect");
      rect.setAttribute("x", String(plotLeft));
      rect.setAttribute("y", String(y));
      rect.setAttribute("width", String(barWidth));
      rect.setAttribute("height", String(thickness));
      rect.setAttribute("fill", PERCENTILE_COLORS[rank]);
      rect.dataset.row = row.label;
      rect.dataset.rank = String(rank);
      rect.dataset.valueSec = String(value);
      if (barWidth > DATA_END_RADIUS) {
        const clipId = `durations-cap-${rowIndex}-${rank}`;
        const clip = svgEl("clipPath");
        clip.setAttribute("id", clipId);
        const capPath = svgEl("path");
        capPath.setAttribute("d", roundedRightRect(plotLeft, y, barWidth, thickness, DATA_END_RADIUS));
        clip.appendChild(capPath);
        svg.appendChild(clip);
        rect.setAttribute("clip-path", `url(#${clipId})`);
      }
      svg.appendChild(rect);

      if (rank === DIRECT_LABEL_RANK) {
        // One direct label per row — the typical (p50) duration at the bar
        // tip; the axis, legend and tooltip carry the rest.
        const tip = svgEl("text");
        tip.setAttribute("x", String(plotLeft + barWidth + 6));
        tip.setAttribute("y", String(y + thickness / 2));
        tip.setAttribute("dominant-baseline", "middle");
        tip.setAttribute("font-size", String(CHROME.fontSize));
        tip.setAttribute("class", "durations-chart__tip-label");
        tip.setAttribute("style", `fill: ${CHROME.axisText}`);
        tip.textContent = formatDuration(value);
        svg.appendChild(tip);
      }

      // Hit target: the bar plus its gap, at least 24px tall and never
      // narrower than 24px, as a <path> (rect-counting callers see bars only).
      const hitHeight = Math.max(thickness + BAR_GAP, 24 / ranks.length);
      const hitWidth = Math.max(barWidth, 24);
      const hit = svgEl("path");
      hit.setAttribute("class", "chart-hit");
      hit.setAttribute("d", `M${plotLeft},${y + thickness / 2 - hitHeight / 2} h${hitWidth} v${hitHeight} h${-hitWidth} Z`);
      hit.setAttribute("fill", "transparent");
      hit.setAttribute("tabindex", "0");
      hit.setAttribute("role", "img");
      hit.setAttribute("aria-label", `${row.label} p${rank}: ${formatDuration(value)}`);
      const tooltipRows = [
        { label: "", value: row.label, emphasis: true },
        ...ranks
          .filter((r) => row.percentiles[r] !== undefined)
          .map((r) => ({
            label: `p${r}`,
            value: formatDuration(row.percentiles[r]),
            color: PERCENTILE_COLORS[r],
            emphasis: r === rank,
          })),
      ];
      const show = (): void => {
        rect.classList.add("is-hovered");
        tooltip.show(tooltipRows, plotLeft + barWidth, y);
      };
      const hide = (): void => {
        rect.classList.remove("is-hovered");
        tooltip.hide();
      };
      hit.addEventListener("pointerenter", show);
      hit.addEventListener("pointermove", (event) => {
        rect.classList.add("is-hovered");
        const point = localPoint(container, event);
        tooltip.show(tooltipRows, point.x, point.y);
      });
      hit.addEventListener("pointerleave", hide);
      hit.addEventListener("focus", show);
      hit.addEventListener("blur", hide);
      svg.appendChild(hit);
    });
  });

  renderLegend(
    container,
    ranks.map((rank) => ({ label: `p${rank}`, color: PERCENTILE_COLORS[rank], shape: "rect" })),
  );
  renderTableView(
    container,
    "Sweep duration percentiles",
    ["Phase", ...ranks.map((rank) => `p${rank}`)],
    rows.map((row) => [row.label, ...ranks.map((rank) => formatDuration(row.percentiles[rank]))]),
  );
}
