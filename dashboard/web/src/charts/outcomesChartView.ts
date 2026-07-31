/**
 * Outcomes-over-time chart view (issue #4751, AC1): renders `OutcomeBucket[]`
 * as a stacked bar chart, one bar per bucket, segmented by `SweepResult`.
 * Plain SVG rendering, matching `liveFeedPanel.ts` / `sweepTimelineView.ts`'s
 * framework-agnostic DOM approach — no charting library dependency, so it
 * needs no adapter once #4749's frontend scaffold lands (issue #4751's
 * "Dependencies" section calls that scaffold a soft dependency — this view
 * is independently implementable, same as #4750's panel/timeline views were).
 */

import type { SweepResult } from "../types.js";
import type { OutcomeBucket } from "./outcomes.js";
import { SWEEP_RESULTS } from "./outcomes.js";

const SVG_NS = "http://www.w3.org/2000/svg";

/** Stable per-result color. Also stamped as `data-result` on each segment so
 * a stylesheet can restyle without touching this module. */
export const OUTCOME_COLORS: Record<SweepResult, string> = {
  success: "#2e7d32",
  failure: "#c62828",
  cancelled: "#757575",
  blocked: "#ef6c00",
};

export interface OutcomesChartOptions {
  width?: number;
  height?: number;
  barGap?: number;
}

const DEFAULTS: Required<OutcomesChartOptions> = { width: 640, height: 240, barGap: 4 };

function svgEl<K extends keyof SVGElementTagNameMap>(tag: K): SVGElementTagNameMap[K] {
  return document.createElementNS(SVG_NS, tag);
}

/**
 * Render `buckets` into `container` as a stacked bar chart. Clears any prior
 * content on each call (matching `renderSweepTimeline`'s re-render
 * contract). An empty `buckets` array still renders a (contentless) `<svg>`
 * — callers decide whether to show a "no data" message around it.
 */
export function renderOutcomesChart(
  container: HTMLElement,
  buckets: OutcomeBucket[],
  options: OutcomesChartOptions = {},
): void {
  const { width, height, barGap } = { ...DEFAULTS, ...options };
  container.innerHTML = "";

  const svg = svgEl("svg");
  svg.setAttribute("class", "outcomes-chart");
  svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", "Sweep outcomes over time");
  container.appendChild(svg);

  if (buckets.length === 0) return;

  const maxTotal = Math.max(...buckets.map((bucket) => bucket.total), 1);
  const barWidth = Math.max((width - barGap * (buckets.length + 1)) / buckets.length, 0);

  buckets.forEach((bucket, index) => {
    const x = barGap + index * (barWidth + barGap);
    let yCursor = height;

    const group = svgEl("g");
    group.setAttribute("class", "outcomes-chart__bar");
    group.dataset.bucketKey = bucket.bucketKey;
    group.dataset.total = String(bucket.total);
    svg.appendChild(group);

    for (const result of SWEEP_RESULTS) {
      const count = bucket.counts[result];
      if (count === 0) continue;

      const segmentHeight = (count / maxTotal) * height;
      const rect = svgEl("rect");
      rect.setAttribute("x", String(x));
      rect.setAttribute("y", String(yCursor - segmentHeight));
      rect.setAttribute("width", String(barWidth));
      rect.setAttribute("height", String(segmentHeight));
      rect.setAttribute("fill", OUTCOME_COLORS[result]);
      rect.dataset.result = result;
      rect.dataset.count = String(count);
      group.appendChild(rect);

      yCursor -= segmentHeight;
    }
  });
}
