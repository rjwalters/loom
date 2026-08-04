/**
 * Shell-integration tests for the panel routes (#4895).
 *
 * These exist because every other suite in this package passes without them.
 * #4750, #4751 and #4752 each shipped with thorough module-level tests, and
 * all of them stayed green while the features were absent from the built
 * bundle — nothing in the app shell imported them, so Vite tree-shook the lot.
 * Module tests prove a panel *works*; only a shell test proves a user can
 * reach it.
 *
 * So the assertions here are deliberately about *reachability and lifecycle*,
 * not rendering fidelity — that is the sibling suites' job.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { PANEL_STATUS, mountPanel } from "../src/panels";
import type { PanelRouteName } from "../src/router";

const PANEL_NAMES: PanelRouteName[] = ["charts", "tokens", "feed"];

/** Panels fetch on mount. Nothing here asserts on the response — the point is
 * that mounting reaches the network at all, and never throws when it fails. */
function stubFetch(): ReturnType<typeof vi.fn> {
  const impl = vi.fn(async () =>
    new Response(JSON.stringify({ records: [], nextCursor: null }), {
      status: 200,
      headers: { "content-type": "application/json" },
    }),
  );
  globalThis.fetch = impl as unknown as typeof fetch;
  return impl;
}

let root: HTMLElement;
let realFetch: typeof fetch;

beforeEach(() => {
  realFetch = globalThis.fetch;
  document.body.replaceChildren();
  root = document.createElement("div");
  document.body.appendChild(root);
  stubFetch();
});

afterEach(() => {
  globalThis.fetch = realFetch;
  vi.restoreAllMocks();
});

describe("mountPanel", () => {
  it.each(PANEL_NAMES)("%s renders content into the root", (name) => {
    mountPanel(name, root);
    expect(root.childElementCount).toBeGreaterThan(0);
  });

  it.each(PANEL_NAMES)("%s returns a teardown that does not throw", (name) => {
    const teardown = mountPanel(name, root);
    expect(typeof teardown).toBe("function");
    expect(() => teardown()).not.toThrow();
  });

  it.each(PANEL_NAMES)("%s replaces prior contents rather than appending", (name) => {
    // Tag the pre-existing node so the assertion is about *that* element
    // rather than any <p> a panel legitimately renders inside itself.
    const stale = document.createElement("p");
    stale.dataset.testid = "stale-content";
    root.appendChild(stale);

    mountPanel(name, root);

    expect(root.querySelector('[data-testid="stale-content"]')).toBeNull();
    expect(root.contains(stale)).toBe(false);
  });

  it("mounts the charts panel with its three chart slots", () => {
    mountPanel("charts", root);
    expect(root.querySelector('[data-testid="chart-outcomes"]')).not.toBeNull();
    expect(root.querySelector('[data-testid="chart-success-rate"]')).not.toBeNull();
    expect(root.querySelector('[data-testid="chart-durations"]')).not.toBeNull();
  });

  it("mounts the token analytics container", () => {
    mountPanel("tokens", root);
    expect(root.querySelector('[data-testid="token-analytics"]')).not.toBeNull();
  });

  it("mounts the live feed and states the sweep.phase gap", () => {
    mountPanel("feed", root);
    expect(root.querySelector('[data-testid="live-feed"]')).not.toBeNull();
    // #4863: phase transitions are not emitted, so the panel says so rather
    // than silently rendering a partial view. Remove with that issue.
    expect(root.querySelector('[data-testid="feed-phase-caveat"]')?.textContent).toContain("sweep.phase");
  });

  it("hits the network on mount for the data-backed panels", () => {
    const impl = stubFetch();
    mountPanel("charts", root);
    expect(impl).toHaveBeenCalled();
  });

  // A panel that throws on mount would blank the app. Every fetch path here is
  // expected to swallow its own failure and render a state instead.
  it.each(PANEL_NAMES)("%s survives a failing fetch", (name) => {
    globalThis.fetch = vi.fn(async () => {
      throw new Error("network down");
    }) as unknown as typeof fetch;
    expect(() => mountPanel(name, root)).not.toThrow();
  });
});

describe("PANEL_STATUS", () => {
  it("covers every panel route", () => {
    for (const name of PANEL_NAMES) {
      expect(PANEL_STATUS[name]).toBeTruthy();
    }
  });
});
