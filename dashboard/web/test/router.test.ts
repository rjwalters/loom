import { describe, expect, it } from "vitest";

import { OVERVIEW, isPanelRoute, parseRoute, routeToHash } from "../src/router";

describe("parseRoute", () => {
  it("treats an empty or root hash as the overview", () => {
    expect(parseRoute("")).toEqual(OVERVIEW);
    expect(parseRoute("#")).toEqual(OVERVIEW);
    expect(parseRoute("#/")).toEqual(OVERVIEW);
    expect(parseRoute("#/nonsense")).toEqual(OVERVIEW);
  });

  it("parses a host drill-down", () => {
    expect(parseRoute("#/hosts/fleet-mac-1")).toEqual({ name: "host", hostId: "fleet-mac-1" });
  });

  it("round-trips a host id that needs escaping", () => {
    const route = { name: "host", hostId: "host/with space" } as const;
    expect(parseRoute(routeToHash(route))).toEqual(route);
  });

  it("does not throw on a malformed percent-escape", () => {
    expect(parseRoute("#/hosts/%E0%A4%A")).toEqual({ name: "host", hostId: "%E0%A4%A" });
  });

  it("renders the overview hash", () => {
    expect(routeToHash(OVERVIEW)).toBe("#/");
  });
});

// #4895: the routes that make the Phase-3 panels reachable at all.
describe("panel routes", () => {
  it.each([
    ["#/charts", "charts"],
    ["#/tokens", "tokens"],
    ["#/feed", "feed"],
  ])("parses %s", (hash, name) => {
    expect(parseRoute(hash)).toEqual({ name });
  });

  it("round-trips through routeToHash", () => {
    for (const name of ["charts", "tokens", "feed"] as const) {
      expect(routeToHash({ name })).toBe(`#/${name}`);
      expect(parseRoute(routeToHash({ name }))).toEqual({ name });
    }
  });

  it("identifies panel routes and excludes fleet routes", () => {
    expect(isPanelRoute({ name: "charts" })).toBe(true);
    expect(isPanelRoute({ name: "tokens" })).toBe(true);
    expect(isPanelRoute({ name: "feed" })).toBe(true);
    expect(isPanelRoute(OVERVIEW)).toBe(false);
    expect(isPanelRoute({ name: "host", hostId: "h" })).toBe(false);
  });

  // A panel name that is not an exact path must not swallow a host id, and an
  // unknown hash still lands somewhere useful rather than erroring.
  it.each(["#/chartsy", "#/charts/extra", "#/tokens?x=1", "#/nonsense"])(
    "falls back to the overview for %s",
    (hash) => {
      expect(parseRoute(hash)).toEqual(OVERVIEW);
    },
  );

  it("still parses a host whose id resembles a panel name", () => {
    expect(parseRoute("#/hosts/charts")).toEqual({ name: "host", hostId: "charts" });
  });
});
