/**
 * Tests for the daemon status tool's relay surface (issue #9950).
 *
 * The wire frames are typed as the daemon's open response shape — the same
 * `{type, payload}` envelope `sendDaemonRequest` yields — so these fixtures
 * exercise the rendering rather than the compiler's narrowing. The relay
 * directive is the point: an agent that calls daemon_status while telemetry
 * is going wrong must get an explicit RELAY-TO-THE-USER block with the fix
 * hint, never a bare data line it can file away.
 */

import { describe, expect, it } from "vitest";
import {
  describeObservability,
  isTelemetryProblem,
  relayDirective,
  renderDaemonStatus,
  type DaemonStatusReport,
  type ObservabilityExportStatus,
} from "./status.js";

type WireFrame = { type: string; payload?: unknown };

function exportStatus(mutate: (s: ObservabilityExportStatus) => void): ObservabilityExportStatus {
  const s: ObservabilityExportStatus = {
    state: "starting",
    host_id: "joseph-air",
    ingest_host_id: null,
    endpoint: "http://127.0.0.1:14318",
    exporter: "otlp",
    started_at: "2026-10-02T06:00:00Z",
    last_success_at: null,
    last_failure_at: null,
    last_failure_detail: null,
    records_exported: 0,
    consecutive_failures: 0,
    flush_interval_secs: 30,
    endpoint_loopback: true,
  };
  mutate(s);
  return s;
}

describe("isTelemetryProblem", () => {
  it("flags exactly the states that mean telemetry is going wrong", () => {
    expect(isTelemetryProblem("failing")).toBe(true);
    expect(isTelemetryProblem("never_exported")).toBe(true);
    expect(isTelemetryProblem("misconfigured")).toBe(true);
    expect(isTelemetryProblem("host_id_mismatch")).toBe(true);
    expect(isTelemetryProblem("healthy")).toBe(false);
    expect(isTelemetryProblem("disabled")).toBe(false);
    expect(isTelemetryProblem("starting")).toBe(false);
    expect(isTelemetryProblem("unrecognized")).toBe(false);
  });
});

describe("describeObservability", () => {
  it("names the endpoint and what it means when failing", () => {
    const line = describeObservability(exportStatus((s) => {
      s.state = "failing";
      s.consecutive_failures = 12;
      s.last_success_at = "2026-10-02T04:00:00Z";
      s.last_failure_detail = "connection refused";
    }), new Date("2026-10-02T06:00:00Z"));
    expect(line).toContain("FAILING");
    expect(line).toContain("12 consecutive failed flush(es)");
    expect(line).toContain("connection refused");
    expect(line).toContain("host_id=joseph-air");
  });

  it("says NEVER EXPORTED plainly", () => {
    const line = describeObservability(exportStatus((s) => { s.state = "never_exported"; }));
    expect(line).toContain("NEVER EXPORTED");
    expect(line).toContain("telemetry is not reaching");
  });

  it("a healthy line states its first-hop scope", () => {
    const line = describeObservability(exportStatus((s) => {
      s.state = "healthy";
      s.last_success_at = "2026-10-02T05:59:48Z";
      s.records_exported = 311;
    }), new Date("2026-10-02T06:00:00Z"));
    expect(line).toContain("OK (first hop only)");
    expect(line).toContain("311 record(s)");
    expect(line).toContain("LOCAL collector");
  });
});

describe("relayDirective", () => {
  it("tells the agent to relay a failing exporter to the user, with the fix", () => {
    const d = relayDirective(exportStatus((s) => { s.state = "failing"; }));
    expect(d).toContain("RELAY TO THE USER");
    expect(d).toContain("FAILING");
    expect(d).toContain("edge collector / tunnel / ingest key");
    expect(d).toContain("loom-daemon health");
    expect(d).toContain("Tell the user now");
  });

  it("carries the fix for never_exported too", () => {
    const d = relayDirective(exportStatus((s) => { s.state = "never_exported"; }));
    expect(d).toContain("RELAY TO THE USER");
    expect(d).toContain("loom-daemon health");
  });

  it("the host-id mismatch names both identities", () => {
    const d = relayDirective(exportStatus((s) => { s.state = "healthy"; }),
      { daemon_host_id: "joseph-air", ingest_host_id: "loom-worker-1", first_seen_at: "2026-10-02T00:00:00Z" });
    expect(d).toContain("RELAY TO THE USER");
    expect(d).toContain("loom-worker-1");
    expect(d).toContain("joseph-air");
    expect(d).toContain("LOOM_HOST_ID");
  });

  it("healthy and disabled stay quiet — no relay pressure", () => {
    expect(relayDirective(exportStatus((s) => { s.state = "healthy"; }))).toBe("");
    expect(relayDirective(exportStatus((s) => { s.state = "disabled"; }))).toBe("");
    expect(relayDirective(exportStatus((s) => { s.state = "starting"; }))).toBe("");
  });
});

describe("renderDaemonStatus", () => {
  const wire = (payload: DaemonStatusReport): WireFrame => ({ type: "DaemonStatus", payload });

  it("renders the observability line and carries the relay block on a problem", () => {
    const report: DaemonStatusReport = {
      daemon_build_commit: "8703b78",
      in_flight: [{}, {}],
      observability_export: exportStatus((s) => {
        s.state = "never_exported";
      }),
      observability_host_id_mismatch: null,
    };
    const out = renderDaemonStatus(wire(report).payload as DaemonStatusReport);
    expect(out).toContain("Loom Daemon Status");
    expect(out).toContain("build: 8703b78");
    expect(out).toContain("in-flight sweeps: 2");
    expect(out).toContain("NEVER EXPORTED");
    expect(out).toContain("RELAY TO THE USER");
  });

  it("a healthy daemon renders OK without a relay block", () => {
    const report: DaemonStatusReport = {
      daemon_build_commit: "8703b78",
      in_flight: [],
      observability_export: exportStatus((s) => {
        s.state = "healthy";
        s.last_success_at = "2026-10-02T05:59:48Z";
        s.records_exported = 311;
      }),
      observability_host_id_mismatch: null,
    };
    const out = renderDaemonStatus(report);
    expect(out).toContain("OK (first hop only)");
    expect(out).not.toContain("RELAY");
  });

  it("a pre-surface daemon payload reads as unknown, never as healthy", () => {
    const report: DaemonStatusReport = {
      daemon_build_commit: null,
      in_flight: [],
      observability_export: null,
      observability_host_id_mismatch: null,
    };
    const out = renderDaemonStatus(report);
    expect(out).toContain("Observability: unknown");
    expect(out).not.toContain("OK");
  });
});
