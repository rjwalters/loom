/**
 * The fleet work queue (Issue #8852, phase 3). The load-bearing assertions are
 * the issue's three acceptance criteria:
 *
 *  1. queued and in-progress work is visible across hosts;
 *  2. a blocked item shows its reason and its issue / PR links;
 *  3. an empty queue, a stale one and a missing one render differently.
 */

import { describe, expect, it } from "vitest";

import { buildFleetView } from "../src/fleet";
import { parseFleetSnapshot } from "../src/parse";
import { parseQueueSnapshot } from "../src/queueParse";
import { parseRoute, routeToHash } from "../src/router";
import { fleetOverviewView } from "../src/views/fleetOverview";
import { hostDetailView } from "../src/views/hostDetail";
import { hostQueuePanel, workQueueSummarySection, workQueueView } from "../src/views/workQueue";
import {
  fleetQueueTotals,
  filterQueueItems,
  mergeFleetQueue,
  openPrNumber,
  planText,
  queueFilterOptions,
  queueHealth,
  rankText,
  reasonText,
  summarizeHostQueue,
  QUEUE_STALE_AFTER_SEC,
} from "../src/workQueue";

const NOW = new Date("2026-09-25T12:10:00Z");

function minutesAgo(minutes: number): string {
  return new Date(NOW.getTime() - minutes * 60_000).toISOString();
}

function row(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    rank: 1,
    repo: "rjwalters/loom",
    visibility: "public",
    issue: 100,
    workspace_priority: 100,
    urgent: false,
    created_at: "2026-09-24T12:10:00Z",
    disposition: "deferred_capacity",
    state: "ready",
    reason: "waiting: concurrency cap full",
    ...overrides,
  };
}

function queue(rows: Record<string, unknown>[], updatedAgoMin = 2, extra: Record<string, unknown> = {}) {
  const counts = { running: 0, ready: 0, blocked: 0 } as Record<string, number>;
  for (const r of rows) counts[r.state as string] = (counts[r.state as string] ?? 0) + 1;
  return {
    record: {
      kind: "queue.snapshot",
      tick_at: minutesAgo(updatedAgoMin + 1),
      max_concurrent: 2,
      seen: rows.length,
      counts,
      listing_failed: [],
      listing_failed_unresolved: 0,
      rows,
      unresolved_rows: 0,
      rows_truncated: 0,
      ...extra,
    },
    updatedAt: minutesAgo(updatedAgoMin),
  };
}

function health(agoMin = 1) {
  return { record: { kind: "host.health", captured_at: minutesAgo(agoMin) }, updatedAt: minutesAgo(agoMin) };
}

/** host-a runs #100 while host-b sees it held by a peer; host-b also has a
 * blocked #200 (open PR) and a ready #300; host-c is idle; host-d is stale;
 * host-e never sent a queue. */
function fleet() {
  return parseFleetSnapshot({
    hosts: {
      "host-a": {
        health: health(),
        queue: queue([row({ disposition: "in_flight", state: "running", reason: "sweep already running" })]),
      },
      "host-b": {
        health: health(),
        queue: queue([
          row({ disposition: "peer_claim", state: "blocked", reason: "held by a peer host" }),
          row({
            rank: 2,
            issue: 200,
            disposition: "open_pr",
            state: "blocked",
            reason: "blocked: open linked PR",
            detail: "open PR #201",
          }),
          row({ rank: 3, issue: 300, operator_priority: true }),
        ]),
      },
      "host-c": { health: health(), queue: queue([]) },
      "host-d": { health: health(), queue: queue([row({ issue: 400 })], QUEUE_STALE_AFTER_SEC / 60 + 5) },
      "host-e": { health: health() },
    },
    activeSweeps: [
      {
        hostId: "host-a",
        sweepId: "sweep-issue-100-0",
        repo: "rjwalters/loom",
        issue: 100,
        phase: "judge",
        startedAt: minutesAgo(30),
        updatedAt: minutesAgo(1),
      },
    ],
  });
}

