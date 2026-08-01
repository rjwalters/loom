/**
 * The app controller: fetch → view model → render, plus polling and routing.
 *
 * Deliberately a plain class with explicit dependency injection (`fetchState`,
 * `now`, `setTimeout`) rather than a framework runtime, so every test drives it
 * with no timers, no network, and no framework harness — see `../README.md`
 * §"Why vanilla TypeScript".
 *
 * **Polling, not streaming.** `/api/fleet-state` is a snapshot endpoint; the
 * SSE live tail (`/api/events`) is a sibling Phase-3 issue's surface. A
 * refresh keeps the last good snapshot on screen while the next one is in
 * flight, so a transient failure mid-session shows a banner over live data
 * rather than replacing the page with an error — only the *first* load has
 * nothing to fall back to.
 */

import { fetchFleetState } from "./api";
import { replaceChildren } from "./dom";
import { buildFleetView, findHost, type FleetView } from "./fleet";
import { formatClock } from "./format";
import { OVERVIEW, isPanelRoute, parseRoute, type PanelRouteName, type Route } from "./router";
import { PANEL_STATUS, mountPanel } from "./panels";
import type { FleetSnapshot } from "./types";
import { fleetOverviewView } from "./views/fleetOverview";
import { hostDetailView } from "./views/hostDetail";
import { errorView, loadingView, unknownHostView } from "./views/states";

export const DEFAULT_POLL_INTERVAL_MS = 10_000;

export interface AppOptions {
  root: HTMLElement;
  /** Optional status line + refresh button in the page chrome. */
  statusEl?: HTMLElement | null;
  refreshButton?: HTMLElement | null;
  fetchState?: () => Promise<FleetSnapshot>;
  now?: () => Date;
  pollIntervalMs?: number;
  /** Injected so tests can advance polling deterministically. */
  scheduler?: {
    setTimeout: (handler: () => void, ms: number) => number;
    clearTimeout: (handle: number) => void;
  };
}

export class App {
  private readonly root: HTMLElement;
  private readonly statusEl: HTMLElement | null;
  private readonly refreshButton: HTMLElement | null;
  private readonly fetchState: () => Promise<FleetSnapshot>;
  private readonly now: () => Date;
  private readonly pollIntervalMs: number;
  private readonly scheduler: NonNullable<AppOptions["scheduler"]>;

  private route: Route = OVERVIEW;
  private snapshot: FleetSnapshot | null = null;
  private error: Error | null = null;
  private loading = true;
  private inFlight = false;
  private timer: number | null = null;
  private stopped = false;

  /** Which panel route is currently mounted, and how to tear it down.
   *
   * Panel routes (#4895) own their own fetching, so they must be mounted on a
   * route *change* and not on every `render()` — `render()` runs on each poll
   * tick, and remounting there would refetch and flicker the panel every
   * `pollIntervalMs`. */
  private mountedPanel: PanelRouteName | null = null;
  private teardownPanel: (() => void) | null = null;

  constructor(options: AppOptions) {
    this.root = options.root;
    this.statusEl = options.statusEl ?? null;
    this.refreshButton = options.refreshButton ?? null;
    this.fetchState = options.fetchState ?? (() => fetchFleetState());
    this.now = options.now ?? (() => new Date());
    this.pollIntervalMs = options.pollIntervalMs ?? DEFAULT_POLL_INTERVAL_MS;
    this.scheduler = options.scheduler ?? {
      setTimeout: (handler, ms) => globalThis.setTimeout(handler, ms) as unknown as number,
      clearTimeout: (handle) => globalThis.clearTimeout(handle),
    };
    this.refreshButton?.addEventListener("click", () => void this.refresh());
  }

  /** Current route — set by `start()` from the URL, then by `navigate()`. */
  get currentRoute(): Route {
    return this.route;
  }

  /** Render immediately (loading state), then fetch. */
  async start(initialHash = ""): Promise<void> {
    this.route = parseRoute(initialHash);
    this.render();
    await this.refresh();
  }

  /** Route changed — re-render from the snapshot already in hand. No fetch:
   * the drill-down is a projection of the same `/api/fleet-state` payload, so
   * navigating between hosts costs zero requests. */
  navigate(route: Route): void {
    this.route = route;
    this.render();
  }

