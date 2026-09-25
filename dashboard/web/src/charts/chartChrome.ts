/**
 * Shared chart chrome (issue #8546): the parts a reader needs to interpret a
 * chart and that the three historical charts were shipped without — a title,
 * axes with clean ticks and hairline gridlines, a legend for multi-series
 * charts, a hover/focus tooltip, and a table view that makes every plotted
 * value reachable without hovering.
 *
 * Design rules encoded here (not restated per chart):
 * - Chrome is recessive: gridlines/axes are 1px solid, one step off the
 *   surface; all text wears the page's text tokens, never a series color.
 * - Charts render at **pixel size** (`width`/`height` attributes equal to the
 *   viewBox) so glyphs, strokes and markers never scale with the viewport;
 *   the caller measures its container (`measureWidth`) and re-renders on
 *   resize.
 * - Tooltip/legend/table text is inserted with `textContent` — labels come
 *   from API payloads and are untrusted.
 * - A tooltip enhances, it never gates: the same values are in the table.
 */

const SVG_NS = "http://www.w3.org/2000/svg";

export function svgEl<K extends keyof SVGElementTagNameMap>(tag: K): SVGElementTagNameMap[K] {
  return document.createElementNS(SVG_NS, tag);
}

/** Plot margins (px) around the drawing area; left/bottom hold the axes. */
export interface Margins {
  top: number;
  right: number;
  bottom: number;
  left: number;
}

/** CSS-variable-driven chrome so the stylesheet's theme tokens apply. */
export const CHROME = {
  grid: "var(--border)",
  axisText: "var(--text-dim)",
  surface: "var(--bg)",
  fontSize: 11,
} as const;

/** The width to render at: the container's laid-out width, else `fallback`
 * (tests under happy-dom have no layout and get a deterministic size). */
export function measureWidth(container: HTMLElement, fallback: number): number {
  const measured = container.clientWidth;
  return measured > 0 ? measured : fallback;
}

/**
 * "Nice" tick values covering `[0, max]` — 1/2/2.5/5 × 10^k steps, so a
 * y-axis reads 0 / 5 / 10 rather than 0 / 3.7 / 7.4. Always includes 0 and a
 * top tick ≥ `max`. `max <= 0` yields `[0, 1]` so an all-zero series still
 * has an axis.
 */
export function niceTicks(max: number, targetCount = 4): number[] {
  if (!(max > 0)) return [0, 1];
  const rough = max / Math.max(targetCount, 1);
  const magnitude = 10 ** Math.floor(Math.log10(rough));
  const residual = rough / magnitude;
  const factor = residual <= 1 ? 1 : residual <= 2 ? 2 : residual <= 2.5 ? 2.5 : residual <= 5 ? 5 : 10;
  const step = factor * magnitude;
  const ticks: number[] = [];
  for (let value = 0; value < max + step; value += step) {
    ticks.push(Number(value.toFixed(10)));
    if (value >= max) break;
  }
  return ticks;
}

/** "Nice" ticks for a duration axis in seconds: snaps to human units so the
 * axis reads 30 s / 1 min / 5 min / 1 h rather than 300 s / 600 s. */
export function niceDurationTicks(maxSeconds: number, targetCount = 4): number[] {
  if (!(maxSeconds > 0)) return [0, 1];
  const candidates = [
    1, 2, 5, 10, 15, 30, // seconds
    60, 120, 300, 600, 900, 1800, // minutes
    3600, 7200, 10800, 21600, 43200, // hours
    86400, 172800, 604800, // days
  ];
  const rough = maxSeconds / Math.max(targetCount, 1);
  const step = candidates.find((candidate) => candidate >= rough) ?? candidates[candidates.length - 1]! * Math.ceil(rough / candidates[candidates.length - 1]!);
  const ticks: number[] = [];
  for (let value = 0; value < maxSeconds + step; value += step) {
    ticks.push(value);
    if (value >= maxSeconds) break;
  }
  return ticks;
}

