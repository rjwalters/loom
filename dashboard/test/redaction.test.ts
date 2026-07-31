import { createExecutionContext, env, waitOnExecutionContext } from "cloudflare:test";
import { beforeEach, describe, expect, it } from "vitest";
import worker from "../src/index";
import {
  redactActiveSweep,
  redactFleetSnapshot,
  redactHistoryQueryResult,
  redactHistoryRecord,
  redactPayload,
  redactSseFrame,
} from "../src/redaction";
import type { HistoryRecord } from "../src/query";
import type { ActiveSweepState, FleetSnapshot } from "../src/fleetState";
import {
  hostHealthEnvelope,
  seedHost,
  sweepCompletedEnvelope,
  sweepOutcomeEnvelope,
  sweepPhaseEnvelope,
  sweepStartedEnvelope,
  tokensSnapshotEnvelope,
} from "./testHelpers";

// ---------------------------------------------------------------------------
// Adversarial redaction test suite (issue #4727).
//
// Covers every record kind from the Phase-1 schema
// (`.loom/docs/telemetry-schema.md`) across visibility × auth, asserting
// field-level absence (not just "doesn't crash") of the four leak vectors
// the acceptance criteria name: repo names, issue numbers, sweep/branch
// identifiers, and PR links (`pr_number`).
// ---------------------------------------------------------------------------

async function callWorker(request: Request): Promise<Response> {
  const ctx = createExecutionContext();
  const response = await worker.fetch(request as Request<unknown, IncomingRequestCfProperties>, env, ctx);
  await waitOnExecutionContext(ctx);
  return response;
}

function ingestRequest(body: unknown, authHeader = "Bearer abc-ingest-key"): Request {
  return new Request("https://ingest.example/ingest", {
    method: "POST",
    headers: { "content-type": "application/json", authorization: authHeader },
    body: JSON.stringify(body),
  });
}

async function ingest(envelopes: unknown[]): Promise<Response> {
  return callWorker(ingestRequest(envelopes));
}

beforeEach(async () => {
  await seedHost(env.DB, "host-abc", "abc-ingest-key");
});

// ---------------------------------------------------------------------------
// Unit tests: `redactPayload` (the per-kind field allowlist), one fixture
// per record kind the schema defines. Each fixture is private-visibility
// (or, for the host-level kinds, has no visibility at all) with every field
// the schema documents, plus adversarial extras (`branch`, `issue_title`)
// that are NOT part of today's schema — the allowlist must drop unknown
// fields by default, exactly the "future field leaks by accident" scenario
// the module doc calls out.
// ---------------------------------------------------------------------------

