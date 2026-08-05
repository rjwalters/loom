/**
 * End-to-end-ish controller tests: real DOM (happy-dom), fake network, fake
 * clock, fake timers. Covers the state machine the acceptance criteria call
 * out — loading, loaded, empty, error, error-over-stale-data — plus routing
 * between the overview and a drill-down.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { App } from "../src/app";
import { FleetStateError } from "../src/api";
import type { FetchLike } from "../src/historyClient";
import { parseFleetSnapshot } from "../src/parse";
import type { FleetSnapshot, HistoryQueryResult, HistoryRecord } from "../src/types";
import { EMPTY_SNAPSHOT, HEALTHY_HOST_ID, NOW, multiHostSnapshot } from "./fixtures";

/**
 * A `FetchLike` stub for the host-history panel's own `/api/history` (or
 * `/public/history`) fetch — issue #5355. Every `App`/host-route test needs
 * one: without it, `HostHistoryPanel.refresh()` falls through to the real
 * global `fetch`, which fails against nothing listening on `localhost` in
 * the test environment. The failure is caught internally (so tests still
 * pass), but it is a real, noisy network attempt this stub avoids.
 */
function stubHistoryFetch(records: HistoryRecord[] = []): { fetchImpl: FetchLike; calls: string[] } {
  const calls: string[] = [];
  const page: HistoryQueryResult = { records, nextCursor: null };
  const fetchImpl: FetchLike = async (url: string) => {
    calls.push(url);
    return { ok: true, status: 200, async json() { return page; } };
  };
  return { fetchImpl, calls };
}

/** A hand-driven scheduler, so a "poll" is one explicit call and no test ever
 * waits on a wall clock. */
class FakeScheduler {
  private next = 1;
  private readonly pending = new Map<number, () => void>();

  setTimeout = (handler: () => void, _ms: number): number => {
    const handle = this.next++;
    this.pending.set(handle, handler);
    return handle;
  };

  clearTimeout = (handle: number): void => {
    this.pending.delete(handle);
  };

  get pendingCount(): number {
    return this.pending.size;
  }

  /** Fire every currently-scheduled timer. */
  tick(): void {
    const handlers = [...this.pending.entries()];
    this.pending.clear();
    for (const [, handler] of handlers) handler();
  }
}

let root: HTMLElement;
let statusEl: HTMLElement;
let refreshButton: HTMLElement;
let scheduler: FakeScheduler;

beforeEach(() => {
  document.body.innerHTML = "";
  root = document.createElement("main");
  statusEl = document.createElement("span");
  refreshButton = document.createElement("button");
  document.body.append(root, statusEl, refreshButton);
  scheduler = new FakeScheduler();
});

afterEach(() => {
  vi.restoreAllMocks();
});

function makeApp(
  fetchState: () => Promise<FleetSnapshot>,
  pollIntervalMs = 10_000,
  historyFetchImpl: FetchLike = stubHistoryFetch().fetchImpl,
): App {
  return new App({
    root,
    statusEl,
    refreshButton,
    fetchState,
    now: () => NOW,
    pollIntervalMs,
    scheduler,
    historyFetchImpl,
  });
}

const loaded = () => parseFleetSnapshot(multiHostSnapshot());

describe("App — loading state", () => {
  it("renders the loading state before the first response arrives", async () => {
    let release: (snapshot: FleetSnapshot) => void = () => {};
    const pending = new Promise<FleetSnapshot>((resolve) => {
      release = resolve;
    });
    const app = makeApp(() => pending);

    const started = app.start("#/");
    expect(root.querySelector('[data-testid="loading"]')).not.toBeNull();
    expect(root.getAttribute("aria-busy")).toBe("true");

    release(loaded());
    await started;

    expect(root.querySelector('[data-testid="loading"]')).toBeNull();
    expect(root.querySelector('[data-testid="fleet-overview"]')).not.toBeNull();
    expect(root.getAttribute("aria-busy")).toBe("false");
  });
});

