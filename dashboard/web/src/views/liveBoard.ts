/**
 * The `#/live` status board's DOM (issue #9077).
 *
 * **This is the one view that is not a pure `(viewModel) => HTMLElement`.**
 * The board updates every second (timers) and every few seconds (snapshots),
 * and it animates: cards slide in, the current phase pulses, a card flashes
 * when its phase or labels change, a tile flashes when its number changes.
 * Rebuilding the tree on every update, as the other views do, would restart
 * every one of those animations on every poll. So the board keeps a small map
 * of keyed nodes — cards by work item, host pills by `hostId` — and patches
 * them in place. That is the "durable local state across re-renders" case
 * `../../README.md` §"When to overturn this" anticipates. It is contained to
 * this file on purpose, so it does not need a framework.
 *
 * **Cards never move (issue #9094).** A card is keyed by its work item
 * (`owner/repo#123`, so a re-dispatched sweep on the same issue lands on the
 * same card), placed once — the first paint in longest-running order, every
 * later card after the last — and from then on only patched. A sweep that
 * finishes leaves its card where it is, marked done; a card is removed only
 * by "Clear finished", or when the board is over [`MAX_CARDS`] and it is the
 * oldest card with nothing in flight. Watching one card means its position is
 * something you can rely on.
 *
 * Label state comes from `labels.snapshot` records (`../labelTracker.ts`),
 * off both the snapshot poll and the live tail. An issue whose labels change
 * while you watch gets a card even with no sweep on it.
 *
 * Text still goes in only through `textContent` (`el()`), never `innerHTML`.
 */

import { el } from "../dom";
import type { FleetView, HostView } from "../fleet";
import { issueUrl, pullUrl } from "../forgeLinks";
import { formatClock, formatPercent, secondsSince } from "../format";
import {
  LabelTracker,
  describeTransition,
  labelTone,
  shortLabel,
  workItemKey,
  type ItemLabels,
  type KeyedTransition,
  type LabelTransition,
} from "../labelTracker";
import { parseLabelsSnapshot } from "../labelParse";
import type { LabelsSnapshotRecord } from "../labelTypes";
import {
  LIVE_PHASES,
  allSweeps,
  boardTiles,
  describeEvent,
  formatStopwatch,
  isOnline,
  loadFraction,
  phaseIndex,
  sweepTitle,
  type BoardEvent,
} from "../liveBoard";
import { sweepAgentMark } from "../providers";
import { routeToHash } from "../router";
import type { ConnectionState } from "../sseFeedClient";
import type { ActiveSweep, LiveTailFrame } from "../types";

/** How many ticker lines to keep. The board is a glance, not a log. */
export const TICKER_MAX_ROWS = 40;
/** Cards kept on the board. Past this, the oldest card with nothing in flight
 * is dropped — an active sweep's card never is. */
export const MAX_CARDS = 48;
const HOST_BEAT_MS = 1200;

export type Schedule = (handler: () => void, ms: number) => void;

type CardState = "active" | "done" | "watching";

interface CardRefs {
  key: string;
  repo: string | undefined;
  /** The issue, for an issue key; `undefined` for a PR-only or sweep key. */
  issue: number | undefined;
  root: HTMLElement;
  state: CardState;
  labels: HTMLElement;
  history: HTMLOListElement;
  labelsKey: string;
  /** The newest transition rendered — identity changes with every new one,
   * even once the history is full and its length stops changing. */
  historyTop: LabelTransition | undefined;
  title: HTMLElement;
  host: HTMLAnchorElement;
  steps: HTMLElement[];
  phaseLabel: HTMLElement;
  timer: HTMLElement;
  phaseTimer: HTMLElement;
  meta: HTMLElement;
  agent: HTMLElement;
  /** Last-rendered values, so a poll that changed nothing touches nothing. */
  patched: boolean;
  phase: string | undefined;
  metaText: string;
  agentKey: string;
}

interface HostRefs {
  root: HTMLAnchorElement;
  loadFill: HTMLElement;
  load: HTMLElement;
  count: HTMLElement;
}

interface TileRefs {
  root: HTMLElement;
  value: HTMLElement;
  hint: HTMLElement;
}

const TILE_KEYS = ["hosts", "running", "ready", "blocked", "tokens", "attention"] as const;
type TileKey = (typeof TILE_KEYS)[number];