function view() {
  return buildFleetView(fleet(), NOW);
}

describe("parseQueueSnapshot", () => {
  it("rejects a record without a parseable tick_at and defaults visibility to private", () => {
    expect(parseQueueSnapshot({ rows: [] })).toBeUndefined();
    const parsed = parseQueueSnapshot({ tick_at: minutesAgo(1), rows: [{ rank: 1, visibility: "PUBLIC-ish" }] });
    expect(parsed?.rows[0]?.visibility).toBe("private");
    expect(parsed?.rows[0]?.state).toBe("unknown");
  });

  it("is carried through parseFleetSnapshot onto the host entry", () => {
    expect(fleet().hosts["host-b"]?.queue?.record.rows).toHaveLength(3);
    expect(fleet().hosts["host-e"]?.queue).toBeUndefined();
  });
});

describe("queueHealth — empty vs stale vs absent (AC 3)", () => {
  it("tells the four states apart", () => {
    const hosts = fleet().hosts;
    expect(queueHealth(hosts["host-b"]?.queue, NOW)).toBe("active");
    expect(queueHealth(hosts["host-c"]?.queue, NOW)).toBe("idle");
    expect(queueHealth(hosts["host-d"]?.queue, NOW)).toBe("stale");
    expect(queueHealth(hosts["host-e"]?.queue, NOW)).toBe("absent");
  });

  it("goes offline after four hours", () => {
    expect(queueHealth(parseFleetSnapshot({ hosts: { h: { queue: queue([], 5 * 60) } } }).hosts.h?.queue, NOW)).toBe(
      "offline",
    );
  });

  it("totals only hosts whose queue is current", () => {
    const summaries = view().hosts.map((host) => summarizeHostQueue(host, NOW));
    const totals = fleetQueueTotals(summaries);
    expect(totals).toMatchObject({ backlog: 4, running: 1, ready: 1, blocked: 2, currentHosts: 3, staleHosts: 1 });
  });
});

describe("mergeFleetQueue", () => {
  it("folds one issue seen by two hosts into the most advanced observation", () => {
    const v = view();
    const summaries = v.hosts.map((host) => summarizeHostQueue(host, NOW));
    const items = mergeFleetQueue(summaries, v.hosts.flatMap((host) => host.sweeps));
    const issue100 = items.filter((item) => item.issue === 100);
    expect(issue100).toHaveLength(1);
    expect(issue100[0]?.state).toBe("running");
    expect(issue100[0]?.primary.hostId).toBe("host-a");
    expect(issue100[0]?.others.map((o) => o.hostId)).toEqual(["host-b"]);
    expect(issue100[0]?.sweep?.phase).toBe("judge");
    // Stale hosts still contribute rows (flagged by their host badge);
    // running first, then starred ready (#9244), then by age.
    expect(items.map((item) => item.issue)).toEqual([100, 300, 400, 200]);
  });

  it("parses the daemon's open-PR detail", () => {
    expect(openPrNumber({ rank: 1, visibility: "public", urgent: false, disposition: "open_pr", state: "blocked", reason: "", detail: "open PR #201" })).toBe(201);
    expect(openPrNumber({ rank: 1, visibility: "public", urgent: false, disposition: "parked", state: "blocked", reason: "", detail: "loom:blocked" })).toBeUndefined();
  });
});

describe("workQueueSummarySection", () => {
  it("renders nothing for a fleet where no host reports a queue", () => {
    const v = buildFleetView(parseFleetSnapshot({ hosts: { h: { health: health() } }, activeSweeps: [] }), NOW);
    expect(workQueueSummarySection(v, NOW)).toBeNull();
    expect(fleetOverviewView(v, NOW, { authenticated: true }).querySelector('[data-testid="work-queue-summary"]')).toBeNull();
  });

  it("gives every host a row whose freshness badge names its state (AC 1, 3)", () => {
    const section = fleetOverviewView(view(), NOW, { authenticated: true }).querySelector<HTMLElement>(
      '[data-testid="work-queue-summary"]',
    );
    expect(section).not.toBeNull();
    const health = Object.fromEntries(
      [...section!.querySelectorAll<HTMLElement>('[data-testid="queue-host-row"]')].map((tr) => [
        tr.dataset.host,
        tr.dataset.health,
      ]),
    );
    expect(health).toEqual({
      "host-a": "active",
      "host-b": "active",
      "host-c": "idle",
      "host-d": "stale",
      "host-e": "absent",
    });
    expect(section!.querySelector('[data-testid="work-queue-totals"]')?.textContent).toContain("1 stale host excluded");
  });
});

