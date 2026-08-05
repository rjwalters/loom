/**
 * The app shell's mount points for the self-contained Phase-3 panels
 * (issue #4895).
 *
 * ## Why this file exists
 *
 * `#4750` (live event feed + sweep timeline), `#4751` (historical charts) and
 * `#4752` (token/cost analytics) were each built as standalone modules
 * exposing an integration point for a shell that did not exist yet. The shell
 * (`#4749`) then landed without calling any of them, so ~3,900 lines of
 * working, tested code sat in the tree while Vite tree-shook every byte of it
 * out of the bundle — green CI, invisible features. This module is the
 * integration point that was missing.
 *
 * ## The contract
 *
 * Every panel here is mounted on a route *change* and torn down on the next
 * one. `App.render()` runs on each poll tick, so a panel that remounted there
 * would refetch and flicker on every tick; `App` guards that by tracking the
 * mounted route, and each `mount*` returns a teardown so anything holding a
 * live connection (the feed's `EventSource`) is released.
 *
 * Panels own their own data. None of them read the fleet snapshot the app
 * polls — that is what lets a panel route paint before the first poll returns.
 */

import { el } from "./dom";
import { isAuthenticatedViewer } from "./api";
import { HistoricalChartsPanel } from "./historicalChartsPanel";
import { LiveFeedPanel } from "./liveFeedPanel";
import { currentSurface } from "./analytics/bootstrap";
import { mountTokenAnalytics } from "./analytics/render";
import type { PanelRouteName } from "./router";

/** Status-line text per panel route. The fleet's "Updated HH:MM:SS" is
 * meaningless here — these panels are not driven by the fleet poll. */
export const PANEL_STATUS: Readonly<Record<PanelRouteName, string>> = {
  charts: "Historical charts",
  tokens: "Token & cost analytics",
  feed: "Live event feed",
};

/** A mounted panel's teardown. Panels with no live resource return a no-op. */
export type PanelTeardown = () => void;

/**
 * Render a mount failure into `container`.
 *
 * `void somePromise()` discards rejections, which surface as unhandled
 * promise rejections and leave the panel blank — the panel tests catch
 * exactly that. Every async mount below routes its failure here instead.
 */
export function renderMountError(container: HTMLElement, error: unknown): void {
  const message = error instanceof Error ? error.message : String(error);
  container.replaceChildren(
    el("p", { class: "panel-route__note", data: { testid: "panel-error" } }, `Could not load: ${message}`),
  );
}

function section(title: string, ...children: (Node | null)[]): HTMLElement {
  return el("section", { class: "panel-route" }, el("h2", { class: "panel-route__title" }, title), ...children);
}

/**
 * Which dataset a panel should read.
 *
 * The same route serves both audiences (issue #4795), so this resolves from
 * the server-injected auth state rather than the path. `/public/*` is redacted
 * server-side, so an anonymous viewer gets real — if reduced — content rather
 * than an error or an empty panel.
 */
export function historyBasePath(): string {
  return isAuthenticatedViewer() ? "/api/history" : "/public/history";
}

function mountCharts(root: HTMLElement): PanelTeardown {
  const outcomes = el("div", { class: "chart-slot", data: { testid: "chart-outcomes" } });
  const successRate = el("div", { class: "chart-slot", data: { testid: "chart-success-rate" } });
  const durations = el("div", { class: "chart-slot", data: { testid: "chart-durations" } });

  root.replaceChildren(
    section(
      "Historical charts",
      el("p", { class: "panel-route__note" }, "Sweep outcomes, success rate and duration percentiles over time."),
      outcomes,
      successRate,
      durations,
    ),
  );

  const panel = new HistoricalChartsPanel({
    basePath: historyBasePath(),
    outcomesContainer: outcomes,
    successRateContainer: successRate,
    durationsContainer: durations,
  });
  panel.refresh().catch((error: unknown) => renderMountError(outcomes, error));

  return () => {};
}

function mountTokens(root: HTMLElement): PanelTeardown {
  const container = el("div", { data: { testid: "token-analytics" } });
  root.replaceChildren(container);

  // `currentSurface()` reads the injected auth state, NOT the path — under the
  // single-URL layout both audiences share one URL, so a path check would call
  // every visitor authenticated (see analytics/bootstrap.ts). Signed in gets
  // per-account detail; anonymous gets the pool-level aggregate (#4870).
  mountTokenAnalytics(container, { surface: currentSurface() }).catch((error: unknown) =>
    renderMountError(container, error),
  );

  return () => {};
}

function mountFeed(root: HTMLElement): PanelTeardown {
  const feed = el("div", { data: { testid: "live-feed" } });

  root.replaceChildren(
    section(
      "Live event feed",
      // Honest about the gap rather than rendering a silently-partial view:
      // sweep.started/completed/outcome all flow today, but sweep.phase is
      // never emitted (#4863), so phase transitions are missing and the
      // per-sweep timeline has nothing to draw. Drop this note when #4863
      // lands.
      el(
        "p",
        { class: "panel-route__note", data: { testid: "feed-phase-caveat" } },
        "Sweep lifecycle events as they arrive. Phase transitions are not shown yet — " +
          "the daemon does not emit sweep.phase telemetry (see issue #4863).",
      ),
      feed,
    ),
  );

  const panel = new LiveFeedPanel({
    container: feed,
    url: isAuthenticatedViewer() ? "/api/events" : "/public/events",
  });
  panel.start();

  // The one panel that genuinely needs teardown: it holds an open SSE
  // connection that would otherwise survive navigation and leak per route
  // change.
  return () => panel.stop();
}

const MOUNTERS: Readonly<Record<PanelRouteName, (root: HTMLElement) => PanelTeardown>> = {
  charts: mountCharts,
  tokens: mountTokens,
  feed: mountFeed,
};

/** Mount `name` into `root`, replacing its contents. Returns the teardown. */
export function mountPanel(name: PanelRouteName, root: HTMLElement): PanelTeardown {
  return MOUNTERS[name](root);
}
