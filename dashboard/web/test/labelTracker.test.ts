/**
 * Label transitions for the Live board (issue #9094): the `labels.snapshot`
 * parser, the snapshot differ, and the board's label chips, history, ticker
 * lines and label-only cards.
 */

import { describe, expect, it } from "vitest";

import { buildFleetView } from "../src/fleet";
import { parseLabelsSnapshot } from "../src/labelParse";
import { LABEL_HISTORY_MAX, LabelTracker, describeTransition, labelTone, shortLabel } from "../src/labelTracker";
import type { LabelItem, LabelsSnapshotRecord } from "../src/labelTypes";
import { parseFleetSnapshot } from "../src/parse";
import type { ActiveSweep, FleetSnapshot, LiveTailFrame } from "../src/types";
import { LiveBoard } from "../src/views/liveBoard";

const REPO = "rjwalters/loom";
const NOW = new Date("2026-09-26T12:00:00Z");

function labels(takenAt: string, items: LabelItem[], extra: Partial<LabelsSnapshotRecord> = {}): LabelsSnapshotRecord {
  return { kind: "labels.snapshot", repo: REPO, visibility: "public", taken_at: takenAt, items, ...extra };
}

const issue = (number: number, ...names: string[]): LabelItem => ({ type: "issue", number, labels: names });
const pr = (number: number, closes: number[], ...names: string[]): LabelItem => ({
  type: "pr",
  number,
  labels: names,
  ...(closes.length > 0 ? { closes } : {}),
});

describe("parseLabelsSnapshot", () => {
  it("keeps well-formed items and drops the rest", () => {
    const record = parseLabelsSnapshot({
      kind: "labels.snapshot",
      repo: REPO,
      visibility: "public",
      taken_at: "2026-09-26T11:00:00Z",
      items: [
        { type: "issue", number: 5, labels: ["loom:issue", 7, ""] },
        { type: "pr", number: 9, labels: ["loom:pr"], closes: [5, -1, "x"] },
        { type: "commit", number: 1, labels: [] },
        { type: "issue", number: 0, labels: [] },
        "junk",
      ],
      truncated: 3,
    });
    expect(record).toEqual({
      kind: "labels.snapshot",
      repo: REPO,
      visibility: "public",
      taken_at: "2026-09-26T11:00:00Z",
      items: [issue(5, "loom:issue"), pr(9, [5], "loom:pr")],
      truncated: 3,
    });
  });

  it("rejects a snapshot it cannot attribute or order, and treats unknown visibility as private", () => {
    expect(parseLabelsSnapshot({ taken_at: "2026-09-26T11:00:00Z", items: [] })).toBeUndefined();
    expect(parseLabelsSnapshot({ repo: REPO, taken_at: "not a time", items: [] })).toBeUndefined();
    expect(parseLabelsSnapshot({ repo: REPO, taken_at: "2026-09-26T11:00:00Z", visibility: "PUBLIC?" })?.visibility).toBe(
      "private",
    );
  });

  it("rides on /api/fleet-state hosts as `labels`", () => {
    const snapshot = parseFleetSnapshot({
      hosts: {
        "mac-1": {
          labels: [
            { updatedAt: "2026-09-26T11:00:05Z", record: labels("2026-09-26T11:00:00Z", [issue(5, "loom:issue")]) },
            { updatedAt: "2026-09-26T11:00:05Z", record: { repo: REPO } },
          ],
        },
        "mac-2": { labels: "nope" },
      },
      activeSweeps: [],
    });
    expect(snapshot.hosts["mac-1"]?.labels).toHaveLength(1);
    expect(snapshot.hosts["mac-2"]).toEqual({});
  });
});

