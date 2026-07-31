// @vitest-environment jsdom
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
  });
});