const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/** `2026-09-21` → `Sep 21`; a key that is not `YYYY-MM-DD` is returned as-is. */
export function formatBucketLabel(bucketKey: string): string {
  const match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(bucketKey);
  if (!match) return bucketKey;
  const month = Number(match[2]);
  const day = Number(match[3]);
  const name = MONTHS[month - 1];
  return name ? `${name} ${day}` : bucketKey;
}

/** Which of `n` evenly spaced positions get an x-axis label: first, last, and
 * every k-th in between so at most ~`maxLabels` are drawn. */
export function xLabelIndices(n: number, maxLabels = 8): number[] {
  if (n <= 0) return [];
  if (n <= maxLabels) return Array.from({ length: n }, (_, i) => i);
  const stride = Math.ceil((n - 1) / (maxLabels - 1));
  const indices: number[] = [];
  for (let i = 0; i < n; i += stride) indices.push(i);
  if (indices[indices.length - 1] !== n - 1) {
    // Drop a penultimate label that would collide with the last one.
    if (n - 1 - indices[indices.length - 1]! < stride / 2) indices.pop();
    indices.push(n - 1);
  }
  return indices;
}

function axisText(x: number, y: number, text: string, anchor: "start" | "middle" | "end"): SVGTextElement {
  const node = svgEl("text");
  node.setAttribute("x", String(x));
  node.setAttribute("y", String(y));
  node.setAttribute("text-anchor", anchor);
  node.setAttribute("font-size", String(CHROME.fontSize));
  node.setAttribute("class", "chart-axis__label");
  node.setAttribute("style", `fill: ${CHROME.axisText}`);
  node.textContent = text;
  return node;
}

function hairline(x1: number, y1: number, x2: number, y2: number, cls: string): SVGLineElement {
  const line = svgEl("line");
  line.setAttribute("x1", String(x1));
  line.setAttribute("y1", String(y1));
  line.setAttribute("x2", String(x2));
  line.setAttribute("y2", String(y2));
  line.setAttribute("class", cls);
  line.setAttribute("style", `stroke: ${CHROME.grid}; stroke-width: 1`);
  line.setAttribute("shape-rendering", "crispEdges");
  return line;
}

/**
 * Draw a left y-axis: a gridline across the plot at every tick and a label at
 * the left margin. `yFor` maps a tick value to a pixel y.
 */
export function drawYAxis(
  svg: SVGSVGElement,
  ticks: number[],
  yFor: (value: number) => number,
  margins: Margins,
  plotWidth: number,
  format: (value: number) => string,
): void {
  const group = svgEl("g");
  group.setAttribute("class", "chart-axis chart-axis--y");
  for (const tick of ticks) {
    const y = yFor(tick);
    group.appendChild(hairline(margins.left, y, margins.left + plotWidth, y, "chart-grid__line"));
    group.appendChild(axisText(margins.left - 6, y + 4, format(tick), "end"));
  }
  svg.appendChild(group);
}

/**
 * Draw a bottom x-axis for `n` evenly spaced categories: a baseline plus a
 * label under a readable subset of positions. `xFor(i)` is the center of
 * position `i`.
 */
export function drawXAxis(
  svg: SVGSVGElement,
  labels: string[],
  xFor: (index: number) => number,
  margins: Margins,
  plotWidth: number,
  plotHeight: number,
): void {
  const group = svgEl("g");
  group.setAttribute("class", "chart-axis chart-axis--x");
  const baseline = margins.top + plotHeight;
  const svgWidth = margins.left + plotWidth + margins.right;
  group.appendChild(hairline(margins.left, baseline, margins.left + plotWidth, baseline, "chart-axis__baseline"));
  for (const index of xLabelIndices(labels.length)) {
    const label = labels[index]!;
    const x = xFor(index);
    // A label centered on the first/last position can run off the svg (a
    // line chart's end points sit on the plot edge); anchor it inward instead
    // of clipping. Width is estimated — good enough for short date labels.
    const halfWidth = (label.length * CHROME.fontSize * 0.6) / 2;
    const anchor = x - halfWidth < 0 ? "start" : x + halfWidth > svgWidth ? "end" : "middle";
    group.appendChild(axisText(x, baseline + CHROME.fontSize + 6, label, anchor));
  }
  svg.appendChild(group);
}