describe("LabelTracker", () => {
  it("treats a repo's first snapshot as the baseline, not as changes", () => {
    const tracker = new LabelTracker();
    expect(tracker.labelsFor(REPO, `${REPO}#5`)).toBeUndefined();
    expect(tracker.ingest(labels("2026-09-26T11:00:00Z", [issue(5, "loom:issue")]))).toEqual([]);
    expect(tracker.knows(REPO)).toBe(true);
    expect(tracker.labelsFor(REPO, `${REPO}#5`)).toEqual({ issue: ["loom:issue"], prs: [] });
    expect(tracker.historyFor(`${REPO}#5`)).toEqual([]);
  });

  it("diffs labels added and removed, and folds a PR onto the issue it closes", () => {
    const tracker = new LabelTracker();
    tracker.ingest(labels("2026-09-26T11:00:00Z", [issue(5, "loom:issue")]));
    const found = tracker.ingest(
      labels("2026-09-26T11:01:00Z", [issue(5, "loom:building"), pr(40, [5], "loom:review-requested")]),
    );

    expect(found.map((t) => [t.key, t.target, t.number, t.added, t.removed])).toEqual([
      [`${REPO}#5`, "issue", 5, ["loom:building"], ["loom:issue"]],
      [`${REPO}#5`, "pr", 40, ["loom:review-requested"], []],
    ]);
    expect(tracker.labelsFor(REPO, `${REPO}#5`)).toEqual({
      issue: ["loom:building"],
      prs: [{ number: 40, labels: ["loom:review-requested"] }],
    });
    expect(tracker.historyFor(`${REPO}#5`).map(describeTransition)).toEqual([
      "+building −issue",
      "PR #40 +review-requested",
    ]);
  });

  it("reports an item that leaves the open set as closed, unless the snapshot was truncated", () => {
    const tracker = new LabelTracker();
    tracker.ingest(labels("2026-09-26T11:00:00Z", [issue(5, "loom:building"), pr(40, [5], "loom:pr")]));

    expect(tracker.ingest(labels("2026-09-26T11:01:00Z", [issue(5, "loom:building")], { truncated: 1 }))).toEqual([]);

    const found = tracker.ingest(labels("2026-09-26T11:02:00Z", []));
    expect(found.map(describeTransition)).toEqual(["closed", "PR #40 closed"]);
    expect(found.every((t) => t.key === `${REPO}#5` && t.closed)).toBe(true);
  });

  it("keys a PR that closes nothing on its own", () => {
    const tracker = new LabelTracker();
    tracker.ingest(labels("2026-09-26T11:00:00Z", []));
    const [transition] = tracker.ingest(labels("2026-09-26T11:01:00Z", [pr(77, [], "loom:review-requested")]));
    expect(transition?.key).toBe(`${REPO}#pr77`);
  });

  it("ignores a snapshot no newer than the last one applied", () => {
    const tracker = new LabelTracker();
    tracker.ingest(labels("2026-09-26T11:00:00Z", [issue(5, "loom:issue")]));
    tracker.ingest(labels("2026-09-26T11:02:00Z", [issue(5, "loom:building")]));
    expect(tracker.ingest(labels("2026-09-26T11:02:00Z", [issue(5, "loom:pr")]))).toEqual([]);
    expect(tracker.ingest(labels("2026-09-26T11:01:00Z", [issue(5, "loom:pr")]))).toEqual([]);
    expect(tracker.labelsFor(REPO, `${REPO}#5`)?.issue).toEqual(["loom:building"]);
  });

  it("keeps a bounded history per item", () => {
    const tracker = new LabelTracker();
    tracker.ingest(labels("2026-09-26T11:00:00Z", [issue(5)]));
    for (let i = 1; i <= LABEL_HISTORY_MAX + 3; i += 1) {
      const at = new Date(Date.parse("2026-09-26T11:00:00Z") + i * 1000).toISOString();
      tracker.ingest(labels(at, [issue(5, i % 2 === 0 ? "loom:building" : "loom:issue")]));
    }
    expect(tracker.historyFor(`${REPO}#5`)).toHaveLength(LABEL_HISTORY_MAX);
  });

  it("names and tones chips", () => {
    expect(shortLabel("loom:changes-requested")).toBe("changes-requested");
    expect(shortLabel("bug")).toBe("bug");
    expect(labelTone("loom:pr")).toBe("approved");
    expect(labelTone("loom:triage")).toBe("neutral");
  });
});

function sweep(overrides: Partial<ActiveSweep> = {}): ActiveSweep {
  return {
    hostId: "mac-1",
    sweepId: "s1",
    repo: REPO,
    visibility: "public",
    issue: 5,
    phase: "builder",
    startedAt: "2026-09-26T11:50:00Z",
    ...overrides,
  };
}

function fleet(sweeps: ActiveSweep[], snapshots: LabelsSnapshotRecord[] = []): FleetSnapshot {
  return {
    hosts: {
      "mac-1": snapshots.length > 0 ? { labels: snapshots.map((record) => ({ record, updatedAt: record.taken_at })) } : {},
    },
    activeSweeps: sweeps,
  };
}

function labelFrame(record: LabelsSnapshotRecord): LiveTailFrame {
  return { topic: "labels.snapshot", event: { hostId: "mac-1", emittedAt: record.taken_at, schemaVersion: 1, record } };
}