describe("redactPayload — per-kind field allowlist", () => {
  it("sweep.started: keeps lifecycle/model fields, strips repo/issue/sweep_id and any unknown field", () => {
    const redacted = redactPayload("sweep.started", {
      kind: "sweep.started",
      repo: "rjwalters/loom",
      visibility: "private",
      issue: 4703,
      sweep_id: "sweep-issue-4703-0",
      started_at: "2026-07-30T12:00:00Z",
      model: "opus",
      effort: "high",
      branch: "feature/issue-4703",
      issue_title: "Fix the thing",
    });
    expect(redacted).toEqual({
      kind: "sweep.started",
      started_at: "2026-07-30T12:00:00Z",
      model: "opus",
      effort: "high",
    });
  });

  it("sweep.phase: keeps phase/entered_at, strips repo/issue/sweep_id", () => {
    const redacted = redactPayload("sweep.phase", {
      kind: "sweep.phase",
      repo: "rjwalters/loom",
      visibility: "private",
      issue: 4703,
      sweep_id: "sweep-issue-4703-0",
      phase: "builder",
      entered_at: "2026-07-30T12:03:20Z",
      branch: "feature/issue-4703",
    });
    expect(redacted).toEqual({ kind: "sweep.phase", phase: "builder", entered_at: "2026-07-30T12:03:20Z" });
  });

  it("sweep.completed: keeps completed_at/result, strips repo/issue/sweep_id", () => {
    const redacted = redactPayload("sweep.completed", {
      kind: "sweep.completed",
      repo: "rjwalters/loom",
      visibility: "private",
      issue: 4703,
      sweep_id: "sweep-issue-4703-0",
      completed_at: "2026-07-30T12:08:32Z",
      result: "success",
    });
    expect(redacted).toEqual({ kind: "sweep.completed", completed_at: "2026-07-30T12:08:32Z", result: "success" });
  });

  it("sweep.outcome: keeps model/config/phase_durations/result, strips repo/issue/sweep_id/pr_number (the PR link vector)", () => {
    const redacted = redactPayload("sweep.outcome", {
      kind: "sweep.outcome",
      repo: "rjwalters/loom",
      visibility: "private",
      issue: 4703,
      sweep_id: "sweep-issue-4703-0",
      model: "opus",
      effort: "high",
      config: { runtime: "claude" },
      phase_durations: [{ phase: "builder", duration_sec: 340 }],
      total_duration_sec: 512,
      result: "success",
      pr_number: 4710,
    });
    expect(redacted).toEqual({
      kind: "sweep.outcome",
      model: "opus",
      effort: "high",
      config: { runtime: "claude" },
      phase_durations: [{ phase: "builder", duration_sec: 340 }],
      total_duration_sec: 512,
      result: "success",
    });
    expect(redacted).not.toHaveProperty("pr_number");
  });

  it("tokens.snapshot: host-level, no repo/issue reference — passes through unchanged (documented decision)", () => {
    const payload = {
      kind: "tokens.snapshot",
      captured_at: "2026-07-30T12:00:00Z",
      accounts: [{ account: "agent-1", rank: 0, usage_fraction: 0.42, exhausted: false }],
    };
    expect(redactPayload("tokens.snapshot", payload)).toEqual(payload);
  });

  it("host.health: host-level, no repo/issue reference — passes through unchanged (documented decision)", () => {
    const payload = {
      kind: "host.health",
      captured_at: "2026-07-30T12:00:00Z",
      daemon_version: "0.16.0",
      uptime_sec: 86400,
      logical_cpus: 28,
      cpu_idle_fraction: 0.83,
    };
    expect(redactPayload("host.health", payload)).toEqual(payload);
  });

  it("an unrecognized (forward-compatible) kind reveals only `kind`", () => {
    const redacted = redactPayload("future.kind", {
      kind: "future.kind",
      repo: "rjwalters/loom",
      some_new_field: "unexpected",
    });
    expect(redacted).toEqual({ kind: "future.kind" });
  });
});

// ---------------------------------------------------------------------------
// Unit tests: `redactHistoryRecord` / `redactHistoryQueryResult`
// ---------------------------------------------------------------------------

function historyRecordFixture(overrides: Partial<HistoryRecord> = {}): HistoryRecord {
  return {
    id: 1,
    schemaVersion: 1,
    emittedAt: "2026-07-30T12:00:00Z",
    hostId: "host-abc",
    kind: "sweep.started",
    repo: "rjwalters/loom",
    visibility: "private",
    issue: 4703,
    sweepId: "sweep-issue-4703-0",
    ingestedAt: "2026-07-30T12:00:01Z",
    record: {
      kind: "sweep.started",
      repo: "rjwalters/loom",
      visibility: "private",
      issue: 4703,
      sweep_id: "sweep-issue-4703-0",
      started_at: "2026-07-30T12:00:00Z",
      model: "opus",
    },
    ...overrides,
  };
}

describe("redactHistoryRecord", () => {
  it("authenticated viewer sees full detail regardless of visibility", () => {
    const record = historyRecordFixture();
    expect(redactHistoryRecord(record, true)).toBe(record);
  });

  it("public-visibility record is returned unchanged even without authentication", () => {
    const record = historyRecordFixture({ visibility: "public" });
    expect(redactHistoryRecord(record, false)).toBe(record);
  });

  it("private + unauthenticated: repo/issue/sweepId nulled at the top level, payload allowlisted", () => {
    const record = historyRecordFixture();
    const redacted = redactHistoryRecord(record, false);
    expect(redacted.repo).toBeNull();
    expect(redacted.issue).toBeNull();
    expect(redacted.sweepId).toBeNull();
    expect(redacted.hostId).toBe("host-abc"); // host id is not repo-identifying
    expect(redacted.record).toEqual({ kind: "sweep.started", started_at: "2026-07-30T12:00:00Z", model: "opus" });
    // Adversarial: the serialized JSON must never contain the repo name,
    // issue number, or sweep id anywhere, not just at the fields we checked.
    const serialized = JSON.stringify(redacted);
    expect(serialized).not.toContain("rjwalters/loom");
    expect(serialized).not.toContain("4703");
    expect(serialized).not.toContain("sweep-issue-4703-0");
  });

  it("a missing/invalid visibility value fails safe to private (never accidentally decodes to public)", () => {
    const record = historyRecordFixture({ visibility: "not-a-real-value" });
    const redacted = redactHistoryRecord(record, false);
    expect(redacted.repo).toBeNull();
  });
});

