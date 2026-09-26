/**
 * The `#/live` status board's DOM (issue #9077).
 *
 * **This is the one view that is not a pure `(viewModel) => HTMLElement`.**
 * The board updates every second (timers) and every few seconds (snapshots),
 * and it animates: cards slide in, a finished sweep's card fades out, the
 * current phase pulses, a tile flashes when its number changes. Rebuilding the
 * tree on every update, as the other views do, would restart every one of
 * those animations on every poll. So the board keeps a small map of keyed
 * nodes — sweep cards by `sweepId`, host pills by `hostId` — and patches them
 * in place. That is the "durable local state across re-renders" case
 * `../../README.md` §"When to overturn this" anticipates. It is contained to
 * this file on purpose, so it does not need a framework.
 *
 * Text still goes in only through `textContent` (`el()`), never `innerHTML`.
 */

import { el } from "../dom";
import type { FleetView, HostView } from "../fleet";
import { issueUrl } from "../forgeLinks";
import { formatClock, formatPercent, secondsSince } from "../format";
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
/** How long a finished sweep's card stays on screen fading out. Matches the
 * `live-card-leave` animation in `styles.css`. */
export const CARD_LEAVE_MS = 700;
const HOST_BEAT_MS = 1200;

export type Schedule = (handler: () => void, ms: number) => void;

interface CardRefs {
  root: HTMLElement;
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

  private readonly cards = new Map<string, CardRefs>();
  private readonly hosts = new Map<string, HostRefs>();
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
    this.ticker = el("ol", { class: "live__ticker", data: { testid: "live-ticker" } });
    this.tickerEmpty = el(
      "p",
      { class: "live__ticker-empty" },
      "Events show up here as they happen: sweeps starting, changing phase, and finishing.",
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
          el("h3", { class: "live__section-title" }, "In flight ", this.sweepCount),
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
    this.updateSweeps(allSweeps(view), firstSnapshot);
    this.tick(now);
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
   * the host's pill either way. */
  pushEvent(frame: LiveTailFrame, now: Date): void {
    const host = this.hosts.get(frame.event.hostId);
    if (host) {
      retrigger(host.root, "is-beat");
      this.schedule(() => host.root.classList.remove("is-beat"), HOST_BEAT_MS);
    }

    const event = describeEvent(frame);
    if (!event) return;
    this.tickerEmpty.hidden = true;
    const row = tickerRow(event);
    this.ticker.insertBefore(row, this.ticker.firstChild);
    while (this.ticker.childElementCount > TICKER_MAX_ROWS) this.ticker.lastElementChild?.remove();
    this.tick(now);
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
    const seen = new Set<string>();
    let cursor = this.grid.firstElementChild;
    const skipLeaving = (): void => {
      while (cursor?.classList.contains("is-leaving")) cursor = cursor.nextElementSibling;
    };

    for (const sweep of sweeps) {
      seen.add(sweep.sweepId);
      let refs = this.cards.get(sweep.sweepId);
      if (!refs) {
        refs = sweepCard(sweep);
        // Cards on the first paint just appear; only a sweep that starts
        // while you are watching slides in.
        if (!firstSnapshot) refs.root.classList.add("is-entering");
        this.cards.set(sweep.sweepId, refs);
      }
      patchSweepCard(refs, sweep);
      skipLeaving();
      if (cursor === refs.root) cursor = cursor.nextElementSibling;
      else this.grid.insertBefore(refs.root, cursor);
    }

    for (const [sweepId, refs] of this.cards) {
      if (seen.has(sweepId)) continue;
      this.cards.delete(sweepId);
      refs.root.classList.remove("is-entering");
      refs.root.classList.add("is-leaving");
      this.schedule(() => refs.root.remove(), CARD_LEAVE_MS);
    }

    setText(this.sweepCount, String(sweeps.length));
    this.idle.hidden = sweeps.length > 0;
    setText(this.idleText, "All quiet. Nothing is in flight right now.");
  }
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

function sweepCard(sweep: ActiveSweep): CardRefs {
  const url = issueUrl(sweep.repo, sweep.issue);
  const title =
    url === undefined
      ? el("span", { class: "live-card__title" })
      : el("a", { class: "live-card__title", href: url, target: "_blank", rel: "noopener noreferrer" });
  const host = el("a", { class: "live-card__host", href: routeToHash({ name: "host", hostId: sweep.hostId }) });
  const steps = LIVE_PHASES.map((phase) =>
    el("span", { class: "live-step", data: { step: phase }, title: phase }, el("span", { class: "live-step__label" }, phase)),
  );
  const phaseLabel = el("span", { class: "live-card__phase" });
  const timer = el("div", { class: "live-card__timer", data: { testid: "live-card-timer" } });
  const phaseTimer = el("span", { class: "live-card__phase-timer" });
  const meta = el("span", { class: "live-card__meta" });
  const agent = el("span", { class: "live-card__agent" });

  const root = el(
    "article",
    { class: "live-card", data: { testid: "live-card", sweep: sweep.sweepId } },
    el("header", { class: "live-card__head" }, title, host),
    el("div", { class: "live-card__stepper" }, steps),
    el(
      "div",
      { class: "live-card__body" },
      timer,
      el("div", { class: "live-card__now" }, phaseLabel, phaseTimer),
    ),
    el("footer", { class: "live-card__foot" }, agent, meta),
  );
  setText(title, sweepTitle(sweep.repo, sweep.issue));
  if (sweep.repo === undefined) title.title = "Private repository";

  return { root, title, host, steps, phaseLabel, timer, phaseTimer, meta, agent, patched: false, phase: undefined, metaText: "", agentKey: "" };
}

function patchSweepCard(refs: CardRefs, sweep: ActiveSweep): void {
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
