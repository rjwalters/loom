/**
 * Host-health metric trend chart view (issue #5355): renders
 * `HealthTrendPoint[]` (one `host.health` numeric field, e.g. CPU idle or
 * worktree-free GB, one point per record) as a line chart.
 *
 * Mirrors `successRateChartView.ts`'s gap-handling: a `value: null` point
 * (the field was absent on that record — see `hostHealthTrend.ts`'s doc)
 * breaks the polyline into a separate segment rather than being interpolated
 * through, and draws no marker — so an unmeasurable probe never reads as a
 * plunge to zero.
 */

import type { HealthTrendPoint } from "./hostHealthTrend.js";

const SVG_NS = "http://www.w3.org/2000/svg";

export interface MetricTrendChartOptions {
  width?: number;
  height?: number;
  padding?: number;
  /** Fixed upper bound of the y-axis domain — e.g. `1` for a `[0, 1]`
   * fraction such as CPU idle. Omitted for a metric with no natural bound
   * (e.g. free GB): the domain then auto-scales to the observed maximum,
   * floored to a tiny positive number so a series of all-zero points does
   * not divide by zero. */
  domainMax?: number;
  /** Formats a raw value for the point's `data-value` attribute — e.g.
   * `"83%"` or `"180.0 GB"`. Defaults to `String(value)`. */
  formatValue?: (value: number) => string;
}

const DEFAULTS: Required<Pick<MetricTrendChartOptions, "width" | "height" | "padding">> = {
  width: 640,
  height: 160,
  padding: 8,
};

function svgEl<K extends keyof SVGElementTagNameMap>(tag: K): SVGElementTagNameMap[K] {
  return document.createElementNS(SVG_NS, tag);
}

interface PlottedPoint {
  x: number;
  y: number;
}

/**
 * Render `points` into `container` as a line chart. Clears any prior content
 * on each call. An empty `points` array still renders a (contentless)
 * `<svg>` — callers decide whether to show a "no data" message around it.
 */
export function renderMetricTrendChart(
  container: HTMLElement,
  points: HealthTrendPoint[],
  options: MetricTrendChartOptions = {},
): void {
  const { width, height, padding } = { ...DEFAULTS, ...options };
  const formatValue = options.formatValue ?? ((value: number) => String(value));
  container.innerHTML = "";

  const svg = svgEl("svg");
  svg.setAttribute("class", "metric-trend-chart");
  svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", "Host metric trend");
  container.appendChild(svg);

  if (points.length === 0) return;

  const innerWidth = width - padding * 2;
  const innerHeight = height - padding * 2;
  const step = points.length > 1 ? innerWidth / (points.length - 1) : 0;

  const observedMax = Math.max(0, ...points.map((point) => point.value ?? 0));
  const domainMax = options.domainMax ?? Math.max(observedMax, 1e-9);
  const yFor = (value: number): number => padding + innerHeight * (1 - Math.min(1, value / domainMax));

  // Split into contiguous runs of non-null points so the polyline never
  // draws a straight line across a gap point (an absent measurement).
  const segments: PlottedPoint[][] = [];
  let current: PlottedPoint[] = [];
  points.forEach((point, index) => {
    if (point.value === null) {
      if (current.length > 0) segments.push(current);
      current = [];
      return;
    }
    current.push({ x: padding + step * index, y: yFor(point.value) });
  });
  if (current.length > 0) segments.push(current);

  for (const segment of segments) {
    if (segment.length < 2) continue;
    const polyline = svgEl("polyline");
    polyline.setAttribute("points", segment.map((p) => `${p.x},${p.y}`).join(" "));
    polyline.setAttribute("fill", "none");
    polyline.setAttribute("stroke", "#1565c0");
    polyline.setAttribute("stroke-width", "2");
    polyline.setAttribute("class", "metric-trend-chart__line");
    svg.appendChild(polyline);
  }

  // A marker for every non-null point, including isolated ones a polyline
  // segment of length 1 wouldn't otherwise render. No marker at all is drawn
  // for a `null` point — that is the gap.
  points.forEach((point, index) => {
    if (point.value === null) return;
    const x = padding + step * index;
    const y = yFor(point.value);

    const circle = svgEl("circle");
    circle.setAttribute("cx", String(x));
    circle.setAttribute("cy", String(y));
    circle.setAttribute("r", "3");
    circle.setAttribute("fill", "#1565c0");
    circle.setAttribute("class", "metric-trend-chart__point");
    circle.dataset.emittedAt = point.emittedAt;
    circle.dataset.value = formatValue(point.value);
    svg.appendChild(circle);
  });
}
