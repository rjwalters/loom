/**
 * The "running now" panel (Issue #8306, Phase 3 of #8257).
 *
 * The load-bearing assertions here are the three the parent epic's acceptance
 * criteria name: every live job instance is listed with the fields an operator
 * needs to go find it, a leaked instance is distinguishable from a running one
 * by something other than colour, and zero jobs is an empty state rather than
 * an error or a fabricated row.
 */

import { describe, expect, it } from "vitest";

import { buildFleetView, sortComputeJobs } from "../src/fleet";
import { parseFleetSnapshot } from "../src/parse";
import {
  computeSubprocessList,
  instanceShapeText,
  runningComputeSection,
  runningForText,
} from "../src/views/runningCompute";
import { fleetOverviewView } from "../src/views/fleetOverview";
import type { ActiveComputeJob } from "../src/types";

const NOW = new Date("2026-09-19T18:00:00Z");

function job(overrides: Partial<ActiveComputeJob> = {}): ActiveComputeJob {
  return {
    hostId: "2am-elastic",
    jobId: "job-abc123",
    instanceId: "i-0123456789abcdef0",
    region: "us-east-1",
    instanceType: "c7i.4xlarge",
    spot: true,
    ami: "ami-0123456789abcdef0",
    startedAt: "2026-09-19T12:00:00Z",
    updatedAt: "2026-09-19T12:00:00Z",
    ...overrides,
  };
}

function rows(rendered: HTMLElement): HTMLElement[] {
  return [...rendered.querySelectorAll<HTMLElement>('[data-testid="compute-row"]')];
}

describe("runningComputeSection", () => {
  it("lists each live job with id, instance, shape and how long it has been running", () => {
    const rendered = runningComputeSection([job()], NOW, { authenticated: true });
    expect(rendered).not.toBeNull();

    const cells = [...rendered!.querySelectorAll("td")].map((cell) => cell.textContent);
    expect(cells[0]).toContain("job-abc123");
    expect(cells[1]).toBe("i-0123456789abcdef0");
    // Type, region and the spot flag in one line — all three are what an
    // operator needs to find the instance in a cloud console.
    expect(cells[2]).toBe("c7i.4xlarge · us-east-1 · spot");
    // 12:00Z → 18:00Z.
    expect(cells[3]).toBe("6h 0m");
  });

  it("distinguishes a leaked instance from a normally-running one by more than colour", () => {
    const rendered = runningComputeSection(
      [job({ jobId: "job-ok" }), job({ jobId: "job-leaked", leaked: true })],
      NOW,
      { authenticated: true },
    );
    const byJob = new Map(rows(rendered!).map((row) => [row.getAttribute("data-job"), row]));

    const leaked = byJob.get("job-leaked")!;
    const healthy = byJob.get("job-ok")!;

    // 1. A machine-readable attribute.
    expect(leaked.getAttribute("data-leaked")).toBe("true");
    expect(healthy.getAttribute("data-leaked")).toBe("false");
    // 2. A row modifier class.
    expect(leaked.className).toContain("compute-row--leaked");
    expect(healthy.className).not.toContain("compute-row--leaked");
    // 3. A text badge with a status role — visible to a screen reader and to a
    //    colour-blind reader both.
    const badge = leaked.querySelector('[data-testid="leaked-badge"]');
    expect(badge?.textContent).toBe("LEAKED");
    expect(badge?.getAttribute("role")).toBe("status");
    expect(healthy.querySelector('[data-testid="leaked-badge"]')).toBeNull();
    // 4. A count in the panel header.
    expect(rendered!.querySelector('[data-testid="compute-leaked-count"]')?.textContent).toBe(
      "1 possibly leaked",
    );
  });

  it("renders no panel — not an error, not a fake row — when nothing is running", () => {
    expect(runningComputeSection([], NOW, { authenticated: true })).toBeNull();
    expect(runningComputeSection([], NOW, { authenticated: false })).toBeNull();
  });

  it("never renders job detail to an unauthenticated viewer, even if entries somehow arrive", () => {
    // Defense in depth: `/public/fleet-state` already returns `activeCompute:
    // []`, so this input cannot occur today. If a future backend change ever
    // let it, the frontend must not be the surface that publishes it.
    const rendered = runningComputeSection([job()], NOW, { authenticated: false });
    expect(rendered).not.toBeNull();
    expect(rows(rendered!)).toHaveLength(0);
    expect(rendered!.querySelector('[data-testid="compute-withheld"]')).not.toBeNull();
    expect(rendered!.textContent).not.toContain("i-0123456789abcdef0");
    expect(rendered!.textContent).not.toContain("c7i.4xlarge");
  });

  it("degrades each missing field to an em dash rather than inventing one", () => {
    const rendered = runningComputeSection(
      [{ hostId: "2am-elastic", jobId: "job-bare" }],
      NOW,
      { authenticated: true },
    );
    const cells = [...rendered!.querySelectorAll("td")].map((cell) => cell.textContent);
    expect(cells[1]).toBe("—");
    expect(cells[2]).toBe("—");
    // Unknown start time is NOT "0s" — that would claim the job just started.
    expect(cells[3]).toBe("—");
  });
});

