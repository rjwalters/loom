/**
 * Browser entrypoint — wiring only. Everything testable lives in `app.ts`,
 * which this module constructs with the real DOM, the real clock, and the real
 * `fetch`.
 */

import "./styles.css";
import { App } from "./app";
import { onRouteChange, parseRoute } from "./router";

const root = document.getElementById("app");
if (!(root instanceof HTMLElement)) {
  throw new Error("#app container is missing from index.html");
}

const app = new App({
  root,
  statusEl: document.getElementById("refresh-status"),
  refreshButton: document.getElementById("refresh-button"),
});

onRouteChange((route) => app.navigate(route));

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