const TILE_LABELS: Readonly<Record<TileKey, string>> = {
  hosts: "Hosts online",
  running: "Sweeps running",
  ready: "Ready to build",
  blocked: "Blocked",
  tokens: "Peak token use",
  attention: "Need attention",
};

/** Restart a one-shot CSS animation class on `node`. */
function retrigger(node: HTMLElement, className: string): void {
  node.classList.remove(className);
  // Reading layout forces a style flush, so re-adding the class starts the
  // animation again instead of being coalesced with the removal.
  void node.offsetWidth;
  node.classList.add(className);
}

function setText(node: HTMLElement, text: string): boolean {
  if (node.textContent === text) return false;
  node.textContent = text;
  return true;
}

export class LiveBoard {
  readonly root: HTMLElement;
  private readonly schedule: Schedule;

  private readonly beacon: HTMLElement;
  private readonly beaconText: HTMLElement;
  private readonly clock: HTMLElement;
  private readonly freshness: HTMLElement;
  private readonly banner: HTMLElement;
  private readonly tiles = new Map<TileKey, TileRefs>();
  private readonly hostStrip: HTMLElement;
  private readonly grid: HTMLElement;
  private readonly idle: HTMLElement;
  private readonly idleText: HTMLElement;
  private readonly sweepCount: HTMLElement;
  private readonly ticker: HTMLOListElement;
  private readonly tickerEmpty: HTMLElement;

  private readonly clearButton: HTMLButtonElement;

  /** Insertion-ordered, which is also DOM order: a card is appended once and
   * never moved. */
  private readonly cards = new Map<string, CardRefs>();
  private readonly hosts = new Map<string, HostRefs>();
  private readonly labels = new LabelTracker();
  /** Latest `sweep.completed` result per work item key. */
  private readonly results = new Map<string, string>();
  private hasSnapshot = false;

  constructor(schedule: Schedule = (handler, ms) => void globalThis.setTimeout(handler, ms)) {
    this.schedule = schedule;

    this.beaconText = el("span", { class: "live__beacon-text" }, "Connecting");
    this.beacon = el(
      "div",
      { class: "live__beacon", data: { testid: "live-beacon", connection: "connecting" } },
      el("span", { class: "live__beacon-dot", aria: { hidden: "true" } }),
      this.beaconText,
    );
    this.clock = el("div", { class: "live__clock", data: { testid: "live-clock" } }, "--:--:--");
    this.freshness = el(
      "div",
      { class: "live__freshness", data: { testid: "live-freshness", format: "updated" } },
      "Waiting for the first snapshot…",
    );
    this.banner = el("div", { class: "live__banner", role: "alert", data: { testid: "live-error" } });
    this.banner.hidden = true;

    const tileRow = el("div", { class: "live__tiles" });
    for (const key of TILE_KEYS) {
      const value = el("div", { class: "live-tile__value" }, "—");
      const hint = el("div", { class: "live-tile__hint" });
      const root = el(
        "div",
        { class: "live-tile", data: { testid: `live-tile-${key}`, tile: key } },
        el("div", { class: "live-tile__label" }, TILE_LABELS[key]),
        value,
        hint,
      );
      this.tiles.set(key, { root, value, hint });
      tileRow.appendChild(root);
    }

    this.hostStrip = el("div", { class: "live__hosts", data: { testid: "live-hosts" } });
    this.grid = el("div", { class: "live__grid", data: { testid: "live-sweeps" } });
    this.idleText = el("span", { class: "live__idle-text" }, "Waiting for the first snapshot…");
    this.idle = el(
      "div",
      { class: "live__idle", data: { testid: "live-idle" } },
      el("span", { class: "live__idle-ring", aria: { hidden: "true" } }),
      this.idleText,
    );
    this.sweepCount = el("span", { class: "live__section-count" });
    this.clearButton = el(
      "button",
      { class: "live__clear", type: "button", data: { testid: "live-clear-finished" } },
      "Clear finished",
    );
    this.clearButton.hidden = true;
    this.clearButton.addEventListener("click", () => this.clearFinished());
    this.ticker = el("ol", { class: "live__ticker", data: { testid: "live-ticker" } });
    this.tickerEmpty = el(
      "p",
      { class: "live__ticker-empty" },
      "Events show up here as they happen: sweeps starting, changing phase and finishing, and labels changing.",
    );

    this.root = el(
      "section",
      { class: "live", data: { testid: "live-board" } },
      el(
        "header",
        { class: "live__header" },
        el("div", { class: "live__heading" }, this.beacon, el("h2", { class: "live__title" }, "Fleet, live")),
        el("div", { class: "live__time" }, this.clock, this.freshness),
      ),
      this.banner,
      tileRow,
      this.hostStrip,
      el(
        "div",
        { class: "live__main" },
        el(
          "section",
          { class: "live__panel live__panel--sweeps" },
          el(
            "div",
            { class: "live__section-head" },
            el("h3", { class: "live__section-title" }, "In flight ", this.sweepCount),
            this.clearButton,
          ),
          this.grid,
          this.idle,
        ),
        el(
          "aside",
          { class: "live__panel live__panel--ticker" },
          el("h3", { class: "live__section-title" }, "Activity"),
          this.tickerEmpty,
          this.ticker,
        ),
      ),
    );
  }

