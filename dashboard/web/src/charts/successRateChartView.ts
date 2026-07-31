/**
 * Success-rate trend chart view (issue #4751, AC2): renders
 * `SuccessRatePoint[]` as a line chart. A point with `successRate: null` (no
 * completed sweeps in that bucket) is skipped when drawing the line — per
 * `successRate.ts`'s doc, "a chart should render a gap there rather than a
 * misleading 0" — so the polyline breaks into separate segments around any
 * gap instead of interpolating through it.
 */

import type { SuccessRatePoint } from "./successRate.js";

const SVG_NS = "http://www.w3.org/2000/svg";

export interface SuccessRateChartOptions {
  width?: number;
  height?: number;
  padding?: number;
}

const DEFAULTS: Required<SuccessRateChartOptions> = { width: 640, height: 200, padding: 8 };

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
export function renderSuccessRateChart(
  container: HTMLElement,
  points: SuccessRatePoint[],
  options: SuccessRateChartOptions = {},
): void {
  const { width, height, padding } = { ...DEFAULTS, ...options };
  container.innerHTML = "";

  const svg = svgEl("svg");
  svg.setAttribute("class", "success-rate-chart");
  svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", "Success rate trend");
  container.appendChild(svg);

  if (points.length === 0) return;

  const innerWidth = width - padding * 2;
  const innerHeight = height - padding * 2;
  const step = points.length > 1 ? innerWidth / (points.length - 1) : 0;

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
    current.push({ x: padding + step * index, y: padding + innerHeight * (1 - point.successRate) });
  });
  if (current.length > 0) segments.push(current);

  for (const segment of segments) {
    if (segment.length < 2) continue;
    const polyline = svgEl("polyline");
    polyline.setAttribute("points", segment.map((p) => `${p.x},${p.y}`).join(" "));
    polyline.setAttribute("fill", "none");
    polyline.setAttribute("stroke", "#1565c0");
    polyline.setAttribute("stroke-width", "2");
    polyline.setAttribute("class", "success-rate-chart__line");
    svg.appendChild(polyline);
  }

  // A marker for every non-null bucket, including isolated ones a polyline
  // segment of length 1 wouldn't otherwise render.
  points.forEach((point, index) => {
    if (point.successRate === null) return;
    const x = padding + step * index;
    const y = padding + innerHeight * (1 - point.successRate);

    const circle = svgEl("circle");
    circle.setAttribute("cx", String(x));
    circle.setAttribute("cy", String(y));
    circle.setAttribute("r", "3");
    circle.setAttribute("fill", "#1565c0");
    circle.setAttribute("class", "success-rate-chart__point");
    circle.dataset.bucketKey = point.bucketKey;
    circle.dataset.successRate = String(point.successRate);
    svg.appendChild(circle);
  });
}