/** Draw a bottom x-axis for a continuous scale (tick values → x). */
export function drawXValueAxis(
  svg: SVGSVGElement,
  ticks: number[],
  xFor: (value: number) => number,
  margins: Margins,
  plotWidth: number,
  plotHeight: number,
  format: (value: number) => string,
): void {
  const group = svgEl("g");
  group.setAttribute("class", "chart-axis chart-axis--x");
  const baseline = margins.top + plotHeight;
  group.appendChild(hairline(margins.left, baseline, margins.left + plotWidth, baseline, "chart-axis__baseline"));
  for (const tick of ticks) {
    const x = xFor(tick);
    group.appendChild(hairline(x, margins.top, x, baseline, "chart-grid__line"));
    group.appendChild(axisText(x, baseline + CHROME.fontSize + 6, format(tick), "middle"));
  }
  svg.appendChild(group);
}

/** A path for a rectangle with only its top corners rounded — a column's
 * data-end, square at the baseline side. */
export function roundedTopRect(x: number, y: number, w: number, h: number, r: number): string {
  const radius = Math.max(0, Math.min(r, w / 2, h));
  return [
    `M${x},${y + h}`,
    `V${y + radius}`,
    `Q${x},${y} ${x + radius},${y}`,
    `H${x + w - radius}`,
    `Q${x + w},${y} ${x + w},${y + radius}`,
    `V${y + h}`,
    "Z",
  ].join(" ");
}

/** A path for a rectangle with only its right corners rounded — a bar's
 * data-end, square at the baseline side. */
export function roundedRightRect(x: number, y: number, w: number, h: number, r: number): string {
  const radius = Math.max(0, Math.min(r, h / 2, w));
  return [
    `M${x},${y}`,
    `H${x + w - radius}`,
    `Q${x + w},${y} ${x + w},${y + radius}`,
    `V${y + h - radius}`,
    `Q${x + w},${y + h} ${x + w - radius},${y + h}`,
    `H${x}`,
    "Z",
  ].join(" ");
}

// --- HTML chrome around the SVG ---------------------------------------------

function div(cls: string): HTMLDivElement {
  const node = document.createElement("div");
  node.className = cls;
  return node;
}

/** Title + subtitle above a chart. */
export function renderChartHeader(container: HTMLElement, title: string, subtitle?: string): void {
  const head = div("chart-head");
  const heading = document.createElement("h3");
  heading.className = "chart-title";
  heading.textContent = title;
  head.appendChild(heading);
  if (subtitle) {
    const sub = document.createElement("p");
    sub.className = "chart-subtitle";
    sub.textContent = subtitle;
    head.appendChild(sub);
  }
  container.appendChild(head);
}

export interface LegendItem {
  label: string;
  color: string;
  /** Mirrors the mark: `rect` for bars/areas, `line` for lines. */
  shape?: "rect" | "line";
}

/** A legend row. Callers only add one for two or more series. */
export function renderLegend(container: HTMLElement, items: LegendItem[]): void {
  const legend = div("chart-legend");
  legend.setAttribute("role", "list");
  for (const item of items) {
    const entry = div("chart-legend__item");
    entry.setAttribute("role", "listitem");
    const swatch = document.createElement("span");
    swatch.className = `chart-legend__swatch chart-legend__swatch--${item.shape ?? "rect"}`;
    swatch.style.background = item.color;
    swatch.setAttribute("aria-hidden", "true");
    const label = document.createElement("span");
    label.className = "chart-legend__label";
    label.textContent = item.label;
    entry.append(swatch, label);
    legend.appendChild(entry);
  }
  container.appendChild(legend);
}