describe("workQueueView — the #/queue route", () => {
  it("is routable", () => {
    expect(parseRoute("#/queue")).toEqual({ name: "queue" });
    expect(routeToHash({ name: "queue" })).toBe("#/queue");
  });

  it("lists running work with its host and phase (AC 1)", () => {
    const page = workQueueView(view(), NOW);
    const running = page.querySelector('[data-testid="queue-list-running"]')!;
    const item = running.querySelector<HTMLElement>('[data-testid="queue-item"]')!;
    expect(item.dataset.host).toBe("host-a");
    expect(item.textContent).toContain("judge, running 30m");
  });

  // #9032: the merged lists previously showed no dispatch position at all —
  // only the per-host detail panel (`hostQueuePanel`) did.
  it("shows each merged item's dispatch rank, matching the host detail's rankText convention", () => {
    const page = workQueueView(view(), NOW);
    const running = page.querySelector<HTMLElement>('[data-testid="queue-item"][data-issue="100"]')!;
    expect(running.dataset.rank).toBe("1");
    expect(running.querySelector(".queue__rank")?.textContent).toBe("1");
    const ready = page.querySelector<HTMLElement>('[data-testid="queue-item"][data-issue="300"]')!;
    expect(ready.dataset.rank).toBe("3");
  });

  it("shows a dash for an unranked (forge-side labelled_blocked) row, not 0", () => {
    const v = buildFleetView(
      parseFleetSnapshot({
        hosts: {
          h: {
            health: health(),
            queue: queue([row({ rank: 0, issue: 500, disposition: "labelled_blocked", state: "blocked" })]),
          },
        },
        activeSweeps: [],
      }),
      NOW,
    );
    const item = workQueueView(v, NOW).querySelector<HTMLElement>('[data-testid="queue-item"][data-issue="500"]')!;
    expect(item.dataset.rank).toBe("0");
    expect(item.querySelector(".queue__rank")?.textContent).toBe("–");
  });

  it("shows a blocked item's reason with issue and PR links (AC 2)", () => {
    const blocked = workQueueView(view(), NOW).querySelector('[data-testid="queue-list-blocked"]')!;
    const item = blocked.querySelector<HTMLElement>('[data-issue="200"]')!;
    expect(item.textContent).toContain("blocked: open linked PR (open PR #201)");
    const hrefs = [...item.querySelectorAll("a")].map((a) => a.getAttribute("href"));
    expect(hrefs).toContain("https://github.com/rjwalters/loom/issues/200");
    expect(hrefs).toContain("https://github.com/rjwalters/loom/pull/201");
  });

  it("renders a private row from the public view without repo detail", () => {
    const v = buildFleetView(
      parseFleetSnapshot({
        hosts: {
          h: {
            health: health(),
            queue: queue(
              [{ rank: 1, visibility: "private", urgent: false, disposition: "parked", state: "blocked", reason: "blocked: skip/park label" }],
              2,
              { withheld_rows: 1 },
            ),
          },
        },
        activeSweeps: [],
      }),
      NOW,
    );
    const page = workQueueView(v, NOW);
    expect(page.textContent).toContain("private issue");
    expect(page.textContent).toContain("1 row from private repositories");
  });
});

