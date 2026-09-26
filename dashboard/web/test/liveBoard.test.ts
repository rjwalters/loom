/**
 * The `#/live` status board (issue #9077): the DOM-free view model, and the
 * keyed view's one real promise — that updates patch nodes in place instead
 * of rebuilding them, so animations are not restarted on every poll and tick.
 */

import { describe, expect, it } from "vitest";

import { buildFleetView } from "../src/fleet";
import {
  allSweeps,
  boardTiles,
  describeEvent,
  formatStopwatch,
  phaseIndex,
  sweepTitle,
} from "../src/liveBoard";
import type { ActiveSweep, FleetSnapshot, LiveTailFrame } from "../src/types";
import { LiveBoard, MAX_CARDS, TICKER_MAX_ROWS } from "../src/views/liveBoard";

const NOW = new Date("2026-09-26T12:00:00Z");

function sweep(overrides: Partial<ActiveSweep> & { sweepId: string }): ActiveSweep {
  return {
    hostId: "mac-1",
    repo: "rjwalters/loom",
    visibility: "public",
    issue: 100,
    phase: "builder",
    startedAt: "2026-09-26T11:50:00Z",
    enteredPhaseAt: "2026-09-26T11:58:00Z",
    model: "opus",
    ...overrides,
  };
}

function snapshot(sweeps: ActiveSweep[], extra: Partial<FleetSnapshot> = {}): FleetSnapshot {
  return {
    hosts: {
      "mac-1": {
        health: {
          updatedAt: "2026-09-26T11:59:00Z",
          record: { kind: "host.health", load_per_core: 0.5 },
        },
        tokens: {
          updatedAt: "2026-09-26T11:59:00Z",
          record: { kind: "tokens.snapshot", accounts: [{ account: "a", usage_fraction: 0.42, exhausted: false }] },
        },
      },
    },
    activeSweeps: sweeps,
    ...extra,
  };
}

function frame(topic: string, record: Record<string, unknown>, hostId = "mac-1"): LiveTailFrame {
  return {
    topic,
    event: {
      hostId,
      emittedAt: "2026-09-26T11:59:50Z",
      schemaVersion: 1,
      record: { kind: topic, ...record },
    },
  };
}

/** Collects scheduled callbacks so a test can run them on demand. */
function manualSchedule(): { schedule: (handler: () => void, ms: number) => void; flush: () => void; delays: number[] } {
  const queue: (() => void)[] = [];
  const delays: number[] = [];
  return {
    schedule: (handler, ms) => {
      queue.push(handler);
      delays.push(ms);
    },
    flush: () => {
      for (const handler of queue.splice(0)) handler();
    },
    delays,
  };
}

function cardKeys(board: LiveBoard): string[] {
  return [...board.root.querySelectorAll<HTMLElement>('[data-testid="live-card"]')].map((node) => node.dataset.key ?? "");
}

function card(board: LiveBoard, key: string): HTMLElement {
  const found = board.root.querySelector<HTMLElement>(`[data-testid="live-card"][data-key="${key}"]`);
  if (!found) throw new Error(`no card ${key}`);
  return found;
}