describe("redactHistoryQueryResult", () => {
  it("redacts every record and preserves the pagination cursor unchanged", () => {
    const result = redactHistoryQueryResult(
      { records: [historyRecordFixture({ id: 1 }), historyRecordFixture({ id: 2, visibility: "public" })], nextCursor: 1 },
      false,
    );
    expect(result.nextCursor).toBe(1);
    expect(result.records[0]?.repo).toBeNull();
    expect(result.records[1]?.repo).toBe("rjwalters/loom"); // public record untouched
  });
});

// ---------------------------------------------------------------------------
// Unit tests: `redactActiveSweep` / `redactFleetSnapshot`
// ---------------------------------------------------------------------------

function activeSweepFixture(overrides: Partial<ActiveSweepState> = {}): ActiveSweepState {
  return {
    hostId: "host-abc",
    sweepId: "sweep-issue-4703-0",
    repo: "rjwalters/loom",
    visibility: "private",
    issue: 4703,
    phase: "builder",
    startedAt: "2026-07-30T12:00:00Z",
    enteredPhaseAt: "2026-07-30T12:03:20Z",
    model: "opus",
    effort: "high",
    updatedAt: "2026-07-30T12:03:20Z",
    ...overrides,
  };
}

describe("redactActiveSweep", () => {
  it("private + unauthenticated: sweepId/repo/issue are entirely absent (not just null)", () => {
    const redacted = redactActiveSweep(activeSweepFixture(), false);
    expect(redacted).not.toHaveProperty("sweepId");
    expect(redacted).not.toHaveProperty("repo");
    expect(redacted).not.toHaveProperty("issue");
    expect(redacted.phase).toBe("builder");
    expect(redacted.model).toBe("opus");
    const serialized = JSON.stringify(redacted);
    expect(serialized).not.toContain("rjwalters/loom");
    expect(serialized).not.toContain("4703");
    expect(serialized).not.toContain("sweep-issue-4703-0");
  });

  it("public visibility or authenticated: unchanged", () => {
    const sweep = activeSweepFixture({ visibility: "public" });
    expect(redactActiveSweep(sweep, false)).toBe(sweep);
    const privateSweep = activeSweepFixture();
    expect(redactActiveSweep(privateSweep, true)).toBe(privateSweep);
  });
});

describe("redactFleetSnapshot", () => {
  it("redacts private activeSweeps entries and leaves host health/tokens intact", () => {
    const snapshot: FleetSnapshot = {
      hosts: {
        "host-abc": {
          health: { record: { kind: "host.health", uptime_sec: 100 }, updatedAt: "2026-07-30T12:00:00Z" },
          tokens: { record: { kind: "tokens.snapshot", accounts: [] }, updatedAt: "2026-07-30T12:00:00Z" },
        },
      },
      activeSweeps: [activeSweepFixture()],
    };
    const redacted = redactFleetSnapshot(snapshot, false);
    expect(redacted.hosts["host-abc"]?.health?.record).toEqual({ kind: "host.health", uptime_sec: 100 });
    expect(redacted.activeSweeps[0]).not.toHaveProperty("sweepId");
  });
});

// ---------------------------------------------------------------------------
// Unit tests: `redactSseFrame` (live tail)
// ---------------------------------------------------------------------------