describe("App — loaded state", () => {
  it("renders the overview from a single /api/fleet-state call", async () => {
    const fetchState = vi.fn(async () => loaded());
    const app = makeApp(fetchState);
    await app.start("#/");

    expect(fetchState).toHaveBeenCalledTimes(1);
    expect(root.querySelectorAll('[data-testid="host-card"]')).toHaveLength(6);
    expect(statusEl.textContent).toMatch(/^Updated /);
  });

  it("renders the drill-down when the initial hash names a host", async () => {
    const app = makeApp(async () => loaded());
    await app.start(`#/hosts/${HEALTHY_HOST_ID}`);

    const detail = root.querySelector('[data-testid="host-detail"]');
    expect(detail?.getAttribute("data-host")).toBe(HEALTHY_HOST_ID);
    expect(root.querySelectorAll('[data-testid="sweep-row"]')).toHaveLength(2);
  });

  it("navigates between views without re-fetching", async () => {
    const fetchState = vi.fn(async () => loaded());
    const app = makeApp(fetchState);
    await app.start("#/");

    app.navigate({ name: "host", hostId: HEALTHY_HOST_ID });
    expect(root.querySelector('[data-testid="host-detail"]')).not.toBeNull();

    app.navigate({ name: "overview" });
    expect(root.querySelector('[data-testid="fleet-overview"]')).not.toBeNull();
    // The drill-down is a projection of the snapshot already in hand.
    expect(fetchState).toHaveBeenCalledTimes(1);
  });

  it("mounts a history panel on the drill-down and fetches host-scoped history exactly once", async () => {
    const { fetchImpl, calls } = stubHistoryFetch();
    const app = makeApp(async () => loaded(), 10_000, fetchImpl);
    await app.start(`#/hosts/${HEALTHY_HOST_ID}`);

    expect(root.querySelector('[data-testid="host-history-panel"]')).not.toBeNull();
    await vi.waitFor(() => expect(calls).toHaveLength(1));
    const url = new URL(calls[0]!, "http://example.test");
    expect(url.searchParams.get("host")).toBe(HEALTHY_HOST_ID);
  });

  it("does not re-fetch the history panel on a poll tick for the same host (#5355)", async () => {
    const { fetchImpl, calls } = stubHistoryFetch();
    const app = makeApp(async () => loaded(), 10_000, fetchImpl);
    await app.start(`#/hosts/${HEALTHY_HOST_ID}`);
    await vi.waitFor(() => expect(calls).toHaveLength(1));

    // A poll tick re-renders the whole host-detail tree from the snapshot —
    // the history panel's container must be reused, not rebuilt/re-fetched.
    scheduler.tick();
    await vi.waitFor(() => expect(root.querySelector('[data-testid="host-history-panel"]')).not.toBeNull());
    expect(calls).toHaveLength(1);
  });

  it("re-fetches the history panel when navigating to a different host", async () => {
    const { fetchImpl, calls } = stubHistoryFetch();
    const app = makeApp(async () => loaded(), 10_000, fetchImpl);
    await app.start(`#/hosts/${HEALTHY_HOST_ID}`);
    await vi.waitFor(() => expect(calls).toHaveLength(1));

    app.navigate({ name: "host", hostId: "fleet-idle-5" });
    await vi.waitFor(() => expect(calls).toHaveLength(2));
    const url = new URL(calls[1]!, "http://example.test");
    expect(url.searchParams.get("host")).toBe("fleet-idle-5");
  });

  it("shows the unknown-host state for a stale bookmark", async () => {
    const app = makeApp(async () => loaded());
    await app.start("#/hosts/host-that-left");

    expect(root.querySelector('[data-testid="unknown-host"]')?.textContent).toContain(
      "host-that-left",
    );
  });
});

describe("App — empty state", () => {
  it("renders the empty-fleet guidance, not an error", async () => {
    const app = makeApp(async () => parseFleetSnapshot(EMPTY_SNAPSHOT));
    await app.start("#/");

    expect(root.querySelector('[data-testid="empty-fleet"]')).not.toBeNull();
    expect(root.querySelector('[data-testid="error"]')).toBeNull();
  });
});

