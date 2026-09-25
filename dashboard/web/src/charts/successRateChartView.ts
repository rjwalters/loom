/**
 * Success-rate trend chart view (issue #4751, AC2; readable chrome #8546):
 * renders `SuccessRatePoint[]` as a line chart with a 0–100 % axis, dated
 * x-axis, crosshair + nearest-bucket tooltip, and a table view. A point with
 * `successRate: null` (no completed sweeps in that bucket) is skipped when
 * drawing the line — per `successRate.ts`'s doc, "a chart should render a
 * gap there rather than a misleading 0" — so the polyline breaks into
 * separate segments around any gap instead of interpolating through it.
 *
 * Single series, so no legend: the title names what is plotted.
 */

import { formatCount, formatPercent } from "../format.js";
import type { SuccessRatePoint } from "./successRate.js";
import {
  CHROME,
  createTooltip,
  drawXAxis,
  drawYAxis,
  formatBucketLabel,
  localPoint,
  measureWidth,
  renderChartHeader,
  renderTableView,
  svgEl,
  type Margins,
} from "./chartChrome.js";

/** The dashboard's accent: a single series wears the brand hue. */
export const SUCCESS_RATE_COLOR = "#6ea8fe";

export interface SuccessRateChartOptions {
  width?: number;
  /** Plot height in px (the axis band is added on top of this). */
  height?: number;
  granularityLabel?: string;
}

const DEFAULTS = { width: 640, height: 180 };
const MARGINS: Margins = { top: 8, right: 8, bottom: 26, left: 44 };
const MARKER_RADIUS = 4;
const RATE_TICKS = [0, 0.25, 0.5, 0.75, 1];

interface PlottedPoint {
  x: number;
  y: number;
}

/**
 * Render `points` into `container`. Clears any prior content on each call.
 * An empty `points` array still renders a (contentless) `<svg>` — callers
 * decide whether to show a "no data" message around it.
 */
