/**
 * Entry point for the token/cost analytics view (Epic #4702, Phase 3,
 * issue #4752).
 *
 * Intentionally thin: it resolves the surface, mounts the panel, and wires the
 * refresh button. All logic lives in the sibling modules so it is testable
 * without a browser.
 *
 * **Surface resolution** is the one non-obvious bit, and issue #4795 changed
 * its basis. This module originally derived the surface from the URL: a page
 * under `/public` was the public surface, anything else authenticated. That
 * held while the two audiences had two URLs. Under the single-URL layout they
 * share one — `/public` is now a 301 to `/`, and `/` serves both audiences —
 * so `surfaceFromPath("/")` would answer "authenticated" for *every* visitor,
 * and an anonymous one would render the full panel, request `/api/history`,
 * and get a 403.
 *
 * The surface now comes from the auth state the Worker injects into the page
 * after validating the Cloudflare Access JWT (`../api.js`'s
 * `isAuthenticatedViewer`, stamped by `../../../src/index.ts`'s `handleRoot`).
 * That is still a server-side decision the page cannot talk itself out of;
 * the flag only decides which dataset to *ask* for, and both routes stay
 * enforced server-side regardless of what the browser claims.
 *
 * **Nothing runs at import time.** This package has no bundler and no HTML
 * entry point yet; `src/index.ts` is a barrel that the `node`-environment
 * tests import, so a module that touched `document` on load would break them.
 * An app shell (issue #4749) calls `startTokenAnalytics` explicitly instead.
 * For the same reason the panel's stylesheet, `analytics.css`, is *not*
 * imported here — a CSS import is a bundler affordance, so the shell links the
 * file itself.
 */

import { isAuthenticatedViewer } from "../api.js";
import { mountTokenAnalytics } from "./render.js";
import type { DashboardSurface } from "./render.js";

/**
 * The surface for the current viewer, from the server-injected auth state.
 *
 * Fails closed to `"public"`: `isAuthenticatedViewer` treats a missing or
 * malformed flag as anonymous, so a page that never got the injection renders
 * the pool-level public panel (`render.ts`) and fetches from `/public/history`
 * rather than firing an authenticated request it has no evidence it is
 * entitled to.
 */
export function currentSurface(scope: typeof globalThis = globalThis): DashboardSurface {
  return isAuthenticatedViewer(scope) ? "authenticated" : "public";
}

/**
 * @deprecated Superseded by {@link currentSurface}. Issue #4795 collapsed the
 * two audiences onto one URL, so a pathname no longer identifies the viewer —
 * `/` serves both, and this function answers `"authenticated"` for it. Kept
 * only so the rename is a separate, reviewable change; it has no callers.
 */
export function surfaceFromPath(pathname: string): DashboardSurface {
  return pathname === "/public" || pathname.startsWith("/public/") ? "public" : "authenticated";
}

/** Options for {@link startTokenAnalytics}. */
export interface StartTokenAnalyticsOptions {
  /** Element id the panel mounts into. Default `"token-analytics"`. */
  containerId?: string;
  /** Element id of an optional refresh button. Default `"refresh-button"`. */
  refreshButtonId?: string;
  /**
   * Overrides the surface derived from the injected auth state. Provided only
   * so a shell that already knows the viewer need not re-derive it, and for
   * tests; omit it and {@link currentSurface} decides.
   */
  surface?: DashboardSurface;
}

/**
 * Mounts the analytics panel into `doc` and wires the refresh button.
 *
 * Returns the `refresh` callback (also invoked once immediately) so a caller
 * can re-render on its own schedule, or `undefined` when the container is
 * absent — a shell that does not host this panel is not an error.
 */
export function startTokenAnalytics(
  doc: Document,
  options: StartTokenAnalyticsOptions = {},
): (() => void) | undefined {
  const containerId = options.containerId ?? "token-analytics";
  const refreshButtonId = options.refreshButtonId ?? "refresh-button";

  const container = doc.getElementById(containerId);
  if (!container) return undefined;

  const surface = options.surface ?? currentSurface();

  const refresh = (): void => {
    void mountTokenAnalytics(container, { surface });
  };

  doc.getElementById(refreshButtonId)?.addEventListener("click", refresh);
  refresh();
  return refresh;
}
