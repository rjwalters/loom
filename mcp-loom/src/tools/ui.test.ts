/**
 * Tests for `classifyHeartbeat` (Issue #4794, daemon-only since #13198) and
 * `buildUIState` (daemon-only since #13821).
 *
 * Both report loom-daemon liveness alone; neither the retired desktop app's
 * console log nor the `state.json` it used to write is consulted.
 */

import { describe, expect, it } from "vitest";
import { buildUIState, classifyHeartbeat } from "./ui.js";

describe("classifyHeartbeat", () => {
  it("reports daemon_healthy with the in-flight sweep count for a live daemon", () => {
    const result = classifyHeartbeat({ alive: true, inFlightSweeps: 8 });

    expect(result.status).toBe("daemon_healthy");
    expect(result.daemon).toEqual({ alive: true, inFlightSweeps: 8 });
    expect(result.message).toContain("loom-daemon is responding on its Unix socket");
    expect(result.message).toContain("8 sweeps in flight");
  });

  it("uses singular 'sweep' wording for exactly one in-flight sweep", () => {
    const result = classifyHeartbeat({ alive: true, inFlightSweeps: 1 });
    expect(result.message).toContain("1 sweep in flight");
    expect(result.message).not.toContain("1 sweeps");
  });

  it("omits the sweep count entirely when DaemonStatus could not be queried", () => {
    const result = classifyHeartbeat({ alive: true, inFlightSweeps: null });
    expect(result.status).toBe("daemon_healthy");
    expect(result.message).not.toContain("sweep");
    expect(result.daemon.inFlightSweeps).toBeNull();
  });

  it("reports daemon_unreachable when the daemon does not respond", () => {
    const result = classifyHeartbeat({ alive: false, inFlightSweeps: null });

    expect(result.status).toBe("daemon_unreachable");
    expect(result.message).toContain("did not respond");
    expect(result.daemon).toEqual({ alive: false, inFlightSweeps: null });
    expect(result).not.toHaveProperty("desktopApp");
  });
});

describe("buildUIState (daemon-only since #13821)", () => {
  const config = { version: "1.0", terminals: [{ id: "terminal-1" }], offlineMode: true };

  it("reports the engine running when the daemon answers", () => {
    const state = buildUIState("/repo", config, { alive: true, inFlightSweeps: 3 });

    // The regression this replaces: liveness came from a `state.json` with no
    // writer since #3353, so `isRunning` was false even with a live daemon.
    expect(state.engine).toEqual({ isRunning: true, inFlightSweeps: 3 });
  });

  it("reports the engine stopped only when the daemon does not answer", () => {
    const state = buildUIState("/repo", config, { alive: false, inFlightSweeps: null });

    expect(state.engine).toEqual({ isRunning: false, inFlightSweeps: null });
  });

  it("no longer exposes the dead state-file blocks", () => {
    const state = buildUIState("/repo", config, { alive: true, inFlightSweeps: null });

    expect(state).not.toHaveProperty("state");
    expect(state.workspace).not.toHaveProperty("hasState");
    expect(state.engine).not.toHaveProperty("daemonPid");
    expect(state.engine).not.toHaveProperty("terminalCount");
  });

  it("still reports the workspace and the effective tiered config", () => {
    const state = buildUIState("/repo", config, { alive: true, inFlightSweeps: 0 });

    expect(state.workspace).toEqual({ path: "/repo", hasConfig: true });
    expect(state.config).toEqual({
      version: "1.0",
      terminalCount: 1,
      offlineMode: true,
      terminals: config.terminals,
    });
  });

  it("reports hasConfig false and a null config for an uninitialized workspace", () => {
    const state = buildUIState("/repo", null, { alive: true, inFlightSweeps: null });

    expect(state.workspace.hasConfig).toBe(false);
    expect(state.config).toBeNull();
  });
});
