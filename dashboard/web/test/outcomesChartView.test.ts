import { describe, expect, it } from "vitest";
import { OUTCOME_COLORS, renderOutcomesChart } from "../src/charts/outcomesChartView.js";
import type { OutcomeBucket } from "../src/charts/outcomes.js";

function bucket(overrides: Partial<OutcomeBucket> & { bucketKey: string }): OutcomeBucket {
  return {
    counts: { success: 0, failure: 0, cancelled: 0, blocked: 0 },
    total: 0,
    ...overrides,
  };
}

describe("renderOutcomesChart", () => {
  it("renders one <rect> segment per non-zero result, colored per result", () => {
    const container = document.createElement("div");
    const buckets: OutcomeBucket[] = [
      bucket({
        bucketKey: "2026-07-28",
        counts: { success: 2, failure: 1, cancelled: 0, blocked: 0 },
        total: 3,
      }),
    ];

    renderOutcomesChart(container, buckets);

    const svg = container.querySelector("svg");
    expect(svg).not.toBeNull();
    expect(svg?.getAttribute("aria-label")).toBe("Sweep outcomes over time");

    const rects = container.querySelectorAll("rect");
    expect(rects.length).toBe(2); // success + failure; cancelled/blocked are 0 and skipped
    const success = container.querySelector('rect[data-result="success"]');
    expect(success?.getAttribute("fill")).toBe(OUTCOME_COLORS.success);
    expect(success?.getAttribute("data-count")).toBe("2");
  });

  it("groups each bucket's segments under one <g data-bucket-key>", () => {
    const container = document.createElement("div");
    const buckets: OutcomeBucket[] = [
      bucket({ bucketKey: "2026-07-28", counts: { success: 1, failure: 0, cancelled: 0, blocked: 0 }, total: 1 }),
      bucket({ bucketKey: "2026-07-29", counts: { success: 0, failure: 1, cancelled: 0, blocked: 0 }, total: 1 }),
    ];

    renderOutcomesChart(container, buckets);

    const groups = container.querySelectorAll("g.outcomes-chart__bar");
    expect(groups.length).toBe(2);
    expect(groups[0]?.getAttribute("data-bucket-key")).toBe("2026-07-28");
    expect(groups[1]?.getAttribute("data-bucket-key")).toBe("2026-07-29");
  });

  it("renders a contentless <svg> for an empty bucket list", () => {
    const container = document.createElement("div");
    renderOutcomesChart(container, []);
    expect(container.querySelector("svg")).not.toBeNull();
    expect(container.querySelectorAll("rect").length).toBe(0);
  });

  it("clears prior content on re-render", () => {
    const container = document.createElement("div");
    renderOutcomesChart(container, [
      bucket({ bucketKey: "2026-07-28", counts: { success: 1, failure: 0, cancelled: 0, blocked: 0 }, total: 1 }),
    ]);
    renderOutcomesChart(container, []);
    expect(container.querySelectorAll("svg").length).toBe(1);
    expect(container.querySelectorAll("rect").length).toBe(0);
    expect(container.querySelector(".chart-legend")).toBeNull();
  });

  it("draws a count axis, dated x-axis, legend, table view and a per-column tooltip (#8546)", () => {
    const container = document.createElement("div");
    const buckets: OutcomeBucket[] = [
      bucket({ bucketKey: "2026-09-19", counts: { success: 3, failure: 1, cancelled: 0, blocked: 0 }, total: 4 }),
      bucket({ bucketKey: "2026-09-20", counts: { success: 6, failure: 2, cancelled: 1, blocked: 1 }, total: 10 }),
      bucket({ bucketKey: "2026-09-21", counts: { success: 2, failure: 0, cancelled: 0, blocked: 0 }, total: 2 }),
    ];

    renderOutcomesChart(container, buckets, { width: 640 });

    expect(container.querySelector(".chart-title")?.textContent).toBe("Sweep outcomes");
    const axisLabels = Array.from(container.querySelectorAll("text.chart-axis__label")).map((l) => l.textContent);
    expect(axisLabels).toContain("0");
    expect(axisLabels).toContain("10");
    expect(axisLabels).toContain("Sep 19");
    expect(axisLabels).toContain("Sep 21");

    // Legend names every color, in stack order (base → top).
    const legend = Array.from(container.querySelectorAll(".chart-legend__label")).map((l) => l.textContent);
    expect(legend).toEqual(["Succeeded", "Blocked", "Cancelled", "Failed"]);

    // Segments are stacked in that order: success at the baseline.
    const day2 = container.querySelector('g[data-bucket-key="2026-09-20"]');
    const order = Array.from(day2?.querySelectorAll("rect") ?? []).map((r) => r.getAttribute("data-result"));
    expect(order).toEqual(["success", "blocked", "cancelled", "failure"]);
    const successY = Number(day2?.querySelector('rect[data-result="success"]')?.getAttribute("y"));
    const failureY = Number(day2?.querySelector('rect[data-result="failure"]')?.getAttribute("y"));
    expect(failureY).toBeLessThan(successY);

    const tableRows = container.querySelectorAll(".chart-table tbody tr");
    expect(tableRows.length).toBe(3);
    expect(tableRows[1]?.textContent).toBe("Sep 20" + "6" + "1" + "1" + "2" + "10");

    const hit = day2?.querySelector("path.chart-hit");
    expect(hit?.getAttribute("aria-label")).toContain("Sep 20: 10 sweeps");
    hit?.dispatchEvent(new Event("pointerenter"));
    const tooltip = container.querySelector<HTMLElement>(".chart-tooltip");
    expect(tooltip?.hidden).toBe(false);
    const tooltipText = tooltip?.textContent ?? "";
    expect(tooltipText).toContain("Sep 20");
    expect(tooltipText).toContain("Failed");
    expect(tooltipText).toContain("total");
    hit?.dispatchEvent(new Event("pointerleave"));
    expect(tooltip?.hidden).toBe(true);
  });

  it("renders at pixel size — the svg's width/height match its viewBox", () => {
    const container = document.createElement("div");
    renderOutcomesChart(container, [bucket({ bucketKey: "2026-09-21", counts: { success: 1, failure: 0, cancelled: 0, blocked: 0 }, total: 1 })], {
      width: 900,
    });
    const svg = container.querySelector("svg");
    expect(svg?.getAttribute("width")).toBe("900");
    expect(svg?.getAttribute("viewBox")?.startsWith("0 0 900 ")).toBe(true);
  });
});
