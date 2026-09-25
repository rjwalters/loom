import { describe, expect, it } from "vitest";
import { PERCENTILE_COLORS, renderDurationPercentilesChart } from "../src/charts/durationsChartView.js";
import type { DurationPercentiles } from "../src/charts/durations.js";

describe("renderDurationPercentilesChart", () => {
  it("renders an 'overall' row plus one row per phase, each with p50/p90/p99 bars", () => {
    const container = document.createElement("div");
    const data: DurationPercentiles = {
      overall: { 50: 100, 90: 200, 99: 300 },
      byPhase: {
        builder: { 50: 60, 90: 120, 99: 180 },
      },
    };

    renderDurationPercentilesChart(container, data);

    const svg = container.querySelector("svg");
    expect(svg?.getAttribute("aria-label")).toBe("Sweep duration percentiles");

    const labels = container.querySelectorAll("text[data-row]");
    expect(Array.from(labels).map((l) => l.textContent)).toEqual(["overall", "builder"]);

    const overallRects = container.querySelectorAll('rect[data-row="overall"]');
    expect(overallRects.length).toBe(3);
    const p50 = container.querySelector('rect[data-row="overall"][data-rank="50"]');
    expect(p50?.getAttribute("fill")).toBe(PERCENTILE_COLORS[50]);
    expect(p50?.getAttribute("data-value-sec")).toBe("100");

    const builderRects = container.querySelectorAll('rect[data-row="builder"]');
    expect(builderRects.length).toBe(3);
  });

  it("omits missing ranks and rows with no data", () => {
    const container = document.createElement("div");
    const data: DurationPercentiles = { overall: undefined, byPhase: {} };

    renderDurationPercentilesChart(container, data);

    expect(container.querySelector("svg")).not.toBeNull();
    expect(container.querySelectorAll("rect").length).toBe(0);
    expect(container.querySelectorAll("text").length).toBe(0);
  });

  it("scales bar widths relative to the largest value across all rows/ranks", () => {
    const container = document.createElement("div");
    const data: DurationPercentiles = {
      overall: { 50: 50, 90: 100 },
      byPhase: {},
    };

    renderDurationPercentilesChart(container, data, { width: 640, labelWidth: 100 });

    const p90 = container.querySelector('rect[data-row="overall"][data-rank="90"]');
    const p50 = container.querySelector('rect[data-row="overall"][data-rank="50"]');
    const p90Width = Number(p90?.getAttribute("width"));
    const p50Width = Number(p50?.getAttribute("width"));
    // The axis domain snaps to a "nice" duration tick (100 s → a 120 s top
    // tick), so the max bar no longer fills the plot exactly; what must hold
    // is the ratio between bars and that both are drawn in the plot.
    expect(p90Width).toBeGreaterThan(0);
    expect(p50Width).toBeCloseTo(p90Width / 2);
    expect(p90Width).toBeLessThanOrEqual(640 - 100);
  });

  it("draws a duration axis, a p50/p90/p99 legend, a table view, and a per-bar tooltip (#8546)", () => {
    const container = document.createElement("div");
    const data: DurationPercentiles = {
      overall: { 50: 90, 90: 600, 99: 3600 },
      byPhase: { builder: { 50: 60, 90: 120, 99: 180 } },
    };

    renderDurationPercentilesChart(container, data, { width: 640 });

    expect(container.querySelector(".chart-title")?.textContent).toBe("Sweep duration by phase");
    const axisLabels = Array.from(container.querySelectorAll("text.chart-axis__label")).map((l) => l.textContent);
    expect(axisLabels[0]).toBe("0s");
    expect(axisLabels.some((label) => label?.endsWith("h") || label?.includes("m"))).toBe(true);

    const legend = Array.from(container.querySelectorAll(".chart-legend__label")).map((l) => l.textContent);
    expect(legend).toEqual(["p50", "p90", "p99"]);

    const tableRows = container.querySelectorAll(".chart-table tbody tr");
    expect(tableRows.length).toBe(2);
    expect(tableRows[0]?.textContent).toContain("overall");
    expect(tableRows[0]?.textContent).toContain("1h 0m"); // p99 = 3600 s

    // Exactly one direct label per row (the p50 value at the bar tip).
    const tips = Array.from(container.querySelectorAll("text.durations-chart__tip-label")).map((l) => l.textContent);
    expect(tips).toEqual(["1m 30s", "1m 0s"]);

    const hit = container.querySelector('path.chart-hit[aria-label="builder p90: 2m 0s"]');
    expect(hit).not.toBeNull();
    hit?.dispatchEvent(new Event("focus"));
    const tooltip = container.querySelector<HTMLElement>(".chart-tooltip");
    expect(tooltip?.hidden).toBe(false);
    expect(tooltip?.textContent).toContain("builder");
    expect(tooltip?.textContent).toContain("2m 0s");
    hit?.dispatchEvent(new Event("blur"));
    expect(tooltip?.hidden).toBe(true);
  });
});
