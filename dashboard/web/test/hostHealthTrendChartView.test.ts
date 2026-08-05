import { describe, expect, it } from "vitest";
import { renderMetricTrendChart } from "../src/charts/hostHealthTrendChartView.js";
import type { HealthTrendPoint } from "../src/charts/hostHealthTrend.js";

describe("renderMetricTrendChart", () => {
  it("renders one <circle> point per non-null reading and a connecting <polyline>", () => {
    const container = document.createElement("div");
    const points: HealthTrendPoint[] = [
      { emittedAt: "2026-08-01T10:00:00Z", value: 0.5 },
      { emittedAt: "2026-08-01T11:00:00Z", value: 0.9 },
    ];

    renderMetricTrendChart(container, points, { domainMax: 1 });

    const svg = container.querySelector("svg");
    expect(svg).not.toBeNull();

    const circles = container.querySelectorAll("circle");
    expect(circles.length).toBe(2);
    expect(circles[0]?.getAttribute("data-emitted-at")).toBe("2026-08-01T10:00:00Z");
    expect(container.querySelectorAll("polyline").length).toBe(1);
  });

  // Acceptance criterion (#5355): an absent/unmeasurable reading must render
  // as a gap, never as a zero — this is the single most important
  // correctness detail the issue calls out.
  it("renders a gap around a null (unmeasured) point instead of drawing it as zero", () => {
    const container = document.createElement("div");
    const points: HealthTrendPoint[] = [
      { emittedAt: "2026-08-01T10:00:00Z", value: 0.9 },
      { emittedAt: "2026-08-01T11:00:00Z", value: null },
      { emittedAt: "2026-08-01T12:00:00Z", value: 0.8 },
    ];

    renderMetricTrendChart(container, points, { domainMax: 1 });

    // Only the two real readings get a marker — no synthesized zero-value
    // circle for the null point.
    const circles = [...container.querySelectorAll("circle")];
    expect(circles.length).toBe(2);
    expect(circles.map((c) => c.dataset.emittedAt)).toEqual([
      "2026-08-01T10:00:00Z",
      "2026-08-01T12:00:00Z",
    ]);

    // Neither segment (length 1 on each side of the gap) draws a connecting
    // line, so the polyline never bridges across the gap.
    expect(container.querySelectorAll("polyline").length).toBe(0);
  });

  it("does not interpolate a line across a gap even with points on both sides of it", () => {
    const container = document.createElement("div");
    const points: HealthTrendPoint[] = [
      { emittedAt: "2026-08-01T09:00:00Z", value: 0.9 },
      { emittedAt: "2026-08-01T10:00:00Z", value: 0.7 },
      { emittedAt: "2026-08-01T11:00:00Z", value: null },
      { emittedAt: "2026-08-01T12:00:00Z", value: 0.5 },
      { emittedAt: "2026-08-01T13:00:00Z", value: 0.4 },
    ];

    renderMetricTrendChart(container, points, { domainMax: 1 });

    // Two segments — one on each side of the gap — never a single polyline
    // spanning all five points.
    expect(container.querySelectorAll("polyline").length).toBe(2);
    expect(container.querySelectorAll("circle").length).toBe(4);
  });

  it("renders a contentless <svg> for an empty point list", () => {
    const container = document.createElement("div");
    renderMetricTrendChart(container, []);
    expect(container.querySelector("svg")).not.toBeNull();
    expect(container.querySelectorAll("circle").length).toBe(0);
  });

  it("formats the point's data-value via the formatValue callback", () => {
    const container = document.createElement("div");
    renderMetricTrendChart(container, [{ emittedAt: "2026-08-01T10:00:00Z", value: 0.83 }], {
      domainMax: 1,
      formatValue: (value) => `${(value * 100).toFixed(0)}%`,
    });

    expect(container.querySelector("circle")?.dataset.value).toBe("83%");
  });

  it("auto-scales the domain to the observed maximum when no domainMax is given", () => {
    const container = document.createElement("div");
    const points: HealthTrendPoint[] = [
      { emittedAt: "2026-08-01T10:00:00Z", value: 100 },
      { emittedAt: "2026-08-01T11:00:00Z", value: 200 },
    ];

    renderMetricTrendChart(container, points, { height: 100, padding: 0 });

    // The larger value (200) should plot at the very top (y = 0); the
    // smaller (100) at the vertical midpoint.
    const circles = [...container.querySelectorAll("circle")];
    const yFirst = Number(circles[0]?.getAttribute("cy"));
    const ySecond = Number(circles[1]?.getAttribute("cy"));
    expect(ySecond).toBeLessThan(yFirst);
    expect(ySecond).toBeCloseTo(0, 5);
  });
});