function cardFor(board: LiveBoard, key: string): HTMLElement | null {
  return board.root.querySelector<HTMLElement>(`[data-testid="live-card"][data-key="${key}"]`);
}

const chipText = (node: Element | null): string[] =>
  [...(node?.querySelectorAll('[data-testid="live-label"]') ?? [])].map((chip) => chip.textContent ?? "");

describe("LiveBoard labels", () => {
  it("shows a sweep card's issue and PR labels from the fleet snapshot, and updates them in place", () => {
    const board = new LiveBoard(() => {});
    board.update(buildFleetView(fleet([sweep()], [labels("2026-09-26T11:00:00Z", [issue(5, "loom:building")])]), NOW), NOW);
    const card = cardFor(board, `${REPO}#5`)!;
    expect(chipText(card)).toEqual(["building"]);
    expect(card.classList.contains("is-relabeled")).toBe(false);

    board.pushEvent(
      labelFrame(labels("2026-09-26T11:59:00Z", [issue(5, "loom:building"), pr(40, [5], "loom:review-requested")])),
      NOW,
    );

    expect(cardFor(board, `${REPO}#5`)).toBe(card);
    expect(chipText(card)).toEqual(["building", "review-requested"]);
    expect(card.querySelector('[data-pr="40"] a')?.getAttribute("href")).toBe(`https://github.com/${REPO}/pull/40`);
    expect(card.classList.contains("is-relabeled")).toBe(true);
    const history = [...card.querySelectorAll('[data-testid="live-card-change"]')].map((row) =>
      row.querySelector(".live-card__change-text")?.textContent,
    );
    expect(history).toEqual(["PR #40 +review-requested"]);
    const ticker = board.root.querySelector('[data-testid="live-event"]');
    expect(ticker?.textContent).toContain("loom#5");
    expect(ticker?.textContent).toContain("PR #40 +review-requested");
  });

  it("gives an item whose labels change while watching a card of its own, at the end", () => {
    const board = new LiveBoard(() => {});
    board.update(buildFleetView(fleet([sweep()], [labels("2026-09-26T11:00:00Z", [issue(8, "loom:curated")])]), NOW), NOW);
    // The baseline alone does not put issue 8 on the board.
    expect(cardFor(board, `${REPO}#8`)).toBeNull();

    board.update(
      buildFleetView(fleet([sweep()], [labels("2026-09-26T11:01:00Z", [issue(8, "loom:issue")])]), NOW),
      NOW,
    );
    const watching = cardFor(board, `${REPO}#8`)!;
    expect(watching.dataset.state).toBe("watching");
    expect(watching.querySelector(".live-card__phase")?.textContent).toBe("labels changed");
    expect(
      [...board.root.querySelectorAll<HTMLElement>('[data-testid="live-card"]')].map((node) => node.dataset.key),
    ).toEqual([`${REPO}#5`, `${REPO}#8`]);

    // A sweep that then picks the issue up takes over the same card.
    board.update(buildFleetView(fleet([sweep(), sweep({ sweepId: "s2", issue: 8 })]), NOW), NOW);
    expect(cardFor(board, `${REPO}#8`)).toBe(watching);
    expect(watching.dataset.state).toBe("active");
  });

  it("puts a PR-only change on a PR card linked to the PR", () => {
    const board = new LiveBoard(() => {});
    board.update(buildFleetView(fleet([], [labels("2026-09-26T11:00:00Z", [])]), NOW), NOW);
    board.pushEvent(labelFrame(labels("2026-09-26T11:01:00Z", [pr(77, [], "loom:review-requested")])), NOW);
    const card = cardFor(board, `${REPO}#pr77`)!;
    expect(card.querySelector(".live-card__title")?.textContent).toBe("loom PR #77");
    expect(card.querySelector(".live-card__title")?.getAttribute("href")).toBe(`https://github.com/${REPO}/pull/77`);
  });

  it("renders no label row for a repo it has no snapshot of", () => {
    const board = new LiveBoard(() => {});
    board.update(buildFleetView(fleet([sweep()]), NOW), NOW);
    const card = cardFor(board, `${REPO}#5`)!;
    expect(card.querySelector<HTMLElement>('[data-testid="live-card-labels"]')!.hidden).toBe(true);
    expect(card.querySelector<HTMLElement>('[data-testid="live-card-history"]')!.hidden).toBe(true);
  });
});
