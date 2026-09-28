import { describe, expect, it } from "vitest";

import { renderBurnRateChart } from "../src/analytics/burnRateChartView.js";
import type { BurnRatePoint } from "../src/analytics/burnRate.js";

const T0 = Date.parse("2026-09-21T12:00:00Z");
const MINUTE = 60 * 1000;

describe("renderBurnRateChart", () => {
  it("renders one <circle> per series per non-null point and a connecting <polyline> per series", () => {
    const container = document.createElement("div");
    const points: BurnRatePoint[] = [
      { at: T0, meanRatePerHour: 0.1, maxRatePerHour: 0.2 },
      { at: T0 + 30 * MINUTE, meanRatePerHour: 0.15, maxRatePerHour: 0.25 },
    ];

    renderBurnRateChart(container, points);

    const svg = container.querySelector("svg");
    expect(svg?.getAttribute("aria-label")).toBe("Token-pool burn rate");

    expect(container.querySelectorAll("circle.burn-rate-chart__point--mean")).toHaveLength(2);
    expect(container.querySelectorAll("circle.burn-rate-chart__point--peak")).toHaveLength(2);
    expect(container.querySelectorAll("polyline.burn-rate-chart__line--mean")).toHaveLength(1);
    expect(container.querySelectorAll("polyline.burn-rate-chart__line--peak")).toHaveLength(1);
  });

  it("renders a gap around a null point instead of interpolating or drawing a fabricated value", () => {
    const container = document.createElement("div");
    const points: BurnRatePoint[] = [
      { at: T0, meanRatePerHour: 0.2, maxRatePerHour: 0.3 },
      // A rollover: no rate is known here.
      { at: T0 + 20 * MINUTE, meanRatePerHour: null, maxRatePerHour: null },
      { at: T0 + 40 * MINUTE, meanRatePerHour: 0.05, maxRatePerHour: 0.05 },
    ];

    renderBurnRateChart(container, points);

    // Two isolated points on either side of the gap for each series — no
    // polyline connects across it (each side is a segment of length 1).
    expect(container.querySelectorAll("circle.burn-rate-chart__point--mean")).toHaveLength(2);
    expect(container.querySelectorAll("polyline.burn-rate-chart__line--mean")).toHaveLength(0);
  });

  it("draws a zero-centered %/h axis, a dated x-axis, a legend for both series, and a table view", () => {
    const container = document.createElement("div");
    renderBurnRateChart(
      container,
      [
        { at: T0, meanRatePerHour: 0.1, maxRatePerHour: 0.2 },
        { at: T0 + 30 * MINUTE, meanRatePerHour: -0.02, maxRatePerHour: 0.1 },
      ],
      { width: 640, title: "Token-pool burn rate — host-a" },
    );

    expect(container.querySelector(".chart-title")?.textContent).toBe("Token-pool burn rate — host-a");

    const legendLabels = Array.from(container.querySelectorAll(".chart-legend__label")).map((el) => el.textContent);
    expect(legendLabels).toEqual(["Mean", "Peak"]);

    const axisLabels = Array.from(container.querySelectorAll("text.chart-axis__label")).map((l) => l.textContent);
    // A zero tick must always be present — the domain is zero-centered.
    expect(axisLabels).toContain("0.0%/h");

    const tableRows = container.querySelectorAll(".chart-table tbody tr");
    expect(tableRows.length).toBe(2);

    const hover = container.querySelector<SVGElement>("path.chart-hit");
    hover?.dispatchEvent(new Event("focus"));
    const tooltip = container.querySelector<HTMLElement>(".chart-tooltip");
    expect(tooltip?.hidden).toBe(false);
    expect(tooltip?.textContent).toContain("mean rate");
    expect(tooltip?.textContent).toContain("peak rate");
    hover?.dispatchEvent(new Event("blur"));
    expect(tooltip?.hidden).toBe(true);
  });

  it("renders a contentless <svg> for an empty point list", () => {
    const container = document.createElement("div");
    renderBurnRateChart(container, []);
    expect(container.querySelector("svg")).not.toBeNull();
    expect(container.querySelectorAll("circle").length).toBe(0);
    expect(container.querySelector(".chart-title")).toBeNull();
  });
});