describe("hostQueuePanel", () => {
  it("says so when the host never sent a queue", () => {
    const host = view().hosts.find((h) => h.hostId === "host-e")!;
    const panel = hostQueuePanel(host, NOW);
    expect(panel.dataset.health).toBe("absent");
    expect(panel.textContent).toContain("No queue telemetry from this host");
  });

  it("says a stale queue is last-known, not current", () => {
    const host = view().hosts.find((h) => h.hostId === "host-d")!;
    expect(hostQueuePanel(host, NOW).textContent).toContain("last-known, not current");
  });

  it("lists the host's rows in dispatch order on host detail", () => {
    const host = view().hosts.find((h) => h.hostId === "host-b")!;
    const detail = hostDetailView(host, NOW);
    const ranks = [...detail.querySelectorAll<HTMLElement>('[data-testid="host-queue-row"]')].map((tr) => tr.dataset.rank);
    expect(ranks).toEqual(["1", "2", "3"]);
  });
});

/** A single host listing two repos and two tiers — enough to exercise the
 * repo/tier filter's narrowing and its AND semantics (Issue #9032). All three
 * rows default to `state: "ready"` so they land in the same list. */
function multiRepoView() {
  return buildFleetView(
    parseFleetSnapshot({
      hosts: {
        h: {
          health: health(),
          queue: queue([
            row({ issue: 1, repo: "acme/one", tier: "tier:goal-advancing" }),
            row({ issue: 2, repo: "acme/two", tier: "tier:maintenance", rank: 2 }),
            row({ issue: 3, repo: "acme/one", rank: 3 }),
          ]),
        },
      },
      activeSweeps: [],
    }),
    NOW,
  );
}

describe("queueFilterOptions / filterQueueItems (Issue #9032)", () => {
  function items() {
    const v = multiRepoView();
    return mergeFleetQueue(
      v.hosts.map((host) => summarizeHostQueue(host, NOW)),
      [],
    );
  }

  it("collects distinct repos and tiers across the merged items, sorted", () => {
    expect(queueFilterOptions(items())).toEqual({
      repos: ["acme/one", "acme/two"],
      tiers: ["tier:goal-advancing", "tier:maintenance"],
    });
  });

  it("filters by repo and by tier independently, ANDed when both are set", () => {
    const all = items();
    expect(filterQueueItems(all, {}).map((i) => i.issue)).toEqual([1, 2, 3]);
    expect(filterQueueItems(all, { repo: "acme/one" }).map((i) => i.issue)).toEqual([1, 3]);
    expect(filterQueueItems(all, { tier: "tier:maintenance" }).map((i) => i.issue)).toEqual([2]);
    expect(filterQueueItems(all, { repo: "acme/one", tier: "tier:goal-advancing" }).map((i) => i.issue)).toEqual([1]);
    expect(filterQueueItems(all, { repo: "acme/one", tier: "tier:maintenance" }).map((i) => i.issue)).toEqual([]);
  });
});

