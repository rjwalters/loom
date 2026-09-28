/**
 * Token-pool burn-rate chart view (issue #9029): renders `BurnRatePoint[]`
 * (`burnRate.ts`) as a first-class interactive chart — dated x-axis, a
 * zero-centered %/h y-axis, a legend for the two series (mean, peak), a
 * hover/focus tooltip, and a `renderTableView` fallback — using the same
 * `charts/chartChrome.ts` chrome `charts/successRateChartView.ts` does. This
 * supersedes the old bare decorative sparkline (`render.ts`'s
 * `renderPoolSparkline`) as the pattern any new analytics chart should
 * follow; that sparkline itself is left as-is (out of scope for #9029 — see
 * the issue's "Explicitly out of scope" section).
 *
 * Two series, both drawn as line + markers with independent null-gaps
 * (mirroring `successRateChartView.ts`'s gap handling): a limit-window
 * rollover or an unmeasured endpoint reports `null` in `burnRate.ts`, which
 * breaks the polyline there rather than interpolating through it or drawing
 * a fabricated cliff.
 */

import { formatInstant, formatRatePerHour } from "./format.js";
import type { BurnRatePoint } from "./burnRate.js";
import {
  CHROME,
  createTooltip,
  drawXAxis,
  drawYAxis,
  localPoint,
  measureWidth,
  niceTicks,
  renderChartHeader,
  renderLegend,
  renderTableView,
  svgEl,
  type Margins,
} from "../charts/chartChrome.js";

/** Mean-usage rate: the same brand accent a single-series chart wears
 * (`successRateChartView.ts`'s `SUCCESS_RATE_COLOR`). */
export const MEAN_RATE_COLOR = "#6ea8fe";
/** Peak-usage rate: a distinct hue — the "how close is the busiest account"
 * reading `forecast.ts`'s `PoolHealthSummary` treats as the one that
 * matters most, so it gets the visually distinct, attention-carrying color. */
export const MAX_RATE_COLOR = "#f2994a";

export interface BurnRateChartOptions {
  width?: number;
  /** Plot height in px (the axis band is added on top of this). */
  height?: number;
  /** Shown in the chart title — typically the `hostId` this series covers. */
  title?: string;
}

const DEFAULTS = { width: 640, height: 180 };
const MARGINS: Margins = { top: 8, right: 8, bottom: 26, left: 48 };
const MARKER_RADIUS = 3.5;

interface PlottedPoint {
  x: number;
  y: number;
}

interface SeriesSpec {
  key: "meanRatePerHour" | "maxRatePerHour";
  /** Short, CSS-class-safe identifier — `key` is a field name, this is what
   * ends up in `data-series` and the `__line--*`/`__point--*` modifiers. */
  slug: "mean" | "peak";
  label: string;
  color: string;
}

const SERIES: readonly SeriesSpec[] = [
  { key: "meanRatePerHour", slug: "mean", label: "Mean", color: MEAN_RATE_COLOR },
  { key: "maxRatePerHour", slug: "peak", label: "Peak", color: MAX_RATE_COLOR },
];

/**
 * Render `points` into `container`. Clears any prior content on each call. An
 * empty `points` array still renders a (contentless) `<svg>` — callers decide
 * whether to show a "no data" message around it.
 */