describe("live board model", () => {
  it("places each lifecycle phase on the stepper", () => {
    expect(phaseIndex("curator")).toBe(0);
    expect(phaseIndex("Judge")).toBe(2);
    expect(phaseIndex("merge")).toBe(4);
    expect(phaseIndex(undefined)).toBe(-1);
    expect(phaseIndex("something-new")).toBe(-1);
  });

  it("formats a ticking stopwatch", () => {
    expect(formatStopwatch(0)).toBe("0:00");
    expect(formatStopwatch(65)).toBe("1:05");
    expect(formatStopwatch(3723)).toBe("1:02:03");
    expect(formatStopwatch(-4)).toBe("0:00");
    expect(formatStopwatch(undefined)).toBe("—");
  });

  it("titles a sweep by repo short name and issue, tolerating redaction", () => {
    expect(sweepTitle("rjwalters/loom", 42)).toBe("loom#42");
    expect(sweepTitle(undefined, 42)).toBe("#42");
    expect(sweepTitle(undefined, undefined)).toBe("sweep");
  });

  it("orders sweeps fleet-wide, longest-running first", () => {
    const view = buildFleetView(
      snapshot([
        sweep({ sweepId: "b", startedAt: "2026-09-26T11:55:00Z" }),
        sweep({ sweepId: "a", hostId: "mac-2", startedAt: "2026-09-26T11:40:00Z" }),
      ]),
      NOW,
    );
    expect(allSweeps(view).map((s) => s.sweepId)).toEqual(["a", "b"]);
  });

  it("renders unknown queue and token data as undefined, not zero", () => {
    const tiles = boardTiles(buildFleetView({ hosts: {}, activeSweeps: [] }, NOW), NOW);
    expect(tiles.ready).toBeUndefined();
    expect(tiles.blocked).toBeUndefined();
    expect(tiles.peakUsage).toBeUndefined();
    expect(tiles.running).toBe(0);
  });

  it("counts online hosts, running sweeps and peak token use", () => {
    const tiles = boardTiles(buildFleetView(snapshot([sweep({ sweepId: "a" })]), NOW), NOW);
    expect(tiles.hostsOnline).toBe(1);
    expect(tiles.hostsTotal).toBe(1);
    expect(tiles.running).toBe(1);
    expect(tiles.peakUsage).toBeCloseTo(0.42);
  });

  it("describes sweep events as ticker sentences", () => {
    expect(describeEvent(frame("sweep.phase", { repo: "rjwalters/loom", issue: 7, phase: "judge" }))).toMatchObject({
      subject: "loom#7",
      text: "entered judge",
      tone: "phase",
    });
    expect(describeEvent(frame("sweep.completed", { issue: 7, result: "failure" }))).toMatchObject({
      text: "completed · failure",
      tone: "bad",
    });
    expect(describeEvent(frame("sweep.outcome", { issue: 7, result: "success", pr_number: 9 }))).toMatchObject({
      text: "opened PR #9",
      tone: "ok",
    });
    expect(describeEvent(frame("ephemeral_compute", { ended_at: "x", instance_type: "m5.large" }))).toMatchObject({
      subject: undefined,
      text: "compute finished · m5.large",
    });
  });

  it("keeps housekeeping records off the ticker", () => {
    expect(describeEvent(frame("host.health", {}))).toBeNull();
    expect(describeEvent(frame("tokens.snapshot", {}))).toBeNull();
    // An outcome without a PR repeats what sweep.completed already said.
    expect(describeEvent(frame("sweep.outcome", { result: "success" }))).toBeNull();
  });
});

