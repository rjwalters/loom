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

    const labels = container.querySelectorAll("text");
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
    // p90 (100) is the max value, so its bar should span the full chart width.
    expect(Number(p90?.getAttribute("width"))).toBeCloseTo(640 - 100);

    const p50 = container.querySelector('rect[data-row="overall"][data-rank="50"]');
    // p50 (50) is half of the max, so its bar should be half as wide.
    expect(Number(p50?.getAttribute("width"))).toBeCloseTo((640 - 100) / 2);
  });
});
