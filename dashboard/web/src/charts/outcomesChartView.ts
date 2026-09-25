/**
 * Outcomes-over-time chart view (issue #4751, AC1; readable chrome #8546):
 * renders `OutcomeBucket[]` as a stacked column chart, one column per bucket,
 * segmented by `SweepResult`, with a count axis, dated x-axis, legend,
 * per-column tooltip and a table view. Plain SVG + DOM, no charting library.
 *
 * Stack order is success (base) → blocked → cancelled → failure (top): the
 * "good" and "bad" ends are never adjacent, which is what lets the status
 * colors clear the colorblind separation check as a stack. The legend
 * follows the same order so it reads like the column does.
 */

import { formatCount } from "../format.js";
import type { SweepResult } from "../types.js";
import type { OutcomeBucket } from "./outcomes.js";
import {
  createTooltip,
  drawXAxis,
  drawYAxis,
  formatBucketLabel,
  localPoint,
  measureWidth,
  niceTicks,
  renderChartHeader,
  renderLegend,
  renderTableView,
  roundedTopRect,
  svgEl,
  type Margins,
} from "./chartChrome.js";

/** Stable per-result color: the status palette (good / warning / neutral /
 * critical), never a categorical slot. Also stamped as `data-result` on each
 * segment so a stylesheet can restyle without touching this module. */
export const OUTCOME_COLORS: Record<SweepResult, string> = {
  success: "#0ca30c",
  failure: "#d03b3b",
  cancelled: "#8a8f9a",
  blocked: "#fab219",
};

/** Base → top. See the module doc for why this is not `SWEEP_RESULTS`'s order. */
export const OUTCOME_STACK_ORDER: readonly SweepResult[] = ["success", "blocked", "cancelled", "failure"] as const;

export const OUTCOME_LABELS: Record<SweepResult, string> = {
  success: "Succeeded",
  failure: "Failed",
  cancelled: "Cancelled",
  blocked: "Blocked",
};

export interface OutcomesChartOptions {
  /** Pixel width; defaults to the container's width, else 640. */
  width?: number;
  /** Plot height in px (the axis band is added on top of this). */
  height?: number;
  /** Maximum column thickness; the slot's leftover is air. */
  maxBarWidth?: number;
  /** Bucket granularity, for the subtitle. */
  granularityLabel?: string;
}

const DEFAULTS = { width: 640, height: 220, maxBarWidth: 24 };
const MARGINS: Margins = { top: 8, right: 8, bottom: 26, left: 44 };
const SEGMENT_GAP = 2;
const DATA_END_RADIUS = 4;

/**
 * Render `buckets` into `container`. Clears any prior content on each call
 * (matching `renderSweepTimeline`'s re-render contract). An empty `buckets`
 * array still renders a (contentless) `<svg>` — callers decide whether to
 * show a "no data" message around it.
 */
