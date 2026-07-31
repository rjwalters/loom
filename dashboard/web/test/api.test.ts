import { describe, expect, it, vi } from "vitest";

import { FLEET_STATE_PATH, FleetStateError, fetchFleetState } from "../src/api";
import { EMPTY_SNAPSHOT, HEALTHY_HOST_ID, multiHostSnapshot } from "./fixtures";

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

describe("fetchFleetState", () => {
  it("requests GET /api/fleet-state and nothing else", async () => {
    const fetchImpl = vi.fn(async () => jsonResponse(multiHostSnapshot()));
    await fetchFleetState({ fetchImpl: fetchImpl as unknown as typeof fetch });

    expect(fetchImpl).toHaveBeenCalledTimes(1);
    const [url, init] = fetchImpl.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe(FLEET_STATE_PATH);
    // Same-origin credentials so the Cloudflare Access session cookie rides
    // along; no Authorization header is ever constructed in-app.
    expect(init.credentials).toBe("same-origin");
    expect(JSON.stringify(init.headers)).not.toMatch(/authorization/i);
  });

  it("returns a parsed snapshot", async () => {
    const snapshot = await fetchFleetState({
      fetchImpl: (async () => jsonResponse(multiHostSnapshot())) as unknown as typeof fetch,
    });
    expect(snapshot.hosts[HEALTHY_HOST_ID]?.health?.record.daemon_version).toBe("0.16.0");
    expect(snapshot.activeSweeps).toHaveLength(3);
  });

  it("returns an empty snapshot for an empty fleet (not an error)", async () => {
    const snapshot = await fetchFleetState({
      fetchImpl: (async () => jsonResponse(EMPTY_SNAPSHOT)) as unknown as typeof fetch,
    });
    expect(snapshot).toEqual({ hosts: {}, activeSweeps: [] });
  });

  it("throws FleetStateError on a non-2xx status", async () => {
    const error = await fetchFleetState({
      fetchImpl: (async () => jsonResponse({ error: "boom" }, 500)) as unknown as typeof fetch,
    }).catch((caught: unknown) => caught);

    expect(error).toBeInstanceOf(FleetStateError);
    expect((error as FleetStateError).status).toBe(500);
    expect((error as FleetStateError).isAuthHint).toBe(false);
  });

  it("flags 401/403 as an Access-session hint", async () => {
    for (const status of [401, 403]) {
      const error = (await fetchFleetState({
        fetchImpl: (async () => new Response("<html>login</html>", { status })) as unknown as typeof fetch,
      }).catch((caught: unknown) => caught)) as FleetStateError;

      expect(error).toBeInstanceOf(FleetStateError);
      expect(error.isAuthHint).toBe(true);
      expect(error.message).toMatch(/Access session/);
    }
  });

  it("throws FleetStateError when the body is not JSON", async () => {
    const error = await fetchFleetState({
      fetchImpl: (async () => new Response("<html>not json</html>", { status: 200 })) as unknown as typeof fetch,
    }).catch((caught: unknown) => caught);

    expect(error).toBeInstanceOf(FleetStateError);
    expect((error as Error).message).toMatch(/did not return JSON/);
  });

  it("throws FleetStateError when the network is unreachable", async () => {
    const error = await fetchFleetState({
      fetchImpl: (async () => {
        throw new TypeError("Failed to fetch");
      }) as unknown as typeof fetch,
    }).catch((caught: unknown) => caught);

    expect(error).toBeInstanceOf(FleetStateError);
    expect((error as Error).message).toMatch(/could not reach/);
  });

  it("re-throws an AbortError unchanged so callers can ignore it", async () => {
    const error = await fetchFleetState({
      fetchImpl: (async () => {
        throw new DOMException("aborted", "AbortError");
      }) as unknown as typeof fetch,
    }).catch((caught: unknown) => caught);

    expect(error).toBeInstanceOf(DOMException);
    expect((error as DOMException).name).toBe("AbortError");
  });

  it("honours a baseUrl override for a split deploy", async () => {
    const fetchImpl = vi.fn(async () => jsonResponse(EMPTY_SNAPSHOT));
    await fetchFleetState({
      fetchImpl: fetchImpl as unknown as typeof fetch,
      baseUrl: "https://fleet.example.com",
    });
    const calls = fetchImpl.mock.calls as unknown as Array<[string, RequestInit]>;
    expect(calls[0]?.[0]).toBe("https://fleet.example.com/api/fleet-state");
  });
});