  /** Apply a new snapshot. Nodes for sweeps and hosts that are still present
   * are patched, not replaced. */
  update(view: FleetView, now: Date): void {
    const firstSnapshot = !this.hasSnapshot;
    this.hasSnapshot = true;
    this.freshness.dataset.since = now.toISOString();

    this.updateTiles(view, now, firstSnapshot);
    this.updateHosts(view);
    // Oldest first, so two hosts' snapshots of one repo diff in order.
    const snapshots = view.hosts
      .flatMap((host) => (host.entry.labels ?? []).map((entry) => ({ hostId: host.hostId, record: entry.record })))
      .sort((a, b) => Date.parse(a.record.taken_at) - Date.parse(b.record.taken_at));
    for (const { hostId, record } of snapshots) this.applyLabels(record, hostId, firstSnapshot);
    this.updateSweeps(allSweeps(view), firstSnapshot);
    this.tick(now);
  }

  /** Remove every card with nothing in flight. The only way a card leaves the
   * board short of [`MAX_CARDS`]. */
  clearFinished(): void {
    for (const [key, refs] of this.cards) {
      if (refs.state === "active") continue;
      refs.root.remove();
      this.cards.delete(key);
    }
    this.syncChrome();
  }

  /** Advance every clock-driven text on the board. Only text changes, so
   * the one-second tick never restarts an animation. */
  tick(now: Date): void {
    setText(this.clock, formatClock(now));
    for (const node of this.root.querySelectorAll<HTMLElement>("[data-since]")) {
      const seconds = secondsSince(node.dataset.since, now);
      switch (node.dataset.format) {
        case "ago":
          setText(node, seconds === undefined || seconds < 5 ? "now" : `${formatStopwatch(seconds)} ago`);
          break;
        case "updated":
          setText(node, seconds === undefined || seconds < 2 ? "Updated just now" : `Updated ${Math.floor(seconds)}s ago`);
          break;
        default:
          setText(node, formatStopwatch(seconds));
      }
    }
  }

  setConnection(state: ConnectionState): void {
    this.beacon.dataset.connection = state;
    setText(this.beaconText, state === "open" ? "Live" : state === "connecting" ? "Connecting" : "Reconnecting");
  }

  /** Show `message` above the board, or clear it with `null`. The board keeps
   * its last good data under the banner. */
  setError(message: string | null): void {
    this.banner.hidden = message === null;
    this.banner.textContent = message === null ? "" : `Showing the last good snapshot. ${message}`;
  }

  /** One live-tail frame: a ticker line if it is news, and a heartbeat on
   * the host's pill either way. A `labels.snapshot` updates card labels and
   * puts each transition it reveals on the ticker; a `sweep.completed`
   * records how the item's sweep ended, for its card once it goes done. */
  pushEvent(frame: LiveTailFrame, now: Date): void {
    const host = this.hosts.get(frame.event.hostId);
    if (host) {
      retrigger(host.root, "is-beat");
      this.schedule(() => host.root.classList.remove("is-beat"), HOST_BEAT_MS);
    }

    const record = frame.event.record as Record<string, unknown>;
    if (record.kind === "labels.snapshot") {
      const snapshot = parseLabelsSnapshot(record);
      if (snapshot) this.applyLabels(snapshot, frame.event.hostId, !this.hasSnapshot);
    }
    if (record.kind === "sweep.completed" && typeof record.result === "string") {
      const key = itemKey(stringField(record.repo), numberField(record.issue), stringField(record.sweep_id));
      if (key !== undefined) {
        this.results.set(key, record.result);
        const refs = this.cards.get(key);
        if (refs) patchStatus(refs, this.results.get(key));
      }
    }

    const event = describeEvent(frame);
    if (event) this.addTickerRow(event);
    this.tick(now);
  }