describe("App — error state", () => {
  it("renders a full-page error when the first load fails", async () => {
    const app = makeApp(async () => {
      throw new FleetStateError("/api/fleet-state returned HTTP 500", { status: 500 });
    });
    await app.start("#/");

    const error = root.querySelector('[data-testid="error"]');
    expect(error?.textContent).toContain("HTTP 500");
    expect(statusEl.textContent).toBe("Failed");
  });

  it("surfaces the Cloudflare Access hint on a 403", async () => {
    const app = makeApp(async () => {
      throw new FleetStateError("403 — your Cloudflare Access session may have expired", {
        status: 403,
      });
    });
    await app.start("#/");

    expect(root.querySelector('[data-testid="error"]')?.textContent).toContain(
      "Reload the page to re-authenticate",
    );
  });

  it("retries from the error state", async () => {
    let attempt = 0;
    const app = makeApp(async () => {
      attempt += 1;
      if (attempt === 1) throw new FleetStateError("down", { status: 502 });
      return loaded();
    });
    await app.start("#/");
    expect(root.querySelector('[data-testid="error"]')).not.toBeNull();

    const retry = root.querySelector<HTMLButtonElement>('[data-testid="error"] button');
    retry?.click();
    await vi.waitFor(() => {
      expect(root.querySelector('[data-testid="fleet-overview"]')).not.toBeNull();
    });
    expect(root.querySelector('[data-testid="error"]')).toBeNull();
  });

  it("keeps the last good snapshot on screen and shows a banner when a refresh fails", async () => {
    let attempt = 0;
    const app = makeApp(async () => {
      attempt += 1;
      if (attempt === 1) return loaded();
      throw new FleetStateError("transient", { status: 503 });
    });
    await app.start("#/");
    expect(root.querySelectorAll('[data-testid="host-card"]')).toHaveLength(6);

    await app.refresh();

    // Banner over live (stale) data — not a blanked page.
    expect(root.querySelector('[data-testid="error"]')?.classList.contains("state--banner")).toBe(true);
    expect(root.querySelectorAll('[data-testid="host-card"]')).toHaveLength(6);
    expect(statusEl.textContent).toBe("Stale — last refresh failed");
  });

  it("clears the banner once a refresh succeeds again", async () => {
    let attempt = 0;
    const app = makeApp(async () => {
      attempt += 1;
      if (attempt === 2) throw new FleetStateError("transient", { status: 503 });
      return loaded();
    });
    await app.start("#/");
    await app.refresh();
    expect(root.querySelector('[data-testid="error"]')).not.toBeNull();

    await app.refresh();
    expect(root.querySelector('[data-testid="error"]')).toBeNull();
  });

  it("wraps a non-Error rejection", async () => {
    const app = makeApp(async () => {
      throw "string failure";
    });
    await app.start("#/");
    expect(root.querySelector('[data-testid="error"]')?.textContent).toContain("string failure");
  });

  it("ignores an aborted request", async () => {
    const app = makeApp(async () => {
      throw new DOMException("aborted", "AbortError");
    });
    await app.start("#/");
    // Still loading — an abort is not a failure to report.
    expect(root.querySelector('[data-testid="error"]')).toBeNull();
    expect(root.querySelector('[data-testid="loading"]')).not.toBeNull();
  });
});

describe("App — polling", () => {
  it("schedules the next poll after each successful refresh", async () => {
    const fetchState = vi.fn(async () => loaded());
    const app = makeApp(fetchState);
    await app.start("#/");
    expect(scheduler.pendingCount).toBe(1);

    scheduler.tick();
    await vi.waitFor(() => expect(fetchState).toHaveBeenCalledTimes(2));
  });

  it("stops scheduling once stopped", async () => {
    const app = makeApp(async () => loaded());
    await app.start("#/");
    app.stop();
    expect(scheduler.pendingCount).toBe(0);
  });

  it("does not arm a timer when polling is disabled", async () => {
    const app = makeApp(async () => loaded(), 0);
    await app.start("#/");
    expect(scheduler.pendingCount).toBe(0);
  });

  it("does not stack concurrent refreshes", async () => {
    let resolveFetch: (snapshot: FleetSnapshot) => void = () => {};
    const fetchState = vi.fn(
      () =>
        new Promise<FleetSnapshot>((resolve) => {
          resolveFetch = resolve;
        }),
    );
    const app = makeApp(fetchState);
    const first = app.start("#/");
    void app.refresh();
    void app.refresh();
    expect(fetchState).toHaveBeenCalledTimes(1);

    resolveFetch(loaded());
    await first;
  });

  it("refreshes when the toolbar button is clicked", async () => {
    const fetchState = vi.fn(async () => loaded());
    const app = makeApp(fetchState);
    await app.start("#/");

    refreshButton.click();
    await vi.waitFor(() => expect(fetchState).toHaveBeenCalledTimes(2));
  });
});