describe("computeSubprocessList (#8835)", () => {
  const entries = (rendered: HTMLElement) => [
    ...rendered.querySelectorAll<HTMLElement>('[data-testid="subprocess"]'),
  ];

  it("renders instance shape, spot flag and elapsed time for each nested job", () => {
    const rendered = computeSubprocessList([job()], NOW)!;
    expect(rendered).not.toBeNull();
    const [entry] = entries(rendered);
    expect(entry!.getAttribute("data-job")).toBe("job-abc123");
    expect(entry!.querySelector(".subprocess__shape")?.textContent).toBe("c7i.4xlarge · us-east-1 · spot");
    // 12:00Z → 18:00Z.
    expect(entry!.querySelector(".subprocess__age")?.textContent).toBe("6h 0m");
  });

  it("flags a leaked nested job by attribute, class and badge — never colour alone", () => {
    const rendered = computeSubprocessList(
      [job({ jobId: "job-ok" }), job({ jobId: "job-leaked", leaked: true })],
      NOW,
    )!;
    const byJob = new Map(entries(rendered).map((entry) => [entry.getAttribute("data-job"), entry]));
    const leaked = byJob.get("job-leaked")!;
    const healthy = byJob.get("job-ok")!;

    expect(leaked.getAttribute("data-leaked")).toBe("true");
    expect(healthy.getAttribute("data-leaked")).toBe("false");
    expect(leaked.className).toContain("subprocess--leaked");
    expect(healthy.className).not.toContain("subprocess--leaked");
    const badge = leaked.querySelector('[data-testid="leaked-badge"]');
    expect(badge?.textContent).toBe("LEAKED");
    expect(badge?.getAttribute("role")).toBe("status");
    expect(healthy.querySelector('[data-testid="leaked-badge"]')).toBeNull();
  });

  it("renders nothing at all for a sweep with no jobs", () => {
    // Nearly every sweep. No empty list, no "no subprocesses" note — the
    // sweep's markup must be what it was before this feature.
    expect(computeSubprocessList([], NOW)).toBeNull();
  });

  it("degrades an unknown shape or start time rather than inventing one", () => {
    const rendered = computeSubprocessList(
      [{ hostId: "2am-elastic", jobId: "job-bare" }],
      NOW,
    )!;
    const [entry] = entries(rendered);
    expect(entry!.querySelector(".subprocess__shape")?.textContent).toBe("—");
    expect(entry!.querySelector(".subprocess__age")?.textContent).toBe("—");
  });
});

describe("instanceShapeText", () => {
  it("drops each part the launch record did not carry", () => {
    expect(instanceShapeText(job({ region: undefined }))).toBe("c7i.4xlarge · spot");
    expect(instanceShapeText(job({ spot: undefined }))).toBe("c7i.4xlarge · us-east-1");
    // `spot: false` is on-demand — the unremarkable default, so no chip. A
    // missing flag must read the same way, never as an assertion either way.
    expect(instanceShapeText(job({ spot: false }))).toBe("c7i.4xlarge · us-east-1");
  });
});

describe("runningForText", () => {
  it("measures from the emitter's started_at, not the backend's updatedAt", () => {
    // `updatedAt` is deliberately much more recent here: leak detection uses
    // it (a skewed emitter clock cannot fake liveness), but "how long has this
    // been billing" is a question only `started_at` answers.
    expect(runningForText(job({ startedAt: "2026-09-19T12:00:00Z", updatedAt: "2026-09-19T17:59:00Z" }), NOW)).toBe(
      "6h 0m",
    );
  });

  it("is unknown, not zero, for an absent or unparseable start", () => {
    expect(runningForText(job({ startedAt: undefined }), NOW)).toBe("—");
    expect(runningForText(job({ startedAt: "not-a-date" }), NOW)).toBe("—");
  });
});

describe("sortComputeJobs", () => {
  it("puts leaked jobs first, then the longest-running, then a stable id tiebreak", () => {
    const sorted = sortComputeJobs([
      job({ jobId: "b-recent", startedAt: "2026-09-19T17:00:00Z" }),
      job({ jobId: "a-old", startedAt: "2026-09-19T06:00:00Z" }),
      job({ jobId: "c-leaked", startedAt: "2026-09-19T17:30:00Z", leaked: true }),
      job({ jobId: "d-nostart", startedAt: undefined }),
    ]);
    expect(sorted.map((entry) => entry.jobId)).toEqual(["c-leaked", "a-old", "b-recent", "d-nostart"]);
  });

  it("does not mutate its input", () => {
    const input = [job({ jobId: "b" }), job({ jobId: "a", leaked: true })];
    sortComputeJobs(input);
    expect(input.map((entry) => entry.jobId)).toEqual(["b", "a"]);
  });
});