export function renderSuccessRateChart(
  container: HTMLElement,
  points: SuccessRatePoint[],
  options: SuccessRateChartOptions = {},
): void {
  const width = options.width ?? measureWidth(container, DEFAULTS.width);
  const plotHeight = options.height ?? DEFAULTS.height;
  const height = MARGINS.top + plotHeight + MARGINS.bottom;
  container.innerHTML = "";

  const svg = svgEl("svg");
  svg.setAttribute("class", "success-rate-chart chart-svg");
  svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
  svg.setAttribute("width", String(width));
  svg.setAttribute("height", String(height));
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", "Success rate trend");

  if (points.length === 0) {
    container.appendChild(svg);
    return;
  }

  renderChartHeader(
    container,
    "Success rate",
    `Share of completed sweeps that succeeded, per ${options.granularityLabel ?? "day"}. A gap is a ${options.granularityLabel ?? "day"} with no completed sweeps.`,
  );
  container.appendChild(svg);

  const plotWidth = width - MARGINS.left - MARGINS.right;
  const step = points.length > 1 ? plotWidth / (points.length - 1) : 0;
  const xFor = (index: number): number => (points.length > 1 ? MARGINS.left + step * index : MARGINS.left + plotWidth / 2);
  const yFor = (rate: number): number => MARGINS.top + plotHeight * (1 - rate);

  drawYAxis(svg, RATE_TICKS, yFor, MARGINS, plotWidth, (value) => formatPercent(value));
  drawXAxis(svg, points.map((point) => formatBucketLabel(point.bucketKey)), xFor, MARGINS, plotWidth, plotHeight);

  // Split into contiguous runs of non-null points so the polyline never
  // draws a straight line across a gap bucket.
  const segments: PlottedPoint[][] = [];
  let current: PlottedPoint[] = [];
  points.forEach((point, index) => {
    if (point.successRate === null) {
      if (current.length > 0) segments.push(current);
      current = [];
      return;
    }
    current.push({ x: xFor(index), y: yFor(point.successRate) });
  });
  if (current.length > 0) segments.push(current);

  for (const segment of segments) {
    if (segment.length < 2) continue;
    const polyline = svgEl("polyline");
    polyline.setAttribute("points", segment.map((p) => `${p.x},${p.y}`).join(" "));
    polyline.setAttribute("fill", "none");
    polyline.setAttribute("stroke", SUCCESS_RATE_COLOR);
    polyline.setAttribute("stroke-width", "2");
    polyline.setAttribute("stroke-linejoin", "round");
    polyline.setAttribute("stroke-linecap", "round");
    polyline.setAttribute("class", "success-rate-chart__line");
    svg.appendChild(polyline);
  }

  // A marker for every non-null bucket, including isolated ones a polyline
  // segment of length 1 wouldn't otherwise render. The 2px surface ring keeps
  // it legible where it sits on the line.
  points.forEach((point, index) => {
    if (point.successRate === null) return;
    const circle = svgEl("circle");
    circle.setAttribute("cx", String(xFor(index)));
    circle.setAttribute("cy", String(yFor(point.successRate)));
    circle.setAttribute("r", String(MARKER_RADIUS));
    circle.setAttribute("fill", SUCCESS_RATE_COLOR);
    circle.setAttribute("style", `stroke: ${CHROME.surface}; stroke-width: 2`);
    circle.setAttribute("class", "success-rate-chart__point");
    circle.dataset.bucketKey = point.bucketKey;
    circle.dataset.successRate = String(point.successRate);
    svg.appendChild(circle);
  });

  // Crosshair + one hover layer over the whole plot: the reader aims at a
  // date, never at the 2px line.
  const crosshair = svgEl("line");
  crosshair.setAttribute("class", "chart-crosshair");
  crosshair.setAttribute("y1", String(MARGINS.top));
  crosshair.setAttribute("y2", String(MARGINS.top + plotHeight));
  crosshair.setAttribute("style", `stroke: ${CHROME.axisText}; stroke-width: 1`);
  crosshair.setAttribute("visibility", "hidden");
  svg.appendChild(crosshair);

  const tooltip = createTooltip(container);
  const hover = svgEl("path");
  hover.setAttribute("class", "chart-hit");
  hover.setAttribute("d", `M${MARGINS.left},${MARGINS.top} h${plotWidth} v${plotHeight} h${-plotWidth} Z`);
  hover.setAttribute("fill", "transparent");
  hover.setAttribute("tabindex", "0");
  hover.setAttribute("aria-label", "Success rate by bucket; use the table view for values");

  const showIndex = (index: number, anchor?: { x: number; y: number }): void => {
    const point = points[index]!;
    const x = xFor(index);
    crosshair.setAttribute("x1", String(x));
    crosshair.setAttribute("x2", String(x));
    crosshair.setAttribute("visibility", "visible");
    tooltip.show(
      [
        { label: "", value: formatBucketLabel(point.bucketKey), emphasis: true },
        {
          label: "success rate",
          value: point.successRate === null ? "no completed sweeps" : formatPercent(point.successRate),
          color: SUCCESS_RATE_COLOR,
        },
        { label: "completed sweeps", value: formatCount(point.total) },
      ],
      anchor?.x ?? x,
      anchor?.y ?? (point.successRate === null ? MARGINS.top + plotHeight / 2 : yFor(point.successRate)),
    );
  };
  const hide = (): void => {
    crosshair.setAttribute("visibility", "hidden");
    tooltip.hide();
  };
  hover.addEventListener("pointermove", (event) => {
    const local = localPoint(container, event);
    const svgLeft = svg.getBoundingClientRect().left - container.getBoundingClientRect().left;
    const xInSvg = local.x - svgLeft;
    const index = step > 0 ? Math.round((xInSvg - MARGINS.left) / step) : 0;
    showIndex(Math.min(points.length - 1, Math.max(0, index)), local);
  });
  hover.addEventListener("pointerleave", hide);
  hover.addEventListener("focus", () => showIndex(points.length - 1));
  hover.addEventListener("blur", hide);
  svg.appendChild(hover);

  renderTableView(
    container,
    "Success rate per bucket",
    ["Date", "Success rate", "Completed sweeps"],
    points.map((point) => [
      formatBucketLabel(point.bucketKey),
      point.successRate === null ? "—" : formatPercent(point.successRate),
      formatCount(point.total),
    ]),
  );
}
