/**
 * Label state and transitions for the Live board (issue #9094), derived by
 * diffing successive `labels.snapshot` records of each repo. DOM-free.
 *
 * ## Keys
 *
 * Everything is attributed to a **work item key** — `"owner/repo#123"` for an
 * issue, the same key the board gives a sweep on that issue, so a sweep card
 * and the labels of its issue meet on one card. A PR is folded onto every
 * issue it closes; a PR that closes nothing gets a key of its own,
 * `"owner/repo#pr456"`.
 *
 * ## What counts as a transition
 *
 * The first snapshot of a repo is the baseline: its labels are known from then
 * on, but nothing in it "changed" — the board would otherwise open with a card
 * for every labelled issue in the fleet. From the second snapshot on, per item:
 *
 *  - labels added or removed → one transition listing both;
 *  - an item that newly appears → a transition adding all its labels;
 *  - an item that disappears → a `closed` transition (it left the open set:
 *    closed, merged, or its last `loom:*` label was removed) — unless the
 *    snapshot was truncated, where absence proves nothing.
 *
 * Snapshots of one repo are ordered by `taken_at`; one no newer than the last
 * applied is ignored, so the same snapshot arriving from both the poll and the
 * live tail — or from two hosts that manage the same repo — is applied once.
 */

import type { LabelItem, LabelsSnapshotRecord } from "./labelTypes";

/** Transitions kept per work item. The card is a glance, not an audit log. */
export const LABEL_HISTORY_MAX = 8;

export interface LabelTransition {
  /** The snapshot's `taken_at` — the change happened at or before this. */
  at: string;
  target: "issue" | "pr";
  /** The issue or PR number the change was on. */
  number: number;
  added: string[];
  removed: string[];
  /** The item left the open, loom-labelled set. */
  closed: boolean;
}

/** One PR's current labels, as shown on the card of the issue it closes. */
export interface PrLabels {
  number: number;
  labels: string[];
}

/** A work item's current labels. */
export interface ItemLabels {
  /** The issue's own labels, or `undefined` when the issue is not in the
   * latest snapshot (closed, unlabelled, or a PR-only key). */
  issue: string[] | undefined;
  prs: PrLabels[];
}

/** A transition plus the work item it belongs to — the ticker's input. */
export interface KeyedTransition extends LabelTransition {
  key: string;
  repo: string;
}

interface RepoState {
  takenAtMs: number;
  items: Map<string, LabelItem>;
}

export function workItemKey(repo: string, issue: number): string {
  return `${repo}#${issue}`;
}

function prOnlyKey(repo: string, pr: number): string {
  return `${repo}#pr${pr}`;
}

function itemId(item: LabelItem): string {
  return `${item.type}:${item.number}`;
}

/** Every work item key `item` contributes labels to. */
function keysOf(repo: string, item: LabelItem): string[] {
  if (item.type === "issue") return [workItemKey(repo, item.number)];
  if (item.closes && item.closes.length > 0) return item.closes.map((issue) => workItemKey(repo, issue));
  return [prOnlyKey(repo, item.number)];
}

export class LabelTracker {
  private readonly repos = new Map<string, RepoState>();
  private readonly history = new Map<string, LabelTransition[]>();

  /**
   * Apply one snapshot. Returns the transitions it revealed, oldest-first
   * within the snapshot; empty for a baseline or a stale snapshot.
   */
  ingest(record: LabelsSnapshotRecord): KeyedTransition[] {
    const takenAtMs = Date.parse(record.taken_at);
    if (Number.isNaN(takenAtMs)) return [];
    const previous = this.repos.get(record.repo);
    if (previous && takenAtMs <= previous.takenAtMs) return [];

    const items = new Map<string, LabelItem>();
    for (const item of record.items) items.set(itemId(item), item);
    if (previous && record.truncated) {
      // Absent from a truncated snapshot means "not sent", so the last known
      // state stands — otherwise the item's later close would go unseen.
      for (const [id, item] of previous.items) if (!items.has(id)) items.set(id, item);
    }
    this.repos.set(record.repo, { takenAtMs, items });
    if (!previous) return [];

    const found: KeyedTransition[] = [];
    const note = (item: LabelItem, added: string[], removed: string[], closed: boolean): void => {
      const transition: LabelTransition = {
        at: record.taken_at,
        target: item.type,
        number: item.number,
        added,
        removed,
        closed,
      };
      for (const key of keysOf(record.repo, item)) {
        const list = this.history.get(key) ?? [];
        list.push(transition);
        if (list.length > LABEL_HISTORY_MAX) list.splice(0, list.length - LABEL_HISTORY_MAX);
        this.history.set(key, list);
        found.push({ ...transition, key, repo: record.repo });
      }
    };

    for (const [id, item] of items) {
      const before = previous.items.get(id);
      const had = new Set(before?.labels ?? []);
      const has = new Set(item.labels);
      const added = item.labels.filter((label) => !had.has(label));
      const removed = (before?.labels ?? []).filter((label) => !has.has(label));
      if (added.length > 0 || removed.length > 0) note(item, added, removed, false);
    }
    for (const [id, before] of previous.items) {
      if (!items.has(id)) note(before, [], before.labels, true);
    }
    return found;
  }

  /** Whether any snapshot of `repo` has been applied — before that, a
   * card's labels are unknown rather than empty. */
  knows(repo: string): boolean {
    return this.repos.has(repo);
  }

  /** The item's current labels from the newest snapshot of its repo, or
   * `undefined` when that repo has never been seen. */
  labelsFor(repo: string, key: string): ItemLabels | undefined {
    const state = this.repos.get(repo);
    if (!state) return undefined;
    let issue: string[] | undefined;
    const prs: PrLabels[] = [];
    for (const item of state.items.values()) {
      if (!keysOf(repo, item).includes(key)) continue;
      if (item.type === "issue") issue = item.labels;
      else prs.push({ number: item.number, labels: item.labels });
    }
    prs.sort((a, b) => a.number - b.number);
    return { issue, prs };
  }

  /** Transitions seen this session for `key`, oldest first. */
  historyFor(key: string): readonly LabelTransition[] {
    return this.history.get(key) ?? [];
  }
}

/** `"loom:review-requested"` → `"review-requested"`: the prefix is on every
 * label the board shows, so it is noise on a chip. */
export function shortLabel(label: string): string {
  return label.startsWith("loom:") ? label.slice("loom:".length) : label;
}

export type LabelTone = "active" | "review" | "changes" | "approved" | "attention" | "neutral";

const LABEL_TONES: Readonly<Record<string, LabelTone>> = {
  "loom:building": "active",
  "loom:curating": "active",
  "loom:review-requested": "review",
  "loom:changes-requested": "changes",
  "loom:pr": "approved",
  "loom:blocked": "attention",
  "loom:operator": "attention",
  "loom:operator-only": "attention",
  "loom:urgent": "attention",
};

export function labelTone(label: string): LabelTone {
  return LABEL_TONES[label] ?? "neutral";
}

/** One transition as a short phrase: `"+building −issue"`, `"PR #40 +pr"`,
 * `"PR #40 closed"`. */
export function describeTransition(transition: LabelTransition): string {
  const subject = transition.target === "pr" ? `PR #${transition.number} ` : "";
  if (transition.closed) return `${subject}closed`;
  const parts = [
    ...transition.added.map((label) => `+${shortLabel(label)}`),
    ...transition.removed.map((label) => `−${shortLabel(label)}`),
  ];
  return `${subject}${parts.join(" ")}`;
}
