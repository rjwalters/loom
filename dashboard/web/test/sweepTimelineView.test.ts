// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { SweepTimelineView, prLinkFor, renderSweepTimeline } from "../src/sweepTimelineView.js";
import type { SweepPhaseRecord, SweepOutcomeRecord } from "../src/types.js";

const SWEEP_ID = "sweep-issue-4703-0";

function phase(phaseName: string, enteredAt: string): SweepPhaseRecord {
  return {
    kind: "sweep.phase",
    repo: "rjwalters/loom",
    visibility: "public",
    issue: 4703,
    sweep_id: SWEEP_ID,
    phase: phaseName,
    entered_at: enteredAt,
  };
}

function outcome(overrides: Partial<SweepOutcomeRecord> = {}): SweepOutcomeRecord {
  return {
    kind: "sweep.outcome",
    repo: "rjwalters/loom",
    visibility: "public",
    issue: 4703,
    sweep_id: SWEEP_ID,
    result: "success",
    ...overrides,
  };
}

describe("SweepTimelineView", () => {
  it("renders one row per phase and updates as records are ingested", () => {
    const container = document.createElement("div");
    const view = new SweepTimelineView(SWEEP_ID, container, "rjwalters/loom");

    view.ingest(phase("curator", "2026-07-30T12:00:00Z"));
    expect(container.querySelectorAll(".sweep-timeline__phase")).toHaveLength(1);
    expect(container.querySelector(".sweep-timeline__phase--ongoing")).not.toBeNull();

    view.ingest(phase("builder", "2026-07-30T12:00:12Z"));
    expect(container.querySelectorAll(".sweep-timeline__phase")).toHaveLength(2);

    view.ingest(outcome({ pr_number: 4710, result: "success" }));
    const result = container.querySelector(".sweep-timeline__result");
    expect(result?.getAttribute("data-result")).toBe("success");

    const link = container.querySelector<HTMLAnchorElement>(".sweep-timeline__pr-link");
    expect(link?.getAttribute("href")).toBe("https://github.com/rjwalters/loom/pull/4710");
  });

  it("supports bulk backfill ingestion via ingestAll", () => {
    const container = document.createElement("div");
    const view = new SweepTimelineView(SWEEP_ID, container);
    view.ingestAll([
      phase("curator", "2026-07-30T12:00:00Z"),
      phase("builder", "2026-07-30T12:00:12Z"),
      phase("judge", "2026-07-30T12:05:52Z"),
    ]);

    expect(container.querySelectorAll(".sweep-timeline__phase")).toHaveLength(3);
  });
});

describe("renderSweepTimeline", () => {
  it("omits the result/PR sections while a sweep is still in progress", () => {
    const container = document.createElement("div");
    renderSweepTimeline(container, {
      sweepId: SWEEP_ID,
      phases: [{ phase: "builder", enteredAt: "2026-07-30T12:00:00Z", ongoing: true }],
    });

    expect(container.querySelector(".sweep-timeline__result")).toBeNull();
    expect(container.querySelector(".sweep-timeline__pr-link")).toBeNull();
  });
});

describe("prLinkFor", () => {
  it("falls back to a bare PR reference when no repo is known", () => {
    const link = prLinkFor({ sweepId: SWEEP_ID, phases: [], prNumber: 42 });
    expect(link).toBe("#42");
  });

  it("returns undefined when there is no pr_number yet", () => {
    const link = prLinkFor({ sweepId: SWEEP_ID, phases: [] }, "rjwalters/loom");
    expect(link).toBeUndefined();
  });
});
