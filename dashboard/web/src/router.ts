/**
 * Hash-based routing (`#/` and `#/hosts/<hostId>`).
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

export type Route = { name: "overview" } | { name: "host"; hostId: string };

export const OVERVIEW: Route = { name: "overview" };

export function parseRoute(hash: string): Route {
  const path = hash.replace(/^#/, "");
  const match = /^\/hosts\/(.+)$/.exec(path);
  if (!match?.[1]) return OVERVIEW;
  try {
    return { name: "host", hostId: decodeURIComponent(match[1]) };
  } catch {
    // A malformed percent-escape (hand-edited URL) is not worth an error page.
    return { name: "host", hostId: match[1] };
  }
}

export function routeToHash(route: Route): string {
  return route.name === "overview" ? "#/" : `#/hosts/${encodeURIComponent(route.hostId)}`;
}

/** Subscribe to hash changes; returns an unsubscribe function. */
export function onRouteChange(handler: (route: Route) => void, target: Window = window): () => void {
  const listener = () => handler(parseRoute(target.location.hash));
  target.addEventListener("hashchange", listener);
  return () => target.removeEventListener("hashchange", listener);
}