export function renderBurnRateChart(
  container: HTMLElement,
  points: BurnRatePoint[],
  options: BurnRateChartOptions = {},
): void {
  const width = options.width ?? measureWidth(container, DEFAULTS.width);
  const plotHeight = options.height ?? DEFAULTS.height;
  const height = MARGINS.top + plotHeight + MARGINS.bottom;
  container.innerHTML = "";

  const svg = svgEl("svg");
  svg.setAttribute("class", "burn-rate-chart chart-svg");
  svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
  svg.setAttribute("width", String(width));
  svg.setAttribute("height", String(height));
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", "Token-pool burn rate");

  if (points.length === 0) {
    container.appendChild(svg);
    return;
  }

  renderChartHeader(
    container,
    options.title ?? "Token-pool burn rate",
    "Rolling trailing-window rate of change of pool usage, in % of pool capacity per hour. " +
      "A gap marks a limit-window reset or a telemetry outage, never an interpolated or fabricated value.",
  );
  container.appendChild(svg);

  const plotWidth = width - MARGINS.left - MARGINS.right;
  const step = points.length > 1 ? plotWidth / (points.length - 1) : 0;
  const xFor = (index: number): number =>
    points.length > 1 ? MARGINS.left + step * index : MARGINS.left + plotWidth / 2;

  // A zero-centered domain: rates are usually >= 0 (usage climbs within a
  // segment), but small negative wobble is real, legitimate data (e.g. the
  // pool's account roster changing between samples), never a bug to clip
  // away. Symmetric ticks keep 0 visually anchored at mid-height.
  const values = points.flatMap((point) => [point.meanRatePerHour, point.maxRatePerHour]).filter(isNumber);
  const maxAbs = Math.max(1e-6, ...values.map((value) => Math.abs(value)));
  const positiveTicks = niceTicks(maxAbs, 3);
  const domainMax = positiveTicks[positiveTicks.length - 1]!;
  const domainMin = -domainMax;
  const ticks = [...positiveTicks.slice(1).map((tick) => -tick).reverse(), ...positiveTicks];
  const yFor = (rate: number): number => MARGINS.top + plotHeight * (1 - (rate - domainMin) / (domainMax - domainMin));

  drawYAxis(svg, ticks, yFor, MARGINS, plotWidth, (value) => formatRatePerHour(value));
  drawXAxis(svg, points.map((point) => formatInstant(point.at)), xFor, MARGINS, plotWidth, plotHeight);

  for (const series of SERIES) {
    drawSeriesLine(svg, points, series, xFor, yFor);
  }

  // Crosshair + one hover layer over the whole plot, mirroring
  // `successRateChartView.ts`: the reader aims at an instant, not at a 2px line.
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
  hover.setAttribute("aria-label", "Burn rate by sample; use the table view for values");

  const showIndex = (index: number, anchor?: { x: number; y: number }): void => {
    const point = points[index]!;
    const x = xFor(index);
    crosshair.setAttribute("x1", String(x));
    crosshair.setAttribute("x2", String(x));
    crosshair.setAttribute("visibility", "visible");
    tooltip.show(
      [
        { label: "", value: formatInstant(point.at), emphasis: true },
        { label: "mean rate", value: formatRatePerHour(point.meanRatePerHour ?? undefined), color: MEAN_RATE_COLOR },
        { label: "peak rate", value: formatRatePerHour(point.maxRatePerHour ?? undefined), color: MAX_RATE_COLOR },
      ],
      anchor?.x ?? x,
      anchor?.y ?? MARGINS.top + plotHeight / 2,
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

  renderLegend(
    container,
    SERIES.map((series) => ({ label: series.label, color: series.color, shape: "line" })),
  );
  renderTableView(
    container,
    "Burn rate per sample",
    ["Time", "Mean rate", "Peak rate"],
    points.map((point) => [
      formatInstant(point.at),
      formatRatePerHour(point.meanRatePerHour ?? undefined),
      formatRatePerHour(point.maxRatePerHour ?? undefined),
    ]),
  );
}

function isNumber(value: number | null): value is number {
  return value !== null;
}

function drawSeriesLine(
  svg: SVGSVGElement,
  points: readonly BurnRatePoint[],
  series: SeriesSpec,
  xFor: (index: number) => number,
  yFor: (rate: number) => number,
): void {
  // Split into contiguous runs of non-null values so the polyline never draws
  // a straight line across a gap (a rollover, a telemetry outage, or an
  // unmeasured endpoint).
  const segments: PlottedPoint[][] = [];
  let current: PlottedPoint[] = [];
  points.forEach((point, index) => {
    const value = point[series.key];
    if (value === null) {
      if (current.length > 0) segments.push(current);
      current = [];
      return;
    }
    current.push({ x: xFor(index), y: yFor(value) });
  });
  if (current.length > 0) segments.push(current);

  for (const segment of segments) {
    if (segment.length < 2) continue;
    const polyline = svgEl("polyline");
    polyline.setAttribute("points", segment.map((p) => `${p.x},${p.y}`).join(" "));
    polyline.setAttribute("fill", "none");
    polyline.setAttribute("stroke", series.color);
    polyline.setAttribute("stroke-width", "2");
    polyline.setAttribute("stroke-linejoin", "round");
    polyline.setAttribute("stroke-linecap", "round");
    polyline.setAttribute("class", `burn-rate-chart__line burn-rate-chart__line--${series.slug}`);
    svg.appendChild(polyline);
  }

  points.forEach((point, index) => {
    const value = point[series.key];
    if (value === null) return;
    const circle = svgEl("circle");
    circle.setAttribute("cx", String(xFor(index)));
    circle.setAttribute("cy", String(yFor(value)));
    circle.setAttribute("r", String(MARKER_RADIUS));
    circle.setAttribute("fill", series.color);
    circle.setAttribute("style", `stroke: ${CHROME.surface}; stroke-width: 1.5`);
    circle.setAttribute("class", `burn-rate-chart__point burn-rate-chart__point--${series.slug}`);
    circle.dataset.series = series.slug;
    circle.dataset.at = String(point.at);
    circle.dataset.value = String(value);
    svg.appendChild(circle);
  });
}
