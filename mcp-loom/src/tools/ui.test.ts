/**
 * Tests for `classifyHeartbeat` (Issue #4794).
 *
 * Before this, `get_heartbeat` derived its entire status from the retired
 * Electron desktop app's console log, so a perfectly healthy `loom-daemon`
 * with a stale/absent desktop log was reported as `status: "stale"` — the
 * wrong answer for "is the fleet running?" (observed: "Last log entry was
 * 284857m ago" against a host with 8 in-flight sweeps under a healthy
 * daemon). These tests pin the fixed behavior: a live daemon always wins.
 */

import { describe, expect, it } from "vitest";
import { classifyHeartbeat, type DesktopLogStatus } from "./ui.js";

const STALE_DESKTOP_LOG: DesktopLogStatus = {
  status: "stale",
  message: "Last log entry was 284857m ago",
  lastLogTime: "2026-01-14T00:00:00.000Z",
  logCount: 42,
  recentLogs: ["Found browse button element"],
};

const ABSENT_DESKTOP_LOG: DesktopLogStatus = {
  status: "not_running",
  message: "Console log file not found - app is not running or console logging is disabled",
  lastLogTime: null,
  logCount: 0,
};

const HEALTHY_DESKTOP_LOG: DesktopLogStatus = {
  status: "healthy",
  message: "Last log entry was 3s ago",
  lastLogTime: "2026-07-31T00:00:00.000Z",
  logCount: 10,
  recentLogs: ["ready"],
};

describe("classifyHeartbeat", () => {
  it("reports daemon_healthy for a live daemon even when the desktop log is stale", () => {
    const result = classifyHeartbeat({ alive: true, inFlightSweeps: 8 }, STALE_DESKTOP_LOG);

    expect(result.status).toBe("daemon_healthy");
    expect(result.status).not.toBe("stale");
    expect(result.daemon).toEqual({ alive: true, inFlightSweeps: 8 });
    expect(result.message).toContain("8 sweeps in flight");
    // The legacy desktop signal must still be visible, just not authoritative.
    expect(result.desktopApp).toEqual(STALE_DESKTOP_LOG);
  });

  it("reports daemon_healthy for a live daemon even when the desktop log is entirely absent", () => {
    const result = classifyHeartbeat({ alive: true, inFlightSweeps: 0 }, ABSENT_DESKTOP_LOG);

    expect(result.status).toBe("daemon_healthy");
    expect(result.status).not.toBe("not_running");
    expect(result.message).toContain("loom-daemon is responding on its Unix socket");
  });

  it("uses singular 'sweep' wording for exactly one in-flight sweep", () => {
    const result = classifyHeartbeat({ alive: true, inFlightSweeps: 1 }, ABSENT_DESKTOP_LOG);
    expect(result.message).toContain("1 sweep in flight");
    expect(result.message).not.toContain("1 sweeps");
  });

  it("omits the sweep count entirely when DaemonStatus could not be queried", () => {
    const result = classifyHeartbeat({ alive: true, inFlightSweeps: null }, ABSENT_DESKTOP_LOG);
    expect(result.status).toBe("daemon_healthy");
    expect(result.message).not.toContain("sweep");
    expect(result.daemon.inFlightSweeps).toBeNull();
  });

  it("falls back to the desktop-log status when the daemon is unreachable, but says so explicitly", () => {
    const result = classifyHeartbeat({ alive: false, inFlightSweeps: null }, STALE_DESKTOP_LOG);

    expect(result.status).toBe("stale");
    expect(result.message).toContain("did not respond");
    expect(result.message).toContain("not fleet health");
    expect(result.daemon).toEqual({ alive: false, inFlightSweeps: null });
  });

  it("preserves legacy top-level fields for backwards compatibility", () => {
    const result = classifyHeartbeat({ alive: true, inFlightSweeps: 2 }, HEALTHY_DESKTOP_LOG);
    expect(result.lastLogTime).toBe(HEALTHY_DESKTOP_LOG.lastLogTime);
    expect(result.logCount).toBe(HEALTHY_DESKTOP_LOG.logCount);
    expect(result.recentLogs).toEqual(HEALTHY_DESKTOP_LOG.recentLogs);
  });
});