describe("redactSseFrame", () => {
  it("passes non-data frames through unchanged (retry/comment preamble, keepalive)", () => {
    const preamble = "retry: 3000\n: connected to loom fleet telemetry live tail\n\n";
    expect(redactSseFrame(preamble, false)).toBe(preamble);
    const keepalive = ": keepalive\n\n";
    expect(redactSseFrame(keepalive, false)).toBe(keepalive);
  });

  it("passes an authenticated frame through unchanged regardless of visibility", () => {
    const frame = `data: ${JSON.stringify({
      topic: "sweep.started",
      event: {
        hostId: "host-abc",
        emittedAt: "2026-07-30T12:00:00Z",
        schemaVersion: 1,
        record: { kind: "sweep.started", repo: "rjwalters/loom", visibility: "private", sweep_id: "sweep-issue-4703-0" },
      },
    })}\n\n`;
    expect(redactSseFrame(frame, true)).toBe(frame);
  });

  it("passes a public-visibility frame through unchanged", () => {
    const frame = `data: ${JSON.stringify({
      topic: "sweep.started",
      event: {
        hostId: "host-abc",
        emittedAt: "2026-07-30T12:00:00Z",
        schemaVersion: 1,
        record: { kind: "sweep.started", repo: "rjwalters/loom", visibility: "public", sweep_id: "sweep-issue-4703-0" },
      },
    })}\n\n`;
    expect(redactSseFrame(frame, false)).toBe(frame);
  });

  it("redacts a private-visibility frame's record payload, unauthenticated", () => {
    const frame = `data: ${JSON.stringify({
      topic: "sweep.started",
      event: {
        hostId: "host-abc",
        emittedAt: "2026-07-30T12:00:00Z",
        schemaVersion: 1,
        record: {
          kind: "sweep.started",
          repo: "rjwalters/loom",
          visibility: "private",
          issue: 4703,
          sweep_id: "sweep-issue-4703-0",
          started_at: "2026-07-30T12:00:00Z",
        },
      },
    })}\n\n`;
    const redacted = redactSseFrame(frame, false);
    expect(redacted).not.toContain("rjwalters/loom");
    expect(redacted).not.toContain("4703");
    expect(redacted).not.toContain("sweep-issue-4703-0");
    expect(redacted).toContain("sweep.started"); // topic + kind survive
    const parsed = JSON.parse(redacted.slice("data: ".length, -2)) as { event: { record: Record<string, unknown> } };
    expect(parsed.event.record).toEqual({ kind: "sweep.started", started_at: "2026-07-30T12:00:00Z" });
  });

  it("never throws on a malformed data frame — passes it through unchanged", () => {
    const malformed = "data: {not valid json\n\n";
    expect(redactSseFrame(malformed, false)).toBe(malformed);
  });
});

// ---------------------------------------------------------------------------
// Integration tests: the actual `/api/*` vs `/public/*` routes end to end,
// covering every record kind × private visibility, plus a spot-check that
// public-visibility data is NOT over-redacted on the public route.
// ---------------------------------------------------------------------------

const PRIVATE_ENVELOPE_BUILDERS: [string, () => Record<string, unknown>][] = [
  ["sweep.started", () => sweepStartedEnvelope({ visibility: "private" })],
  ["sweep.phase", () => sweepPhaseEnvelope({ visibility: "private" })],
  ["sweep.completed", () => sweepCompletedEnvelope({ visibility: "private" })],
  ["sweep.outcome", () => sweepOutcomeEnvelope({ visibility: "private" })],
];

