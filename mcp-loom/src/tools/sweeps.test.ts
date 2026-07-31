/**
 * Tests for the typed runtime-admission refusal on the MCP dispatch client
 * (issue #4494, epic #4489 Phase 5).
 *
 * The daemon emits `Response::RuntimeRejected` with a structured, secret-free
 * payload (role / runtime / source / unmet_capabilities / reason) when
 * fail-closed capability admission refuses a dispatch. Before this the bridge
 * had no model for the variant, so `dispatch_sweep` reported the useless
 * `Unexpected response: RuntimeRejected` and threw the payload away.
 */

import { describe, expect, it } from "vitest";
import {
  extractRuntimeRejection,
  formatDispatchTokenLine,
  formatRuntimeRejection,
  type RuntimeRejection,
} from "./sweeps.js";

/**
 * Wire frames are typed as the daemon's open response shape (the same
 * `{type, payload}` envelope `sendDaemonRequest` yields) so these fixtures
 * exercise the runtime parsing rather than the compiler's narrowing.
 */
type WireFrame = { type: string; payload?: unknown };

const WIRE: WireFrame = {
  type: "RuntimeRejected",
  payload: {
    role: "sweep-lifecycle",
    runtime: "codex",
    source: "default-config",
    unmet_capabilities: ["worktreeIsolation"],
    reason: "unmet capabilities: worktreeIsolation",
  },
};

const DISPATCHED: WireFrame = { type: "SweepDispatched", payload: {} };
const ERRORED: WireFrame = { type: "Error", payload: { message: "boom" } };
const MALFORMED: WireFrame = {
  type: "RuntimeRejected",
  payload: { role: "judge", unmet_capabilities: ["mcp", 7] },
};

describe("extractRuntimeRejection", () => {
  it("models the daemon's RuntimeRejected variant instead of discarding it", () => {
    const rejection = extractRuntimeRejection(WIRE);
    expect(rejection).not.toBeNull();
    expect(rejection).toEqual({
      role: "sweep-lifecycle",
      runtime: "codex",
      source: "default-config",
      unmet_capabilities: ["worktreeIsolation"],
      reason: "unmet capabilities: worktreeIsolation",
    });
  });

  it("returns null for every other response type", () => {
    expect(extractRuntimeRejection(DISPATCHED)).toBeNull();
    expect(extractRuntimeRejection(ERRORED)).toBeNull();
  });

  it("degrades defensively on a malformed payload rather than throwing", () => {
    expect(extractRuntimeRejection(MALFORMED)).toEqual({
      role: "judge",
      runtime: "unknown",
      source: "built-in",
      unmet_capabilities: ["mcp"],
      reason: "runtime admission refused",
    });
  });
});

describe("formatRuntimeRejection", () => {
  it("names the role, runtime, precedence tier, and unmet capabilities", () => {
    const rejection = extractRuntimeRejection(WIRE);
    expect(rejection).not.toBeNull();
    const line = formatRuntimeRejection(rejection as RuntimeRejection);
    expect(line).toContain("role=sweep-lifecycle");
    expect(line).toContain("runtime=codex");
    expect(line).toContain("selected by=default-config");
    expect(line).toContain("unmet capabilities: worktreeIsolation");
    // The full-sweep single-runtime limitation is spelled out, matching the
    // daemon-side diagnostic and defaults/docs/runtime-adapters.md.
    expect(line).toContain("a per-role binding cannot switch runtimes between phases");
    // Secret-free: nothing account/credential-shaped can appear here.
    expect(line.toLowerCase()).not.toContain("token");
    expect(line.toLowerCase()).not.toContain("oauth");
  });

  it("omits the unmet clause for config/adapter-shaped refusals", () => {
    const line = formatRuntimeRejection({
      role: "judge",
      runtime: "codex",
      source: "role-config",
      unmet_capabilities: [],
      reason: "unknown role name(s) in runtimes.roles: not-a-role",
    });
    expect(line).not.toContain("unmet capabilities");
    expect(line).toContain("not-a-role");
    expect(line).not.toContain("per-role binding cannot switch");
  });
});

/**
 * Issue #4689: `dispatch_sweep` used to print a bare `Token: unknown` right
 * alongside `Success`, reading as cosmetic rather than as "not yet known" —
 * the exact ambiguity that let dead dispatches (killed by a token-selection
 * failure) look like launched sweeps.
 */
describe("formatDispatchTokenLine", () => {
  it("never renders a bare 'unknown' token name — always a clarifying pending note", () => {
    const line = formatDispatchTokenLine("unknown");
    expect(line).toContain("unknown");
    expect(line).not.toBe("Token:      unknown");
    expect(line.toLowerCase()).toContain("not yet captured");
    // The clarifying text must not itself read as a failure — the dispatch
    // call genuinely succeeded in this case.
    expect(line.toLowerCase()).toContain("succeeded");
  });

  it("renders a real token name unchanged", () => {
    expect(formatDispatchTokenLine("agent3-2amlogic")).toBe("Token:      agent3-2amlogic");
  });
});
