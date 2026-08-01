/**
 * Browser entrypoint — wiring only. Everything testable lives in `app.ts`,
 * which this module constructs with the real DOM, the real clock, and the real
 * `fetch`.
 */

import "./styles.css";
import { App } from "./app";
import { wireAccountMenu } from "./accountMenu";
import { onRouteChange, parseRoute, routeToHash } from "./router";

const root = document.getElementById("app");
if (!(root instanceof HTMLElement)) {
  throw new Error("#app container is missing from index.html");
}

wireAccountMenu(document);

const app = new App({
  root,
  statusEl: document.getElementById("refresh-status"),
  refreshButton: document.getElementById("refresh-button"),
});

/** Mark the nav link matching the active route, so the user can see where
 * they are. Driven from the parsed route rather than the raw hash so an
 * unrecognized hash highlights Fleet, matching where it actually lands. */
function syncNav(doc: Document, hash: string): void {
  const active = routeToHash(parseRoute(hash));
  for (const link of doc.querySelectorAll<HTMLAnchorElement>(".topbar__navlink")) {
    if (link.getAttribute("href") === active) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  }
}

syncNav(document, window.location.hash);

onRouteChange((route) => {
  app.navigate(route);
  syncNav(document, window.location.hash);
});

void app.start(window.location.hash).then(() => app.startPolling());

// Pause polling while the tab is hidden and catch up on return: a laptop lid
// closed overnight should not queue hundreds of `/api/fleet-state` requests.
document.addEventListener("visibilitychange", () => {
  if (document.hidden) {
    app.stop();
  } else {
    // `startPolling()` first: it clears the stopped flag that `stop()` set, and
    // `refresh()` is a no-op while that flag is up.
    app.startPolling();
    app.navigate(parseRoute(window.location.hash));
    void app.refresh();
  }
});