describe("#/queue filter controls (Issue #9032)", () => {
  function issueDatasets(page: HTMLElement): string[] {
    return [...page.querySelectorAll<HTMLElement>('[data-testid="queue-item"]')].map((el) => el.dataset.issue ?? "");
  }

  it("narrows the merged lists to the selected repo, client-side, without touching the other items", () => {
    window.localStorage.clear();
    const page = workQueueView(multiRepoView(), NOW);
    expect(issueDatasets(page).sort()).toEqual(["1", "2", "3"]);

    const repoSelect = page.querySelector<HTMLSelectElement>('[data-testid="queue-filter-repo"]')!;
    repoSelect.value = "acme/two";
    repoSelect.dispatchEvent(new Event("change"));

    expect(issueDatasets(page)).toEqual(["2"]);
    window.localStorage.clear();
  });

  it("narrows the merged lists to the selected tier", () => {
    window.localStorage.clear();
    const page = workQueueView(multiRepoView(), NOW);
    const tierSelect = page.querySelector<HTMLSelectElement>('[data-testid="queue-filter-tier"]')!;
    tierSelect.value = "tier:goal-advancing";
    tierSelect.dispatchEvent(new Event("change"));

    expect(issueDatasets(page)).toEqual(["1"]);
    window.localStorage.clear();
  });

  it("persists the selected repo filter across a fresh render — the app re-invokes workQueueView on every poll tick", () => {
    window.localStorage.clear();
    const first = workQueueView(multiRepoView(), NOW);
    const firstSelect = first.querySelector<HTMLSelectElement>('[data-testid="queue-filter-repo"]')!;
    firstSelect.value = "acme/one";
    firstSelect.dispatchEvent(new Event("change"));

    const second = workQueueView(multiRepoView(), NOW);
    const secondSelect = second.querySelector<HTMLSelectElement>('[data-testid="queue-filter-repo"]')!;
    expect(secondSelect.value).toBe("acme/one");
    expect(issueDatasets(second)).toEqual(["1", "3"]);
    window.localStorage.clear();
  });

  it("falls back to All when the persisted repo no longer appears among the merged items", () => {
    window.localStorage.clear();
    const first = workQueueView(multiRepoView(), NOW);
    const firstSelect = first.querySelector<HTMLSelectElement>('[data-testid="queue-filter-repo"]')!;
    firstSelect.value = "acme/two";
    firstSelect.dispatchEvent(new Event("change"));

    const onlyOneRepo = buildFleetView(
      parseFleetSnapshot({
        hosts: { h: { health: health(), queue: queue([row({ issue: 1, repo: "acme/one" })]) } },
        activeSweeps: [],
      }),
      NOW,
    );
    const second = workQueueView(onlyOneRepo, NOW);
    const secondSelect = second.querySelector<HTMLSelectElement>('[data-testid="queue-filter-repo"]')!;
    expect(secondSelect.value).toBe("");
    expect(issueDatasets(second)).toEqual(["1"]);
    window.localStorage.clear();
  });
});

describe("rankText — forge-side labelled_blocked rows (#8957)", () => {
  it("shows an unranked row as a dash and keeps its reason and hold labels", () => {
    const parsed = parseQueueSnapshot({
      tick_at: minutesAgo(1),
      rows: [row({ rank: 0, disposition: "labelled_blocked", state: "blocked", reason: "blocked: labelled loom:blocked", detail: "loom:operator" })],
    })!;
    const blocked = parsed.rows[0]!;
    expect(blocked.rank).toBe(0);
    expect(rankText(blocked)).toBe("–");
    expect(rankText({ ...blocked, rank: 3 })).toBe("3");
    expect(reasonText(blocked)).toBe("blocked: labelled loom:blocked (loom:operator)");
  });
});

describe("planText — the host's own dispatch plan (#9288)", () => {
  it("renders position, plan_state and gate verbatim, and falls back for an older daemon", () => {
    const parsed = parseQueueSnapshot({
      tick_at: minutesAgo(1),
      rows: [
        row({ rank: 4, disposition: "deferred_repo_cap", state: "ready", position: 2, plan_state: "queued", gate: "repo_cap" }),
        row({ rank: 1, disposition: "parked", state: "blocked", plan_state: "blocked" }),
        row({ rank: 2, disposition: "deferred_capacity", state: "ready" }),
        row({ rank: 3, disposition: "deferred_capacity", state: "ready", position: 1, plan_state: "someday" }),
      ],
    })!;
    const [capped, parked, legacy, future] = parsed.rows;
    expect(planText(capped!)).toBe("#2 queued (repo cap)");
    expect(planText(parked!)).toBe("blocked");
    expect(planText(legacy!)).toBe("ready");
    expect(future!.plan_state).toBe("unknown");
    // `rank` is untouched by the plan.
    expect(rankText(capped!)).toBe("4");
  });

  it("the host detail panel shows each row's plan", () => {
    const snapshot = parseFleetSnapshot({
      hosts: {
        "host-plan": {
          health: health(),
          queue: queue([row({ position: 1, plan_state: "next", gate: "capacity" })]),
        },
      },
    });
    const host = buildFleetView(snapshot, NOW).hosts.find((h) => h.hostId === "host-plan")!;
    expect(hostQueuePanel(host, NOW).textContent).toContain("#1 next (capacity)");
  });
});