  private addTickerRow(event: BoardEvent): void {
    this.tickerEmpty.hidden = true;
    this.ticker.insertBefore(tickerRow(event), this.ticker.firstChild);
    while (this.ticker.childElementCount > TICKER_MAX_ROWS) this.ticker.lastElementChild?.remove();
  }

  private applyLabels(record: LabelsSnapshotRecord, hostId: string, firstSnapshot: boolean): void {
    const transitions = this.labels.ingest(record);
    for (const transition of transitions) {
      let refs = this.cards.get(transition.key);
      if (!refs) {
        refs = this.addCard(labelCard(transition.key, record.repo), firstSnapshot);
        patchStatus(refs, undefined);
      }
      this.addTickerRow(labelEvent(transition, hostId));
    }
    // Every card of this repo, not only the changed ones: the first snapshot
    // of a repo is when the cards already on screen learn their labels.
    for (const refs of this.cards.values()) {
      if (refs.repo === record.repo) this.patchLabels(refs);
    }
    this.syncChrome();
  }

  private addCard(refs: CardRefs, firstSnapshot: boolean): CardRefs {
    // Cards on the first paint just appear; only one that arrives while you
    // are watching slides in.
    if (!firstSnapshot) refs.root.classList.add("is-entering");
    this.cards.set(refs.key, refs);
    this.grid.appendChild(refs.root);
    this.evictOverflow();
    return refs;
  }

  private evictOverflow(): void {
    for (const [key, refs] of this.cards) {
      if (this.cards.size <= MAX_CARDS) return;
      if (refs.state === "active") continue;
      refs.root.remove();
      this.cards.delete(key);
    }
  }

  private patchLabels(refs: CardRefs): void {
    const current = refs.repo === undefined ? undefined : this.labels.labelsFor(refs.repo, refs.key);
    const labelsKey = current === undefined ? "" : JSON.stringify(current);
    if (labelsKey !== refs.labelsKey) {
      const changed = refs.labelsKey !== "";
      refs.labels.replaceChildren(...labelChips(refs.repo, current));
      refs.labels.hidden = refs.labels.childElementCount === 0;
      refs.labelsKey = labelsKey;
      if (changed) retrigger(refs.root, "is-relabeled");
    }
    const history = this.labels.historyFor(refs.key);
    const top = history[history.length - 1];
    if (top !== refs.historyTop) {
      refs.history.replaceChildren(
        ...[...history].reverse().map((transition) =>
          el(
            "li",
            { class: "live-card__change", data: { testid: "live-card-change" } },
            el("span", { class: "live-card__change-text" }, describeTransition(transition)),
            el("span", { class: "live-card__change-time", data: { since: transition.at, format: "ago" } }),
          ),
        ),
      );
      refs.history.hidden = history.length === 0;
      refs.historyTop = top;
    }
  }

  /** The header count and the empty-state text, from the card map. */
  private syncChrome(): void {
    let active = 0;
    let finished = 0;
    for (const refs of this.cards.values()) {
      if (refs.state === "active") active += 1;
      else finished += 1;
    }
    setText(this.sweepCount, String(active));
    this.clearButton.hidden = finished === 0;
    this.idle.hidden = this.cards.size > 0;
    if (this.hasSnapshot) setText(this.idleText, "All quiet. Nothing is in flight right now.");
  }

