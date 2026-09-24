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

  it("draws a 0–100% axis, dates, a crosshair tooltip and a table view; no legend for one series (#8546)", () => {
    const container = document.createElement("div");
    renderSuccessRateChart(
      container,
      [
        { bucketKey: "2026-09-19", successRate: 0.5, total: 4 },
        { bucketKey: "2026-09-20", successRate: null, total: 0 },
        { bucketKey: "2026-09-21", successRate: 1, total: 2 },
      ],
      { width: 640 },
    );

    expect(container.querySelector(".chart-title")?.textContent).toBe("Success rate");
    const axisLabels = Array.from(container.querySelectorAll("text.chart-axis__label")).map((l) => l.textContent);
    expect(axisLabels).toEqual(expect.arrayContaining(["0%", "50%", "100%", "Sep 19", "Sep 21"]));
    expect(container.querySelector(".chart-legend")).toBeNull();

    const tableRows = container.querySelectorAll(".chart-table tbody tr");
    expect(tableRows.length).toBe(3);
    expect(tableRows[1]?.textContent).toBe("Sep 20" + "—" + "0");

    const hover = container.querySelector<SVGElement>("path.chart-hit");
    hover?.dispatchEvent(new Event("focus"));
    const tooltip = container.querySelector<HTMLElement>(".chart-tooltip");
    expect(tooltip?.hidden).toBe(false);
    expect(tooltip?.textContent).toContain("Sep 21");
    expect(tooltip?.textContent).toContain("100%");
    expect(container.querySelector("line.chart-crosshair")?.getAttribute("visibility")).toBe("visible");
    hover?.dispatchEvent(new Event("blur"));
    expect(tooltip?.hidden).toBe(true);
  });

  it("renders a contentless <svg> for an empty point list", () => {
    const container = document.createElement("div");
    renderSuccessRateChart(container, []);
    expect(container.querySelector("svg")).not.toBeNull();
    expect(container.querySelectorAll("circle").length).toBe(0);
  });
});
