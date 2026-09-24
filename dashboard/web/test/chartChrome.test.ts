import { describe, expect, it } from "vitest";
import {
  createTooltip,
  formatBucketLabel,
  niceDurationTicks,
  niceTicks,
  renderLegend,
  renderTableView,
  xLabelIndices,
} from "../src/charts/chartChrome.js";

describe("chartChrome (#8546)", () => {
  it("niceTicks covers [0, max] with clean steps and a top tick >= max", () => {
    expect(niceTicks(0)).toEqual([0, 1]);
    expect(niceTicks(7, 4)).toEqual([0, 2, 4, 6, 8]);
    expect(niceTicks(37, 4)).toEqual([0, 10, 20, 30, 40]);
    expect(niceTicks(1000, 4)).toEqual([0, 250, 500, 750, 1000]);
    const ticks = niceTicks(123.4, 5);
    expect(ticks[0]).toBe(0);
    expect(ticks[ticks.length - 1]).toBeGreaterThanOrEqual(123.4);
  });

  it("niceDurationTicks snaps to human units", () => {
    expect(niceDurationTicks(45)).toEqual([0, 15, 30, 45]);
    expect(niceDurationTicks(3600)).toEqual([0, 900, 1800, 2700, 3600]);
    expect(niceDurationTicks(0)).toEqual([0, 1]);
    const ticks = niceDurationTicks(100000, 4);
    expect(ticks[ticks.length - 1]).toBeGreaterThanOrEqual(100000);
  });

  it("formatBucketLabel renders YYYY-MM-DD as 'Mon D' and passes anything else through", () => {
    expect(formatBucketLabel("2026-09-21")).toBe("Sep 21");
    expect(formatBucketLabel("2026-01-05")).toBe("Jan 5");
    expect(formatBucketLabel("2026-W38")).toBe("2026-W38");
  });

  it("xLabelIndices keeps first and last and never exceeds the label budget", () => {
    expect(xLabelIndices(0)).toEqual([]);
    expect(xLabelIndices(5)).toEqual([0, 1, 2, 3, 4]);
    const thirty = xLabelIndices(30, 8);
    expect(thirty[0]).toBe(0);
    expect(thirty[thirty.length - 1]).toBe(29);
    expect(thirty.length).toBeLessThanOrEqual(8);
    expect(new Set(thirty).size).toBe(thirty.length);
  });

  it("renderLegend lists every item with a swatch and text inserted safely", () => {
    const container = document.createElement("div");
    renderLegend(container, [
      { label: "<b>Succeeded</b>", color: "#0ca30c" },
      { label: "Failed", color: "#d03b3b", shape: "line" },
    ]);
    const labels = container.querySelectorAll(".chart-legend__label");
    expect(labels.length).toBe(2);
    expect(labels[0]?.textContent).toBe("<b>Succeeded</b>");
    expect(container.querySelector("b")).toBeNull();
    expect(container.querySelector(".chart-legend__swatch--line")).not.toBeNull();
  });

  it("renderTableView builds a collapsed table with a caption, column headers and row headers", () => {
    const container = document.createElement("div");
    renderTableView(container, "Cap", ["Date", "A", "B"], [["Sep 1", "1", "2"], ["Sep 2", "3", "4"]]);
    const details = container.querySelector("details.chart-table");
    expect(details).not.toBeNull();
    expect(details?.querySelector("summary")?.textContent).toBe("Table view");
    expect(details?.querySelector("caption")?.textContent).toBe("Cap");
    expect(details?.querySelectorAll('th[scope="col"]').length).toBe(3);
    expect(details?.querySelectorAll('tbody th[scope="row"]').length).toBe(2);
    expect(details?.querySelectorAll("tbody td").length).toBe(4);
  });

  it("createTooltip shows value-first rows with a color key and hides again", () => {
    const container = document.createElement("div");
    const tooltip = createTooltip(container);
    expect(tooltip.element.hidden).toBe(true);
    tooltip.show(
      [
        { label: "", value: "Sep 21", emphasis: true },
        { label: "Failed", value: "3", color: "#d03b3b" },
      ],
      40,
      20,
    );
    expect(tooltip.element.hidden).toBe(false);
    const rows = tooltip.element.querySelectorAll(".chart-tooltip__row");
    expect(rows.length).toBe(2);
    expect(rows[1]?.querySelector(".chart-tooltip__key")).not.toBeNull();
    expect(rows[1]?.querySelector(".chart-tooltip__value")?.textContent).toBe("3");
    expect(rows[1]?.querySelector(".chart-tooltip__label")?.textContent).toBe("Failed");
    tooltip.hide();
    expect(tooltip.element.hidden).toBe(true);
  });
});