describe("LiveBoard view", () => {
  it("renders one card per sweep with the current phase on the stepper", () => {
    const board = new LiveBoard(manualSchedule().schedule);
    board.update(buildFleetView(snapshot([sweep({ sweepId: "s1", phase: "judge" })]), NOW), NOW);

    const card = board.root.querySelector<HTMLElement>('[data-testid="live-card"]')!;
    expect(card.dataset.phase).toBe("judge");
    const states = [...card.querySelectorAll<HTMLElement>(".live-step")].map((step) => step.dataset.state);
    expect(states).toEqual(["done", "done", "current", "todo", "todo"]);
    expect(card.querySelector(".live-card__title")?.getAttribute("href")).toBe(
      "https://github.com/rjwalters/loom/issues/100",
    );
    // 10 minutes since startedAt.
    expect(card.querySelector('[data-testid="live-card-timer"]')?.textContent).toBe("10:00");
    expect(board.root.querySelector<HTMLElement>('[data-testid="live-idle"]')!.hidden).toBe(true);
  });

  it("advances timers on tick without replacing any node", () => {
    const board = new LiveBoard(manualSchedule().schedule);
    board.update(buildFleetView(snapshot([sweep({ sweepId: "s1" })]), NOW), NOW);
    const card = board.root.querySelector('[data-testid="live-card"]');
    const timer = board.root.querySelector('[data-testid="live-card-timer"]');

    board.tick(new Date(NOW.getTime() + 5_000));

    expect(board.root.querySelector('[data-testid="live-card"]')).toBe(card);
    expect(timer?.textContent).toBe("10:05");
  });

  it("patches a surviving card in place across polls, and flashes a phase change", () => {
    const board = new LiveBoard(manualSchedule().schedule);
    board.update(buildFleetView(snapshot([sweep({ sweepId: "s1", phase: "builder" })]), NOW), NOW);
    const card = board.root.querySelector<HTMLElement>('[data-testid="live-card"]')!;
    expect(card.classList.contains("is-advanced")).toBe(false);

    board.update(buildFleetView(snapshot([sweep({ sweepId: "s1", phase: "judge" })]), NOW), NOW);

    expect(board.root.querySelector('[data-testid="live-card"]')).toBe(card);
    expect(card.dataset.phase).toBe("judge");
    expect(card.classList.contains("is-advanced")).toBe(true);
  });

  it("animates in a sweep that starts while watching, but not the first paint", () => {
    const board = new LiveBoard(manualSchedule().schedule);
    board.update(buildFleetView(snapshot([sweep({ sweepId: "s1" })]), NOW), NOW);
    expect(board.root.querySelector(".live-card.is-entering")).toBeNull();

    board.update(
      buildFleetView(
        snapshot([sweep({ sweepId: "s1" }), sweep({ sweepId: "s2", issue: 101, startedAt: "2026-09-26T11:59:00Z" })]),
        NOW,
      ),
      NOW,
    );
    expect(board.root.querySelector('[data-sweep="s2"]')?.classList.contains("is-entering")).toBe(true);
  });

  it("keeps a finished sweep's card in place, dimmed, with how it ended", () => {
    const board = new LiveBoard(manualSchedule().schedule);
    board.update(
      buildFleetView(snapshot([sweep({ sweepId: "s1", issue: 1 }), sweep({ sweepId: "s2", issue: 2 })]), NOW),
      NOW,
    );
    board.pushEvent(frame("sweep.completed", { repo: "rjwalters/loom", issue: 1, sweep_id: "s1", result: "success" }), NOW);
    board.update(buildFleetView(snapshot([sweep({ sweepId: "s2", issue: 2 })]), NOW), NOW);

    const cards = cardKeys(board);
    expect(cards).toEqual(["rjwalters/loom#1", "rjwalters/loom#2"]);
    const done = card(board, "rjwalters/loom#1");
    expect(done.dataset.state).toBe("done");
    expect(done.dataset.result).toBe("success");
    expect(done.querySelector(".live-card__phase")?.textContent).toBe("finished · success");
    // Its clock stops where it stood.
    expect(done.querySelector<HTMLElement>('[data-testid="live-card-timer"]')!.dataset.since).toBeUndefined();
    // Only the running sweep counts as in flight.
    expect(board.root.querySelector(".live__section-count")?.textContent).toBe("1");
  });

  it("never reorders cards: new ones go last, a re-dispatch reuses its card", () => {
    const board = new LiveBoard(manualSchedule().schedule);
    board.update(
      buildFleetView(
        snapshot([
          sweep({ sweepId: "s1", issue: 1, startedAt: "2026-09-26T11:50:00Z" }),
          sweep({ sweepId: "s2", issue: 2, startedAt: "2026-09-26T11:55:00Z" }),
        ]),
        NOW,
      ),
      NOW,
    );
    const first = card(board, "rjwalters/loom#1");

    // A sweep that started earlier than both arrives late: it still goes last.
    board.update(
      buildFleetView(
        snapshot([
          sweep({ sweepId: "s0", issue: 3, startedAt: "2026-09-26T11:00:00Z" }),
          sweep({ sweepId: "s1", issue: 1, startedAt: "2026-09-26T11:50:00Z" }),
          sweep({ sweepId: "s2", issue: 2, startedAt: "2026-09-26T11:55:00Z" }),
        ]),
        NOW,
      ),
      NOW,
    );
    expect(cardKeys(board)).toEqual(["rjwalters/loom#1", "rjwalters/loom#2", "rjwalters/loom#3"]);

    // Issue 1 finishes, then a new sweep picks it up again.
    board.update(buildFleetView(snapshot([sweep({ sweepId: "s2", issue: 2 })]), NOW), NOW);
    board.update(
      buildFleetView(snapshot([sweep({ sweepId: "s2", issue: 2 }), sweep({ sweepId: "s9", issue: 1, phase: "judge" })]), NOW),
      NOW,
    );
    expect(card(board, "rjwalters/loom#1")).toBe(first);
    expect(first.dataset.state).toBe("active");
    expect(first.dataset.sweep).toBe("s9");
    expect(first.dataset.phase).toBe("judge");
    expect(cardKeys(board)).toEqual(["rjwalters/loom#1", "rjwalters/loom#2", "rjwalters/loom#3"]);
  });

  it("removes finished cards only on Clear finished", () => {
    const board = new LiveBoard(manualSchedule().schedule);
    board.update(
      buildFleetView(snapshot([sweep({ sweepId: "s1", issue: 1 }), sweep({ sweepId: "s2", issue: 2 })]), NOW),
      NOW,
    );
    const clear = board.root.querySelector<HTMLButtonElement>('[data-testid="live-clear-finished"]')!;
    expect(clear.hidden).toBe(true);

    board.update(buildFleetView(snapshot([sweep({ sweepId: "s2", issue: 2 })]), NOW), NOW);
    expect(clear.hidden).toBe(false);
    expect(cardKeys(board)).toHaveLength(2);

    clear.click();
    expect(cardKeys(board)).toEqual(["rjwalters/loom#2"]);
    expect(clear.hidden).toBe(true);
  });

  it("drops the oldest finished card past MAX_CARDS, never an active one", () => {
    const board = new LiveBoard(manualSchedule().schedule);
    const many = (from: number, count: number) =>
      Array.from({ length: count }, (_, i) => sweep({ sweepId: `s${from + i}`, issue: from + i }));
    board.update(buildFleetView(snapshot(many(1, MAX_CARDS)), NOW), NOW);
    // Issue 1 stays running; every other one finishes.
    board.update(buildFleetView(snapshot(many(1, 1)), NOW), NOW);
    const before = cardKeys(board);
    board.update(buildFleetView(snapshot([...many(1, 1), ...many(1000, 1)]), NOW), NOW);

    // The oldest card is still running, so the next-oldest goes.
    expect(cardKeys(board)).toEqual([before[0], ...before.slice(2), "rjwalters/loom#1000"]);
    expect(before[0]).toBe("rjwalters/loom#1");
  });

  it("flashes a tile only when its value changes after the first paint", () => {
    const board = new LiveBoard(manualSchedule().schedule);
    const running = () => board.root.querySelector<HTMLElement>('[data-testid="live-tile-running"]')!;
    board.update(buildFleetView(snapshot([sweep({ sweepId: "s1" })]), NOW), NOW);
    expect(running().classList.contains("is-changed")).toBe(false);
    expect(running().querySelector(".live-tile__value")?.textContent).toBe("1");

    board.update(buildFleetView(snapshot([sweep({ sweepId: "s1" })]), NOW), NOW);
    expect(running().classList.contains("is-changed")).toBe(false);

    board.update(
      buildFleetView(snapshot([sweep({ sweepId: "s1" }), sweep({ sweepId: "s2" })]), NOW),
      NOW,
    );
    expect(running().classList.contains("is-changed")).toBe(true);
  });

  it("shows unknown tiles as a dash, never zero", () => {
    const board = new LiveBoard(manualSchedule().schedule);
    board.update(buildFleetView({ hosts: {}, activeSweeps: [] }, NOW), NOW);
    const value = (key: string) =>
      board.root.querySelector(`[data-testid="live-tile-${key}"] .live-tile__value`)?.textContent;
    expect(value("ready")).toBe("—");
    expect(value("tokens")).toBe("—");
  });

  it("keys host pills and links them to the host drill-down", () => {
    const board = new LiveBoard(manualSchedule().schedule);
    board.update(buildFleetView(snapshot([sweep({ sweepId: "s1" })]), NOW), NOW);
    const pill = board.root.querySelector<HTMLAnchorElement>('[data-testid="live-host"]')!;
    expect(pill.getAttribute("href")).toBe("#/hosts/mac-1");
    expect(pill.dataset.status).toBe("ok");
    expect(pill.dataset.busy).toBe("true");

    board.update(buildFleetView(snapshot([]), NOW), NOW);
    expect(board.root.querySelector('[data-testid="live-host"]')).toBe(pill);
    expect(pill.dataset.busy).toBe("false");
  });

  it("puts news on the ticker newest-first, capped, and beats the host pill for any event", () => {
    const board = new LiveBoard(manualSchedule().schedule);
    board.update(buildFleetView(snapshot([]), NOW), NOW);

    board.pushEvent(frame("host.health", {}), NOW);
    expect(board.root.querySelectorAll('[data-testid="live-event"]')).toHaveLength(0);
    expect(board.root.querySelector('[data-testid="live-host"]')?.classList.contains("is-beat")).toBe(true);

    for (let i = 0; i < TICKER_MAX_ROWS + 5; i += 1) {
      board.pushEvent(frame("sweep.phase", { issue: i, phase: "judge" }), NOW);
    }
    const rows = board.root.querySelectorAll('[data-testid="live-event"]');
    expect(rows).toHaveLength(TICKER_MAX_ROWS);
    expect(rows[0]?.textContent).toContain(`#${TICKER_MAX_ROWS + 4}`);
  });

  it("reflects the SSE connection state on the beacon", () => {
    const board = new LiveBoard(manualSchedule().schedule);
    const beacon = board.root.querySelector<HTMLElement>('[data-testid="live-beacon"]')!;
    board.setConnection("open");
    expect(beacon.dataset.connection).toBe("open");
    expect(beacon.textContent).toBe("Live");
    board.setConnection("closed");
    expect(beacon.textContent).toBe("Reconnecting");
  });

  it("shows an error banner over the last good data, and clears it", () => {
    const board = new LiveBoard(manualSchedule().schedule);
    board.update(buildFleetView(snapshot([sweep({ sweepId: "s1" })]), NOW), NOW);
    board.setError("HTTP 502");
    const banner = board.root.querySelector<HTMLElement>('[data-testid="live-error"]')!;
    expect(banner.hidden).toBe(false);
    expect(banner.textContent).toContain("HTTP 502");
    expect(board.root.querySelector('[data-testid="live-card"]')).not.toBeNull();
    board.setError(null);
    expect(banner.hidden).toBe(true);
  });
});