describe("fleet overview integration", () => {
  const snapshot = (jobs: unknown[]) => ({
    hosts: { "host-1": { health: { record: { kind: "host.health" }, updatedAt: NOW.toISOString() } } },
    activeSweeps: [],
    activeCompute: jobs,
  });

  it("surfaces the running-now panel and a leak count on the overview", () => {
    const view = buildFleetView(
      parseFleetSnapshot(snapshot([job({ jobId: "job-1" }), job({ jobId: "job-2", leaked: true })])),
      NOW,
    );
    expect(view.activeCompute).toHaveLength(2);
    expect(view.leakedCompute).toBe(1);

    const rendered = fleetOverviewView(view, NOW, { authenticated: true });
    expect(rendered.querySelector('[data-testid="running-compute"]')).not.toBeNull();
    expect(rendered.querySelector('[data-testid="fleet-compute-summary"]')?.textContent).toBe(
      "2 compute jobs · 1 possibly leaked",
    );
  });

  it("adds nothing to the overview at all on a fleet that runs no elastic compute", () => {
    // The common case on every Loom fleet that does not use this feature:
    // no panel AND no permanently-zero counter in the headline. `#/spend` is
    // where zero has an answer.
    const view = buildFleetView(parseFleetSnapshot(snapshot([])), NOW);
    const rendered = fleetOverviewView(view, NOW, { authenticated: true });
    expect(rendered.querySelector('[data-testid="running-compute"]')).toBeNull();
    expect(rendered.querySelector('[data-testid="fleet-compute-summary"]')).toBeNull();
    // Still the ordinary overview, not an error or an empty state.
    expect(rendered.getAttribute("data-testid")).toBe("fleet-overview");
    expect(rendered.querySelectorAll('[data-testid="host-card"]')).toHaveLength(1);
  });

  it("omits the compute count for a public viewer, since the count is itself withheld", () => {
    const view = buildFleetView(parseFleetSnapshot(snapshot([job(), job({ jobId: "j2" })])), NOW);
    const rendered = fleetOverviewView(view, NOW, { authenticated: false });
    expect(rendered.querySelector('[data-testid="fleet-compute-summary"]')).toBeNull();
    expect(rendered.textContent).not.toContain("2 compute jobs");
  });

  it("still shows running instances when no daemon host is reporting at all", () => {
    // A hostless elastic emitter pushes `ephemeral_compute` and nothing else,
    // so the fleet can legitimately have zero reporting hosts and live jobs.
    // Short-circuiting to the "no hosts" empty state would hide a leak.
    const view = buildFleetView(
      parseFleetSnapshot({ hosts: {}, activeSweeps: [], activeCompute: [job({ leaked: true })] }),
      NOW,
    );
    const rendered = fleetOverviewView(view, NOW, { authenticated: true });
    expect(rendered.querySelector('[data-testid="running-compute"]')).not.toBeNull();
    expect(rendered.querySelector('[data-testid="empty-fleet"]')).not.toBeNull();
  });
});

describe("parseFleetSnapshot — activeCompute", () => {
  it("drops an entry with no jobId rather than rendering it under a fabricated identity", () => {
    const parsed = parseFleetSnapshot({
      hosts: {},
      activeSweeps: [],
      activeCompute: [job(), { hostId: "2am-elastic" }, { jobId: "orphan" }, 42, null],
    });
    expect(parsed.activeCompute?.map((entry) => entry.jobId)).toEqual(["job-abc123"]);
  });

  it("treats an absent leaked flag as not-flagged, never as leaked", () => {
    const parsed = parseFleetSnapshot({
      hosts: {},
      activeSweeps: [],
      // A backend predating #8305 omits the field; a malformed one might send
      // a truthy non-boolean. Neither may paint a healthy job as a leak.
      activeCompute: [job({ jobId: "j1" }), { ...job({ jobId: "j2" }), leaked: "yes" }],
    });
    expect(parsed.activeCompute?.every((entry) => entry.leaked === undefined)).toBe(true);
  });

  it("degrades a wrong-typed activeCompute sub-tree to an empty list", () => {
    expect(parseFleetSnapshot({ hosts: {}, activeSweeps: [], activeCompute: "nope" }).activeCompute).toEqual([]);
    // A snapshot from a backend that predates the field parses the same way.
    expect(parseFleetSnapshot({ hosts: {}, activeSweeps: [] }).activeCompute).toEqual([]);
  });
});