export function renderOutcomesChart(
  container: HTMLElement,
  buckets: OutcomeBucket[],
  options: OutcomesChartOptions = {},
): void {
  const width = options.width ?? measureWidth(container, DEFAULTS.width);
  const plotHeight = options.height ?? DEFAULTS.height;
  const maxBarWidth = options.maxBarWidth ?? DEFAULTS.maxBarWidth;
  const height = MARGINS.top + plotHeight + MARGINS.bottom;
  container.innerHTML = "";

  const svg = svgEl("svg");
  svg.setAttribute("class", "outcomes-chart chart-svg");
  svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
  svg.setAttribute("width", String(width));
  svg.setAttribute("height", String(height));
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", "Sweep outcomes over time");

  if (buckets.length === 0) {
    container.appendChild(svg);
    return;
  }

  renderChartHeader(
    container,
    "Sweep outcomes",
    `Completed sweeps per ${options.granularityLabel ?? "day"}, by result. Hover a column for its counts.`,
  );
  container.appendChild(svg);

  const plotWidth = width - MARGINS.left - MARGINS.right;
  const maxTotal = Math.max(...buckets.map((bucket) => bucket.total), 0);
  const ticks = niceTicks(maxTotal, 4);
  const domainMax = ticks[ticks.length - 1]!;
  const yFor = (count: number): number => MARGINS.top + plotHeight * (1 - count / domainMax);
  const slot = plotWidth / buckets.length;
  const barWidth = Math.max(Math.min(maxBarWidth, slot * 0.7), 1);
  const xCenter = (index: number): number => MARGINS.left + slot * index + slot / 2;

  drawYAxis(svg, ticks, yFor, MARGINS, plotWidth, (value) => formatCount(value));
  drawXAxis(svg, buckets.map((bucket) => formatBucketLabel(bucket.bucketKey)), xCenter, MARGINS, plotWidth, plotHeight);

  const tooltip = createTooltip(container);
  const baseline = MARGINS.top + plotHeight;

  buckets.forEach((bucket, index) => {
    const x = xCenter(index) - barWidth / 2;
    const group = svgEl("g");
    group.setAttribute("class", "outcomes-chart__bar");
    group.dataset.bucketKey = bucket.bucketKey;
    group.dataset.total = String(bucket.total);
    svg.appendChild(group);

    // Segments, base to top, with a 2px surface gap between neighbours.
    const present = OUTCOME_STACK_ORDER.filter((result) => bucket.counts[result] > 0);
    let yCursor = baseline;
    present.forEach((result, position) => {
      const count = bucket.counts[result];
      const rawHeight = (count / domainMax) * plotHeight;
      const isTop = position === present.length - 1;
      const gap = isTop ? 0 : SEGMENT_GAP;
      const segmentHeight = Math.max(rawHeight - gap, 0.5);
      const y = yCursor - rawHeight;

      const rect = svgEl("rect");
      rect.setAttribute("x", String(x));
      rect.setAttribute("y", String(y + gap));
      rect.setAttribute("width", String(barWidth));
      rect.setAttribute("height", String(segmentHeight));
      rect.setAttribute("fill", OUTCOME_COLORS[result]);
      rect.dataset.result = result;
      rect.dataset.count = String(count);
      if (isTop) {
        // Rounded data-end on the column's top segment only, via a clip on
        // the rect (the rect keeps its attributes for tests and styling).
        const clipId = `outcomes-cap-${index}`;
        const clip = svgEl("clipPath");
        clip.setAttribute("id", clipId);
        const capPath = svgEl("path");
        capPath.setAttribute("d", roundedTopRect(x, y + gap, barWidth, segmentHeight, DATA_END_RADIUS));
        clip.appendChild(capPath);
        group.appendChild(clip);
        rect.setAttribute("clip-path", `url(#${clipId})`);
      }
      group.appendChild(rect);
      yCursor -= rawHeight;
    });

    // Hit target: the whole slot, full plot height, so the pointer only has
    // to be near the column. A <path> so rect-counting callers see segments only.
    const hit = svgEl("path");
    hit.setAttribute("class", "chart-hit");
    hit.setAttribute("d", `M${MARGINS.left + slot * index},${MARGINS.top} h${slot} v${plotHeight} h${-slot} Z`);
    hit.setAttribute("fill", "transparent");
    hit.setAttribute("tabindex", "0");
    hit.setAttribute("role", "img");
    hit.setAttribute(
      "aria-label",
      `${formatBucketLabel(bucket.bucketKey)}: ${bucket.total} sweeps — ${OUTCOME_STACK_ORDER.map((r) => `${bucket.counts[r]} ${OUTCOME_LABELS[r].toLowerCase()}`).join(", ")}`,
    );
    const rows = [
      { label: "", value: formatBucketLabel(bucket.bucketKey), emphasis: true },
      ...OUTCOME_STACK_ORDER.slice().reverse().map((result) => ({
        label: OUTCOME_LABELS[result],
        value: formatCount(bucket.counts[result]),
        color: OUTCOME_COLORS[result],
      })),
      { label: "total", value: formatCount(bucket.total) },
    ];
    const show = (): void => {
      group.classList.add("is-hovered");
      tooltip.show(rows, xCenter(index), yFor(bucket.total));
    };
    const hide = (): void => {
      group.classList.remove("is-hovered");
      tooltip.hide();
    };
    hit.addEventListener("pointerenter", show);
    hit.addEventListener("pointermove", (event) => {
      group.classList.add("is-hovered");
      const point = localPoint(container, event);
      tooltip.show(rows, point.x, point.y);
    });
    hit.addEventListener("pointerleave", hide);
    hit.addEventListener("focus", show);
    hit.addEventListener("blur", hide);
    group.appendChild(hit);
  });

  renderLegend(
    container,
    OUTCOME_STACK_ORDER.map((result) => ({ label: OUTCOME_LABELS[result], color: OUTCOME_COLORS[result], shape: "rect" })),
  );
  renderTableView(
    container,
    "Sweep outcomes per bucket",
    ["Date", ...OUTCOME_STACK_ORDER.map((result) => OUTCOME_LABELS[result]), "Total"],
    buckets.map((bucket) => [
      formatBucketLabel(bucket.bucketKey),
      ...OUTCOME_STACK_ORDER.map((result) => formatCount(bucket.counts[result])),
      formatCount(bucket.total),
    ]),
  );
}