  private updateTiles(view: FleetView, now: Date, firstSnapshot: boolean): void {
    const tiles = boardTiles(view, now);
    const set = (key: TileKey, value: string, hint: string, tone: string): void => {
      const refs = this.tiles.get(key)!;
      refs.root.dataset.tone = tone;
      setText(refs.hint, hint);
      if (setText(refs.value, value) && !firstSnapshot) retrigger(refs.root, "is-changed");
    };

    set(
      "hosts",
      String(tiles.hostsOnline),
      `of ${tiles.hostsTotal}`,
      tiles.hostsTotal === 0 ? "dim" : tiles.hostsOnline === tiles.hostsTotal ? "ok" : "warn",
    );
    set("running", String(tiles.running), tiles.running === 1 ? "sweep" : "sweeps", tiles.running > 0 ? "accent" : "dim");
    set(
      "ready",
      tiles.ready === undefined ? "—" : String(tiles.ready),
      tiles.ready === undefined ? "no queue data" : "in queue",
      "neutral",
    );
    set(
      "blocked",
      tiles.blocked === undefined ? "—" : String(tiles.blocked),
      tiles.blocked === undefined ? "no queue data" : "parked",
      tiles.blocked ? "warn" : "neutral",
    );
    set(
      "tokens",
      formatPercent(tiles.peakUsage),
      "5h window",
      tiles.peakUsage === undefined ? "dim" : tiles.peakUsage >= 0.9 ? "bad" : tiles.peakUsage >= 0.7 ? "warn" : "ok",
    );
    set(
      "attention",
      String(tiles.attention),
      tiles.attention === 0 ? "all clear" : tiles.attention === 1 ? "host" : "hosts",
      tiles.attention > 0 ? "bad" : "ok",
    );
  }

  private updateHosts(view: FleetView): void {
    const seen = new Set<string>();
    let cursor = this.hostStrip.firstElementChild;
    for (const host of view.hosts) {
      seen.add(host.hostId);
      let refs = this.hosts.get(host.hostId);
      if (!refs) {
        refs = hostPill(host);
        this.hosts.set(host.hostId, refs);
      }
      patchHostPill(refs, host);
      if (cursor === refs.root) cursor = cursor.nextElementSibling;
      else this.hostStrip.insertBefore(refs.root, cursor);
    }
    for (const [hostId, refs] of this.hosts) {
      if (seen.has(hostId)) continue;
      refs.root.remove();
      this.hosts.delete(hostId);
    }
  }

  private updateSweeps(sweeps: ActiveSweep[], firstSnapshot: boolean): void {
    const live = new Set<string>();
    for (const sweep of sweeps) {
      const key = sweepKey(sweep);
      // Two hosts on one issue share a card; the longest-running one shows.
      if (live.has(key)) continue;
      live.add(key);
      let refs = this.cards.get(key);
      if (!refs) {
        refs = this.addCard(sweepCard(key, sweep), firstSnapshot);
        this.patchLabels(refs);
      }
      if (refs.state !== "active") {
        // A new sweep on an item already on the board: same card, same place.
        retrigger(refs.root, "is-advanced");
        refs.state = "active";
        refs.phase = undefined;
        refs.patched = false;
        this.results.delete(key);
      }
      patchSweepCard(refs, sweep);
    }

    for (const [key, refs] of this.cards) {
      if (refs.state !== "active" || live.has(key)) continue;
      refs.state = "done";
      patchStatus(refs, this.results.get(key));
    }
    this.syncChrome();
  }
}