describe("GET /public/history vs GET /api/history — end-to-end redaction", () => {
  for (const [kind, buildEnvelope] of PRIVATE_ENVELOPE_BUILDERS) {
    it(`${kind}: the public route never leaks repo/issue/sweep_id/pr_number; the authenticated route does`, async () => {
      await ingest([buildEnvelope()]);

      const publicResponse = await callWorker(new Request("https://ingest.example/public/history"));
      const publicText = await publicResponse.text();
      expect(publicText).not.toContain("rjwalters/loom");
      expect(publicText).not.toContain("4703");
      expect(publicText).not.toContain("sweep-issue-4703-0");
      if (kind === "sweep.outcome") {
        expect(publicText).not.toContain("4710"); // pr_number
      }

      const authResponse = await callWorker(new Request("https://ingest.example/api/history"));
      const authText = await authResponse.text();
      expect(authText).toContain("rjwalters/loom");
      expect(authText).toContain("sweep-issue-4703-0");
    });
  }

  it("tokens.snapshot and host.health (host-level, no repo/visibility) pass through unredacted on the public route", async () => {
    await ingest([tokensSnapshotEnvelope(), hostHealthEnvelope()]);

    const publicResponse = await callWorker(new Request("https://ingest.example/public/history"));
    const body = (await publicResponse.json()) as {
      records: { kind: string; record: Record<string, unknown> }[];
    };
    const tokensRecord = body.records.find((r) => r.kind === "tokens.snapshot");
    const healthRecord = body.records.find((r) => r.kind === "host.health");
    expect(tokensRecord?.record).toMatchObject({ accounts: [{ account: "agent-1" }] });
    expect(healthRecord?.record).toMatchObject({ daemon_version: "0.16.0", uptime_sec: 100 });
  });

  it("a public-visibility record is returned in full on the public route (no over-redaction)", async () => {
    await ingest([sweepStartedEnvelope({ visibility: "public" })]);

    const publicResponse = await callWorker(new Request("https://ingest.example/public/history"));
    const publicText = await publicResponse.text();
    expect(publicText).toContain("rjwalters/loom");
    expect(publicText).toContain("sweep-issue-4703-0");
  });

  it("an adversarial future field on a private record never survives to the public route", async () => {
    await ingest([
      sweepStartedEnvelope({
        visibility: "private",
        branch: "feature/issue-4703",
        issue_title: "Fix the private thing",
      }),
    ]);

    const publicResponse = await callWorker(new Request("https://ingest.example/public/history"));
    const publicText = await publicResponse.text();
    expect(publicText).not.toContain("feature/issue-4703");
    expect(publicText).not.toContain("Fix the private thing");
  });
});

describe("GET /public/fleet-state vs GET /api/fleet-state — end-to-end redaction", () => {
  it("a private in-flight sweep's repo/issue/sweepId never appear on the public route", async () => {
    await ingest([sweepStartedEnvelope({ visibility: "private" })]);

    const publicResponse = await callWorker(new Request("https://ingest.example/public/fleet-state"));
    const publicBody = (await publicResponse.json()) as { activeSweeps: { hostId: string; phase?: string }[] };
    const publicText = JSON.stringify(publicBody);
    expect(publicText).not.toContain("rjwalters/loom");
    expect(publicText).not.toContain("sweep-issue-4703-0");
    expect(publicBody.activeSweeps).toHaveLength(1);
    expect(publicBody.activeSweeps[0]?.hostId).toBe("host-abc");

    const authResponse = await callWorker(new Request("https://ingest.example/api/fleet-state"));
    const authText = await authResponse.text();
    expect(authText).toContain("rjwalters/loom");
    expect(authText).toContain("sweep-issue-4703-0");
  });
});

describe("GET /public/events vs GET /api/events — live tail redaction", () => {
  it("a private record's live-tail frame is redacted on the public route", async () => {
    const response = await callWorker(new Request("https://ingest.example/public/events"));
    const reader = response.body?.getReader();
    if (!reader) throw new Error("expected a readable stream body");
    const decoder = new TextDecoder();
    let buffer = "";
    buffer += decoder.decode((await reader.read()).value);

    await ingest([sweepStartedEnvelope({ visibility: "private", sweep_id: "sweep-live-private" })]);

    const deadline = Date.now() + 8_000;
    while (!buffer.includes("sweep.started") && Date.now() < deadline) {
      const chunk = await reader.read();
      if (chunk.done) break;
      if (chunk.value) buffer += decoder.decode(chunk.value);
    }

    expect(buffer).toContain("sweep.started");
    expect(buffer).not.toContain("rjwalters/loom");
    expect(buffer).not.toContain("sweep-live-private");
    await reader.cancel();
  }, 15_000);

  it("a public record's live-tail frame is delivered unredacted on the public route", async () => {
    const response = await callWorker(new Request("https://ingest.example/public/events"));
    const reader = response.body?.getReader();
    if (!reader) throw new Error("expected a readable stream body");
    const decoder = new TextDecoder();
    let buffer = "";
    buffer += decoder.decode((await reader.read()).value);

    await ingest([sweepStartedEnvelope({ visibility: "public", sweep_id: "sweep-live-public" })]);

    const deadline = Date.now() + 8_000;
    while (!buffer.includes("sweep-live-public") && Date.now() < deadline) {
      const chunk = await reader.read();
      if (chunk.done) break;
      if (chunk.value) buffer += decoder.decode(chunk.value);
    }

    expect(buffer).toContain("rjwalters/loom");
    expect(buffer).toContain("sweep-live-public");
    await reader.cancel();
  }, 15_000);
});
