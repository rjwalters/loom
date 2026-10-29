/**
 * Tests for `classifyHeartbeat` (Issue #4794, daemon-only since #13198).
 *
 * `get_heartbeat` reports loom-daemon liveness alone; the retired desktop
 * app's console log is no longer consulted.
 */

import { describe, expect, it } from "vitest";
import { classifyHeartbeat } from "./ui.js";

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
