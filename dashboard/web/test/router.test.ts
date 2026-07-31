import { describe, expect, it } from "vitest";

import { OVERVIEW, parseRoute, routeToHash } from "../src/router";

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