function stringField(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function numberField(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

/** A card's key: the work item when the sweep names one, else the sweep. */
function itemKey(repo: string | undefined, issue: number | undefined, sweepId: string | undefined): string | undefined {
  if (repo !== undefined && issue !== undefined) return workItemKey(repo, issue);
  return sweepId === undefined ? undefined : `sweep:${sweepId}`;
}

function sweepKey(sweep: ActiveSweep): string {
  return itemKey(sweep.repo, sweep.issue, sweep.sweepId)!;
}

function hostPill(host: HostView): HostRefs {
  const loadFill = el("span", { class: "live-host__load-fill" });
  const load = el("span", { class: "live-host__load", title: "Load per core" }, loadFill);
  const count = el("span", { class: "live-host__count" });
  const root = el(
    "a",
    {
      class: "live-host",
      href: routeToHash({ name: "host", hostId: host.hostId }),
      data: { testid: "live-host", host: host.hostId },
    },
    el("span", { class: "live-host__dot", aria: { hidden: "true" } }),
    el("span", { class: "live-host__name" }, host.hostId),
    load,
    count,
  );
  return { root, loadFill, load, count };
}

function patchHostPill(refs: HostRefs, host: HostView): void {
  refs.root.dataset.status = host.status;
  refs.root.dataset.busy = host.sweeps.length > 0 ? "true" : "false";
  refs.root.title = host.degradedReason ? `${host.status}: ${host.degradedReason}` : host.status;
  const load = isOnline(host) ? loadFraction(host) : undefined;
  refs.load.hidden = load === undefined;
  if (load !== undefined) {
    refs.loadFill.style.width = `${Math.round(load * 100)}%`;
    refs.load.dataset.level = load >= 0.9 ? "high" : load >= 0.6 ? "mid" : "low";
  }
  setText(refs.count, host.sweeps.length > 0 ? String(host.sweeps.length) : "");
}

function sweepCard(key: string, sweep: ActiveSweep): CardRefs {
  const refs = card(key, sweep.repo, sweep.issue, undefined, sweep.sweepId);
  refs.state = "active";
  return refs;
}

/** A card for an item whose labels changed while nobody was sweeping it.
 * `key` is `owner/repo#123` or `owner/repo#pr456`. */
function labelCard(key: string, repo: string): CardRefs {
  const match = /#(pr)?(\d+)$/.exec(key);
  const number = match ? Number(match[2]) : undefined;
  return match?.[1] ? card(key, repo, undefined, number, undefined) : card(key, repo, number, undefined, undefined);
}

function card(
  key: string,
  repo: string | undefined,
  issue: number | undefined,
  pr: number | undefined,
  sweepId: string | undefined,
): CardRefs {
  const url = pr === undefined ? issueUrl(repo, issue) : pullUrl(repo, pr);
  const title =
    url === undefined
      ? el("span", { class: "live-card__title" })
      : el("a", { class: "live-card__title", href: url, target: "_blank", rel: "noopener noreferrer" });
  const host = el("a", { class: "live-card__host" });
  const steps = LIVE_PHASES.map((phase) =>
    el("span", { class: "live-step", data: { step: phase }, title: phase }, el("span", { class: "live-step__label" }, phase)),
  );
  const phaseLabel = el("span", { class: "live-card__phase" });
  const timer = el("div", { class: "live-card__timer", data: { testid: "live-card-timer" } });
  const phaseTimer = el("span", { class: "live-card__phase-timer" });
  const meta = el("span", { class: "live-card__meta" });
  const agent = el("span", { class: "live-card__agent" });
  const labels = el("div", { class: "live-card__labels", data: { testid: "live-card-labels" } });
  labels.hidden = true;
  const history = el("ol", { class: "live-card__history", data: { testid: "live-card-history" } });
  history.hidden = true;

  const root = el(
    "article",
    { class: "live-card", data: { testid: "live-card", key, ...(sweepId === undefined ? {} : { sweep: sweepId }) } },
    el("header", { class: "live-card__head" }, title, host),
    el("div", { class: "live-card__stepper" }, steps),
    el(
      "div",
      { class: "live-card__body" },
      timer,
      el("div", { class: "live-card__now" }, phaseLabel, phaseTimer),
    ),
    labels,
    history,
    el("footer", { class: "live-card__foot" }, agent, meta),
  );
  setText(title, pr === undefined ? sweepTitle(repo, issue) : `${sweepTitle(repo, undefined)} PR #${pr}`);
  if (repo === undefined) title.title = "Private repository";

  return {
    key,
    repo,
    issue,
    root,
    state: "watching",
    labels,
    history,
    labelsKey: "",
    historyTop: undefined,
    title,
    host,
    steps,
    phaseLabel,
    timer,
    phaseTimer,
    meta,
    agent,
    patched: false,
    phase: undefined,
    metaText: "",
    agentKey: "",
  };
}

/** The card's state for a sweep that is not running: `done` stops its
 * clocks where they stood and says how it ended; `watching` (labels only)
 * has no clocks at all. */
function patchStatus(refs: CardRefs, result: string | undefined): void {
  refs.root.dataset.state = refs.state;
  if (refs.state === "active") return;
  delete refs.timer.dataset.since;
  delete refs.phaseTimer.dataset.since;
  setText(refs.phaseTimer, "");
  if (refs.state === "watching") {
    setText(refs.timer, "");
    setText(refs.phaseLabel, "labels changed");
    return;
  }
  refs.root.dataset.result = result ?? "unknown";
  setText(refs.phaseLabel, result ? `finished · ${result}` : "finished");
}

function labelChip(label: string): HTMLElement {
  return el(
    "span",
    { class: "live-label", title: label, data: { tone: labelTone(label), testid: "live-label" } },
    shortLabel(label),
  );
}

/** Issue chips, then one group per PR: `PR #40 [review-requested]`. */
function labelChips(repo: string | undefined, current: ItemLabels | undefined): HTMLElement[] {
  if (!current) return [];
  const nodes: HTMLElement[] = [];
  if (current.issue && current.issue.length > 0) {
    nodes.push(el("span", { class: "live-card__label-group" }, ...current.issue.map(labelChip)));
  }
  for (const pr of current.prs) {
    const url = pullUrl(repo, pr.number);
    const name =
      url === undefined
        ? el("span", { class: "live-card__pr" }, `PR #${pr.number}`)
        : el("a", { class: "live-card__pr", href: url, target: "_blank", rel: "noopener noreferrer" }, `PR #${pr.number}`);
    nodes.push(el("span", { class: "live-card__label-group", data: { pr: String(pr.number) } }, name, ...pr.labels.map(labelChip)));
  }
  return nodes;
}

/** A label transition as a ticker line. */
function labelEvent(transition: KeyedTransition, hostId: string): BoardEvent {
  const issue = /#(\d+)$/.exec(transition.key);
  const number = issue ? Number(issue[1]) : undefined;
  return {
    tone: transition.closed ? "info" : "phase",
    subject: number === undefined ? `${sweepTitle(transition.repo, undefined)} PR #${transition.number}` : sweepTitle(transition.repo, number),
    text: describeTransition(transition),
    hostId,
    at: transition.at,
    repo: transition.repo,
    issue: number,
  };
}

function patchSweepCard(refs: CardRefs, sweep: ActiveSweep): void {
  refs.root.dataset.state = "active";
  delete refs.root.dataset.result;
  refs.root.dataset.sweep = sweep.sweepId;
  refs.host.href = routeToHash({ name: "host", hostId: sweep.hostId });
  setText(refs.host, sweep.hostId);

  if (sweep.startedAt) refs.timer.dataset.since = sweep.startedAt;
  else delete refs.timer.dataset.since;
  if (!sweep.startedAt) setText(refs.timer, "—");
  if (sweep.enteredPhaseAt) refs.phaseTimer.dataset.since = sweep.enteredPhaseAt;
  else {
    delete refs.phaseTimer.dataset.since;
    setText(refs.phaseTimer, "");
  }

  if (!refs.patched || refs.phase !== sweep.phase) {
    const current = phaseIndex(sweep.phase);
    refs.steps.forEach((step, index) => {
      step.dataset.state = current < 0 ? "todo" : index < current ? "done" : index === current ? "current" : "todo";
    });
    refs.root.dataset.phase = current < 0 ? "unknown" : LIVE_PHASES[current];
    setText(refs.phaseLabel, sweep.phase ? `in ${sweep.phase}` : "starting");
    // A phase change on a card already on screen is the moment worth a flash.
    if (refs.patched) retrigger(refs.root, "is-advanced");
    refs.phase = sweep.phase;
  }

  const metaText = [sweep.model, sweep.effort].filter(Boolean).join(" · ");
  if (metaText !== refs.metaText) {
    setText(refs.meta, metaText);
    refs.metaText = metaText;
  }
  const agentKey = `${sweep.provider ?? ""}|${sweep.runtime ?? ""}|${sweep.model ?? ""}`;
  if (agentKey !== refs.agentKey) {
    refs.agent.replaceChildren(...[sweepAgentMark(sweep)].filter((node): node is HTMLElement => node !== null));
    refs.agentKey = agentKey;
  }
  refs.patched = true;
}

function tickerRow(event: BoardEvent): HTMLLIElement {
  const url = issueUrl(event.repo, event.issue);
  const subject =
    event.subject === undefined
      ? null
      : url === undefined
        ? el("strong", { class: "live-event__subject" }, event.subject)
        : el(
            "a",
            { class: "live-event__subject", href: url, target: "_blank", rel: "noopener noreferrer" },
            event.subject,
          );
  return el(
    "li",
    { class: "live-event is-new", data: { testid: "live-event", tone: event.tone } },
    el("span", { class: "live-event__dot", aria: { hidden: "true" } }),
    el(
      "span",
      { class: "live-event__line" },
      subject,
      subject ? " " : null,
      el("span", { class: "live-event__text" }, event.text),
    ),
    el(
      "span",
      { class: "live-event__where" },
      el("span", { class: "live-event__host" }, event.hostId),
      el("span", { class: "live-event__time", data: { since: event.at, format: "ago" } }),
    ),
  );
}
