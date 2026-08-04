/**
 * Loading / empty / error views.
 *
 * These are first-class views, not afterthoughts, because on this dashboard
 * they are the *common* states: a freshly-deployed backend has an empty
 * Durable Object until the first host pushes, and an expired Cloudflare Access
 * session is the single most likely fetch failure. Each one names what is
 * wrong and what to do about it — an empty fleet is a provisioning question
 * (`docs/deploy-runbook.md` §8-9), an auth failure is a reload.
 */

import { el } from "../dom";

export function loadingView(message = "Loading fleet state…"): HTMLElement {
  return el(
    "div",
    { class: "state state--loading", role: "status", data: { testid: "loading" } },
    el("div", { class: "state__spinner", aria: { hidden: "true" } }),
    el("p", { class: "state__message" }, message),
  );
}

export function emptyFleetView(): HTMLElement {
  return el(
    "div",
    { class: "state state--empty", data: { testid: "empty-fleet" } },
    el("h2", { class: "state__title" }, "No hosts are reporting yet"),
    el(
      "p",
      { class: "state__message" },
      "The backend is reachable and returned an empty fleet. A host appears here " +
        "once its daemon pushes its first telemetry record — within ~5 minutes of " +
        "enabling the exporter.",
    ),
    el(
      "ul",
      { class: "state__hints" },
      el("li", {}, "Provision an ingest key for the host (deploy runbook §8)."),
      el("li", {}, "Set the daemon's observability block and restart it (§9)."),
      el("li", {}, "Confirm records are landing (§10)."),
    ),
  );
}

export function errorView(error: Error, onRetry?: () => void): HTMLElement {
  const isAuthHint = (error as { isAuthHint?: boolean }).isAuthHint === true;
  return el(
    "div",
    { class: "state state--error", role: "alert", data: { testid: "error" } },
    el("h2", { class: "state__title" }, "Could not load fleet state"),
    el("p", { class: "state__message" }, error.message),
    isAuthHint
      ? el(
          "p",
          { class: "state__message" },
          "Reload the page to re-authenticate with Cloudflare Access.",
        )
      : null,
    onRetry
      ? el("button", { class: "button", type: "button", onClick: () => onRetry() }, "Retry")
      : null,
  );
}

/** Host id in the URL hash does not exist in the current snapshot — a stale
 * bookmark, or a host that stopped reporting and aged out of the snapshot. */
export function unknownHostView(hostId: string): HTMLElement {
  return el(
    "div",
    { class: "state state--empty", data: { testid: "unknown-host" } },
    el("h2", { class: "state__title" }, `No host “${hostId}” in the current snapshot`),
    el(
      "p",
      { class: "state__message" },
      "It may have been revoked, or it has not pushed a record since the backend " +
        "last restarted.",
    ),
    el("a", { class: "button", href: "#/" }, "Back to the fleet"),
  );
}
