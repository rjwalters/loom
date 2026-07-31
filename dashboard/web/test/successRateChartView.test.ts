import { describe, expect, it } from "vitest";
import { renderSuccessRateChart } from "../src/charts/successRateChartView.js";
import type { SuccessRatePoint } from "../src/charts/successRate.js";

describe("renderSuccessRateChart", () => {
  it("renders one <circle> point per non-null bucket and a connecting <polyline>", () => {
    const container = document.createElement("div");
    const points: SuccessRatePoint[] = [
      { bucketKey: "2026-07-28", successRate: 0.5, total: 2 },
      { bucketKey: "2026-07-29", successRate: 1, total: 1 },
    ];

    renderSuccessRateChart(container, points);

    const svg = container.querySelector("svg");
    expect(svg?.getAttribute("aria-label")).toBe("Success rate trend");

    const circles = container.querySelectorAll("circle");
    expect(circles.length).toBe(2);
    expect(circles[0]?.getAttribute("data-bucket-key")).toBe("2026-07-28");
    expect(circles[0]?.getAttribute("data-success-rate")).toBe("0.5");

    expect(container.querySelectorAll("polyline").length).toBe(1);
  });

  it("renders a gap around a null bucket instead of interpolating through it", () => {
    const container = document.createElement("div");
    const points: SuccessRatePoint[] = [
      { bucketKey: "2026-07-28", successRate: 1, total: 1 },
      { bucketKey: "2026-07-29", successRate: null, total: 0 },
      { bucketKey: "2026-07-30", successRate: 0, total: 1 },
    ];

    renderSuccessRateChart(container, points);

    // Two isolated points on either side of the gap — no polyline connects
    // across it (each segment has length 1, so neither draws a line).
    expect(container.querySelectorAll("circle").length).toBe(2);
    expect(container.querySelectorAll("polyline").length).toBe(0);
  });

  it("renders a contentless <svg> for an empty point list", () => {
    const container = document.createElement("div");
    renderSuccessRateChart(container, []);
    expect(container.querySelector("svg")).not.toBeNull();
    expect(container.querySelectorAll("circle").length).toBe(0);
  });
});