export interface TooltipRow {
  label: string;
  value: string;
  /** Series color for the row's line key; omitted for a title/total row. */
  color?: string;
  emphasis?: boolean;
}

export interface Tooltip {
  show(rows: TooltipRow[], anchorX: number, anchorY: number): void;
  hide(): void;
  readonly element: HTMLElement;
}

/**
 * One tooltip per chart container, positioned in the container's own box
 * (which the stylesheet makes `position: relative`). Values lead, labels
 * follow; series identity is a short line key in the series color.
 */
export function createTooltip(container: HTMLElement): Tooltip {
  const element = div("chart-tooltip");
  element.setAttribute("role", "status");
  element.setAttribute("aria-live", "polite");
  element.hidden = true;
  container.appendChild(element);

  return {
    element,
    show(rows, anchorX, anchorY) {
      element.replaceChildren();
      for (const row of rows) {
        const line = div(`chart-tooltip__row${row.emphasis ? " chart-tooltip__row--emphasis" : ""}`);
        if (row.color) {
          const key = document.createElement("span");
          key.className = "chart-tooltip__key";
          key.style.background = row.color;
          key.setAttribute("aria-hidden", "true");
          line.appendChild(key);
        }
        const value = document.createElement("span");
        value.className = "chart-tooltip__value";
        value.textContent = row.value;
        const label = document.createElement("span");
        label.className = "chart-tooltip__label";
        label.textContent = row.label;
        line.append(value, label);
        element.appendChild(line);
      }
      element.hidden = false;
      // Keep the box inside the container: flip to the left of the anchor
      // when it would overflow the right edge.
      const width = container.clientWidth || 0;
      const boxWidth = element.offsetWidth || 160;
      const left = width > 0 && anchorX + 12 + boxWidth > width ? Math.max(0, anchorX - 12 - boxWidth) : anchorX + 12;
      element.style.left = `${left}px`;
      element.style.top = `${Math.max(0, anchorY - 8)}px`;
    },
    hide() {
      element.hidden = true;
    },
  };
}

/** Pointer position relative to `container`'s box, from a pointer event. */
export function localPoint(container: HTMLElement, event: { clientX: number; clientY: number }): { x: number; y: number } {
  const rect = container.getBoundingClientRect();
  return { x: event.clientX - rect.left, y: event.clientY - rect.top };
}

/**
 * A collapsed table twin of the chart, so every value is reachable without
 * a pointer. `rows` are already-formatted strings; the first column is the
 * row header.
 */
export function renderTableView(container: HTMLElement, caption: string, headers: string[], rows: string[][]): void {
  const details = document.createElement("details");
  details.className = "chart-table";
  const summary = document.createElement("summary");
  summary.textContent = "Table view";
  details.appendChild(summary);

  const table = document.createElement("table");
  const cap = document.createElement("caption");
  cap.textContent = caption;
  table.appendChild(cap);
  const thead = document.createElement("thead");
  const headRow = document.createElement("tr");
  for (const header of headers) {
    const th = document.createElement("th");
    th.scope = "col";
    th.textContent = header;
    headRow.appendChild(th);
  }
  thead.appendChild(headRow);
  table.appendChild(thead);
  const tbody = document.createElement("tbody");
  for (const row of rows) {
    const tr = document.createElement("tr");
    row.forEach((cell, index) => {
      const node = document.createElement(index === 0 ? "th" : "td");
      if (index === 0) (node as HTMLTableCellElement).scope = "row";
      node.textContent = cell;
      tr.appendChild(node);
    });
    tbody.appendChild(tr);
  }
  table.appendChild(tbody);
  details.appendChild(table);
  container.appendChild(details);
}
