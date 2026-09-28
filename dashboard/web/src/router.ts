/**
 * Hash-based routing (`#/`, `#/hosts/<hostId>`, `#/queue`, `#/charts`,
 * `#/tokens`, `#/spend`, `#/feed`, `#/live`).
 *
 * Hash routing rather than the History API is a deliberate deploy-shape
 * decision, not laziness. The UI ships as Workers Assets on the *same* Worker
 * that serves `/api/*` (see `../../wrangler.toml`), and that Worker's asset
 * router is configured `not_found_handling = "none"` so unmatched paths fall
 * through to the API handler. A History-API route like `/hosts/mac-1` would
 * therefore reach the Worker and 404 on a hard refresh; the SPA-rewrite
 * setting that would fix it (`single-page-application`) rewrites *everything*
 * unmatched to `index.html`, which would shadow `/api/*` itself. A hash is
 * never sent to the server, so both problems disappear and deep links stay
 * shareable.
 */

export type Route =
  | { name: "overview" }
  | { name: "host"; hostId: string }
  /** The fleet work queue (Issue #8852) — a view over the polled snapshot,
   * like `overview`/`host`, not a self-fetching panel. */
  | { name: "queue" }
  | { name: "charts" }
  | { name: "tokens" }
  | { name: "spend" }
  | { name: "feed" }
  /** The perpetually-updating status board (issue #9077). */
  | { name: "live" };

export const OVERVIEW: Route = { name: "overview" };

/** Routes that render a self-contained panel owning its own data fetching,
 * rather than a view over the fleet snapshot the app polls (issue #4895). */
export type PanelRouteName = "charts" | "tokens" | "spend" | "feed" | "live";

const PANEL_ROUTES: Readonly<Record<string, PanelRouteName>> = {
  "/charts": "charts",
  "/tokens": "tokens",
  "/spend": "spend",
  "/feed": "feed",
  "/live": "live",
};

export function isPanelRoute(route: Route): route is { name: PanelRouteName } {
  // Derived from `PANEL_ROUTES` rather than an `||` chain of literals: the
  // chain silently omitted a new panel from `routeToHash` when one was added,
  // which turned its nav link into a link back to the overview.
  return Object.values(PANEL_ROUTES).includes(route.name as PanelRouteName);
}

export function parseRoute(hash: string): Route {
  const path = hash.replace(/^#/, "");

  const panel = PANEL_ROUTES[path];
  if (panel) return { name: panel };
  if (path === "/queue") return { name: "queue" };

  const match = /^\/hosts\/(.+)$/.exec(path);
  // Anything unrecognized falls back to the overview rather than erroring —
  // a hand-edited or stale bookmark should land somewhere useful.
  if (!match?.[1]) return OVERVIEW;
  try {
    return { name: "host", hostId: decodeURIComponent(match[1]) };
  } catch {
    // A malformed percent-escape (hand-edited URL) is not worth an error page.
    return { name: "host", hostId: match[1] };
  }
}

export function routeToHash(route: Route): string {
  if (route.name === "host") return `#/hosts/${encodeURIComponent(route.hostId)}`;
  if (isPanelRoute(route) || route.name === "queue") return `#/${route.name}`;
  return "#/";
}

/** Subscribe to hash changes; returns an unsubscribe function. */
export function onRouteChange(handler: (route: Route) => void, target: Window = window): () => void {
  const listener = () => handler(parseRoute(target.location.hash));
  target.addEventListener("hashchange", listener);
  return () => target.removeEventListener("hashchange", listener);
}