  async refresh(): Promise<void> {
    if (this.inFlight || this.stopped) return;
    this.inFlight = true;
    this.setStatus("Refreshing…");
    try {
      this.snapshot = await this.fetchState();
      this.error = null;
    } catch (caught) {
      if (caught instanceof DOMException && caught.name === "AbortError") {
        this.inFlight = false;
        return;
      }
      this.error = caught instanceof Error ? caught : new Error(String(caught));
    } finally {
      this.loading = false;
      this.inFlight = false;
    }
    this.render();
    this.scheduleNext();
  }

  /** (Re-)arm polling after `stop()`.
   *
   * Note this is **not** what turns polling on for the first time: `refresh()`
   * always calls `scheduleNext()` and `stopped` is `false` from construction,
   * so `start()` alone already arms the timer (a `pollIntervalMs <= 0` app
   * never polls either way). What this method is for is clearing the `stopped`
   * flag that `stop()` set — the tab-visibility handler in `main.ts` calls
   * `stop()` when the tab is hidden and `startPolling()` when it returns, and
   * without clearing the flag the subsequent `refresh()` would return early
   * and polling would never resume. */
  startPolling(): void {
    this.stopped = false;
    this.scheduleNext();
  }

  stop(): void {
    this.unmountPanel();
    this.stopped = true;
    if (this.timer !== null) {
      this.scheduler.clearTimeout(this.timer);
      this.timer = null;
    }
  }

  private scheduleNext(): void {
    if (this.stopped || this.pollIntervalMs <= 0) return;
    if (this.timer !== null) this.scheduler.clearTimeout(this.timer);
    this.timer = this.scheduler.setTimeout(() => {
      this.timer = null;
      void this.refresh();
    }, this.pollIntervalMs);
  }

  private setStatus(text: string): void {
    if (this.statusEl) this.statusEl.textContent = text;
  }

  private view(): FleetView | null {
    return this.snapshot ? buildFleetView(this.snapshot, this.now()) : null;
  }

  /** Mount `name`'s panel if it is not already the mounted one. Idempotent by
   * design — `render()` calls this on every poll tick. */
  private renderPanel(name: PanelRouteName): void {
    if (this.mountedPanel === name) return;
    this.unmountPanel();

    this.root.setAttribute("aria-busy", "false");
    this.teardownPanel = mountPanel(name, this.root);
    this.mountedPanel = name;
  }

  private unmountPanel(): void {
    this.teardownPanel?.();
    this.teardownPanel = null;
    this.mountedPanel = null;
  }

  render(): void {
    const now = this.now();

    // Panel routes short-circuit every snapshot-dependent branch below: they
    // do not read the fleet snapshot at all, so a first paint before the first
    // poll returns must not show the fleet's loading view.
    if (isPanelRoute(this.route)) {
      this.renderPanel(this.route.name);
      this.setStatus(PANEL_STATUS[this.route.name]);
      return;
    }
    this.unmountPanel();

    if (this.loading && !this.snapshot) {
      this.root.setAttribute("aria-busy", "true");
      replaceChildren(this.root, loadingView());
      return;
    }
    this.root.setAttribute("aria-busy", this.inFlight ? "true" : "false");

    // A failure with no prior snapshot is a full-page error; a failure with one
    // is a banner above the (stale but real) data.
    if (this.error && !this.snapshot) {
      replaceChildren(this.root, errorView(this.error, () => void this.refresh()));
      this.setStatus("Failed");
      return;
    }

    const view = this.view();
    if (!view) {
      replaceChildren(this.root, loadingView());
      return;
    }

    const banner = this.error ? errorView(this.error, () => void this.refresh()) : null;
    if (banner) banner.classList.add("state--banner");

    if (this.route.name === "host") {
      const host = findHost(view, this.route.hostId);
      replaceChildren(this.root, banner, host ? hostDetailView(host, now) : unknownHostView(this.route.hostId));
    } else {
      replaceChildren(this.root, banner, fleetOverviewView(view, now));
    }

    this.setStatus(this.error ? "Stale — last refresh failed" : `Updated ${formatClock(now)}`);
  }
}
