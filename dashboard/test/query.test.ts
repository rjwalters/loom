import { createExecutionContext, env, waitOnExecutionContext } from "cloudflare:test";
import { beforeAll, beforeEach, describe, expect, it } from "vitest";
import worker from "../src/index";
import {
  authedRequest,
  ephemeralComputeEnvelope,
  initAccessTestKeys,
  mockJwksFetch,
  seedHost,
  sweepStartedEnvelope,
} from "./testHelpers";

async function callWorker(request: Request): Promise<Response> {
  const ctx = createExecutionContext();
  const response = await worker.fetch(request as Request<unknown, IncomingRequestCfProperties>, env, ctx);
  await waitOnExecutionContext(ctx);
  return response;
}

function ingestRequest(body: unknown, authHeader: string): Request {
  return new Request("https://ingest.example/ingest", {
    method: "POST",
    headers: { "content-type": "application/json", authorization: authHeader },
    body: JSON.stringify(body),
  });
}

async function ingest(envelopes: unknown[], authHeader = "Bearer abc-ingest-key"): Promise<Response> {
  return callWorker(ingestRequest(envelopes, authHeader));
}

function outcomeEnvelope(overrides: Partial<Record<string, unknown>> = {}): Record<string, unknown> {
  return {
    schema_version: 1,
    emitted_at: "2026-07-30T12:05:00Z",
    host_id: "host-abc",
    record: {
      kind: "sweep.outcome",
      repo: "rjwalters/loom",
      visibility: "public",
      issue: 4703,
      sweep_id: "sweep-issue-4703-0",
      model: "opus",
      result: "success",
      total_duration_sec: 512,
      ...overrides,
    },
  };
}

beforeAll(async () => {
  // `/api/*` verifies the Access JWT in-Worker (src/index.ts), so this suite
  // needs a real signed cookie and a stubbed JWKS endpoint. Installed once —
  // nothing here tests the failure path, which is index.test.ts's job.
  await initAccessTestKeys();
  mockJwksFetch();
});

beforeEach(async () => {
  await seedHost(env.DB, "host-abc", "abc-ingest-key");
});

describe("GET /api/fleet-state", () => {
  it("returns the fleet snapshot with no admin token required", async () => {
    await ingest([sweepStartedEnvelope()]);

    const response = await callWorker(await authedRequest("https://ingest.example/api/fleet-state"));
    expect(response.status).toBe(200);
    const body = (await response.json()) as {
      hosts: Record<string, unknown>;
      activeSweeps: { sweepId: string; hostId: string }[];
    };
    expect(body.activeSweeps).toHaveLength(1);
    expect(body.activeSweeps[0]).toMatchObject({ sweepId: "sweep-issue-4703-0", hostId: "host-abc" });
  });
});

describe("GET /api/history — filtering", () => {
  it("filters by host", async () => {
    await seedHost(env.DB, "host-xyz", "xyz-ingest-key");
    await ingest([sweepStartedEnvelope({ sweep_id: "sweep-abc" })], "Bearer abc-ingest-key");
    await ingest(
      [{ ...sweepStartedEnvelope({ sweep_id: "sweep-xyz" }), host_id: "host-xyz" }],
      "Bearer xyz-ingest-key",
    );

    const response = await callWorker(await authedRequest("https://ingest.example/api/history?host=host-abc"));
    expect(response.status).toBe(200);
    const body = (await response.json()) as { records: { hostId: string }[] };
    expect(body.records).toHaveLength(1);
    expect(body.records[0]?.hostId).toBe("host-abc");
  });

  it("filters by repo", async () => {
    await ingest([
      sweepStartedEnvelope({ sweep_id: "sweep-loom", repo: "rjwalters/loom" }),
      sweepStartedEnvelope({ sweep_id: "sweep-anvil", repo: "rjwalters/anvil" }),
    ]);

    const response = await callWorker(
      await authedRequest("https://ingest.example/api/history?repo=rjwalters/anvil"),
    );
    const body = (await response.json()) as { records: { repo: string | null }[] };
    expect(body.records).toHaveLength(1);
    expect(body.records[0]?.repo).toBe("rjwalters/anvil");
  });

  it("filters by kind", async () => {
    await ingest([
      sweepStartedEnvelope({ sweep_id: "sweep-started" }),
      outcomeEnvelope({ sweep_id: "sweep-outcome" }),
    ]);

    const response = await callWorker(
      await authedRequest("https://ingest.example/api/history?kind=sweep.outcome"),
    );
    expect(response.status).toBe(200);
    const body = (await response.json()) as { records: { kind: string }[] };
    expect(body.records).toHaveLength(1);
    expect(body.records[0]?.kind).toBe("sweep.outcome");
  });

  it("filters by model (extracted from the payload JSON)", async () => {
    await ingest([
      outcomeEnvelope({ sweep_id: "sweep-opus", model: "opus" }),
      outcomeEnvelope({ sweep_id: "sweep-sonnet", model: "sonnet" }),
    ]);

    const response = await callWorker(await authedRequest("https://ingest.example/api/history?model=sonnet"));
    const body = (await response.json()) as { records: { record: { model?: string } }[] };
    expect(body.records).toHaveLength(1);
    expect(body.records[0]?.record.model).toBe("sonnet");
  });

  it("filters by result (extracted from the payload JSON)", async () => {
    await ingest([
      outcomeEnvelope({ sweep_id: "sweep-ok", result: "success" }),
      outcomeEnvelope({ sweep_id: "sweep-bad", result: "failure" }),
    ]);

    const response = await callWorker(await authedRequest("https://ingest.example/api/history?result=failure"));
    const body = (await response.json()) as { records: { record: { result?: string } }[] };
    expect(body.records).toHaveLength(1);
    expect(body.records[0]?.record.result).toBe("failure");
  });

  it("filters by a since/until time range over emitted_at", async () => {
    await ingest([
      sweepStartedEnvelope({ sweep_id: "sweep-early" }), // emitted_at 2026-07-30T12:00:00Z (fixture default)
    ]);
    await ingest([
      { ...outcomeEnvelope({ sweep_id: "sweep-late" }), emitted_at: "2026-08-01T00:00:00Z" },
    ]);

    const response = await callWorker(
      await authedRequest("https://ingest.example/api/history?since=2026-07-31T00:00:00Z"),
    );
    const body = (await response.json()) as { records: { sweepId: string | null }[] };
    expect(body.records).toHaveLength(1);
    expect(body.records[0]?.sweepId).toBe("sweep-late");

    const untilResponse = await callWorker(
      await authedRequest("https://ingest.example/api/history?until=2026-07-31T00:00:00Z"),
    );
    const untilBody = (await untilResponse.json()) as { records: { sweepId: string | null }[] };
    expect(untilBody.records).toHaveLength(1);
    expect(untilBody.records[0]?.sweepId).toBe("sweep-early");
  });

  it("rejects an unparseable since/until/limit/cursor with 400", async () => {
    const badSince = await callWorker(await authedRequest("https://ingest.example/api/history?since=not-a-date"));
    expect(badSince.status).toBe(400);

    const badLimit = await callWorker(await authedRequest("https://ingest.example/api/history?limit=0"));
    expect(badLimit.status).toBe(400);

    const badCursor = await callWorker(await authedRequest("https://ingest.example/api/history?cursor=abc"));
    expect(badCursor.status).toBe(400);
  });
});

describe("GET /api/history — pagination", () => {
  it("paginates newest-first with a stable keyset cursor", async () => {
    // Five distinct records, ingested in one batch so ordering by `id` is
    // deterministic (insertion order within the batch).
    const envelopes = Array.from({ length: 5 }, (_, i) =>
      sweepStartedEnvelope({ sweep_id: `sweep-${i}` }),
    );
    await ingest(envelopes);

    const firstPage = await callWorker(await authedRequest("https://ingest.example/api/history?limit=2"));
    const firstBody = (await firstPage.json()) as {
      records: { sweepId: string | null }[];
      nextCursor: number | null;
    };
    expect(firstBody.records).toHaveLength(2);
    expect(firstBody.nextCursor).not.toBeNull();
    // Newest-first: the last-ingested envelope (sweep-4) comes first.
    expect(firstBody.records[0]?.sweepId).toBe("sweep-4");

    const secondPage = await callWorker(
      await authedRequest(`https://ingest.example/api/history?limit=2&cursor=${firstBody.nextCursor}`),
    );
    const secondBody = (await secondPage.json()) as {
      records: { sweepId: string | null }[];
      nextCursor: number | null;
    };
    expect(secondBody.records).toHaveLength(2);
    expect(secondBody.records.map((r) => r.sweepId)).toEqual(["sweep-2", "sweep-1"]);

    const thirdPage = await callWorker(
      await authedRequest(`https://ingest.example/api/history?limit=2&cursor=${secondBody.nextCursor}`),
    );
    const thirdBody = (await thirdPage.json()) as {
      records: { sweepId: string | null }[];
      nextCursor: number | null;
    };
    expect(thirdBody.records).toHaveLength(1);
    expect(thirdBody.records[0]?.sweepId).toBe("sweep-0");
    expect(thirdBody.nextCursor).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// Elastic compute spend (Issue #8306, Phase 3 of #8257)
// ---------------------------------------------------------------------------

/** One completed `ephemeral_compute` job, emitted at `emittedAt` (which is
 * also what the aggregation buckets by). */
function completedJob(
  jobId: string,
  emittedAt: string,
  costUsd: number,
  extra: Partial<Record<string, unknown>> = {},
): Record<string, unknown> {
  return {
    ...ephemeralComputeEnvelope({ job_id: jobId, estimated_cost_usd: costUsd, ...extra }),
    emitted_at: emittedAt,
  };
}

interface SpendBody {
  since: string | null;
  until: string | null;
  totalCostUsd?: number;
  jobCount?: number;
  totalWallClockSec?: number | null;
  peakDailyCostUsd?: number | null;
  days?: { day: string; costUsd: number; jobCount: number }[];
  withheld?: boolean;
}

async function getSpend(query = ""): Promise<SpendBody> {
  const response = await callWorker(await authedRequest(`https://ingest.example/api/spend${query}`));
  expect(response.status).toBe(200);
  return (await response.json()) as SpendBody;
}

describe("GET /api/spend — elastic compute spend", () => {
  it("sums estimated_cost_usd across the window and buckets it by UTC day", async () => {
    await ingest([
      completedJob("job-1", "2026-09-17T01:00:00Z", 1.5),
      completedJob("job-2", "2026-09-17T23:00:00Z", 2.25),
      completedJob("job-3", "2026-09-18T12:00:00Z", 10),
    ]);

    const body = await getSpend();
    expect(body.totalCostUsd).toBe(13.75);
    expect(body.jobCount).toBe(3);
    expect(body.days).toEqual([
      { day: "2026-09-17", costUsd: 3.75, jobCount: 2 },
      { day: "2026-09-18", costUsd: 10, jobCount: 1 },
    ]);
    // The number an operator compares against a standing daily ceiling — the
    // window total cannot answer "did any one day breach it".
    expect(body.peakDailyCostUsd).toBe(10);
    // `wall_clock_sec` defaults to 2700 in the fixture, so three jobs sum to
    // 8100 — proving the second SUM is wired to the same row set.
    expect(body.totalWallClockSec).toBe(8100);
  });

  it("honours the since/until window bounds", async () => {
    await ingest([
      completedJob("job-old", "2026-09-10T00:00:00Z", 100),
      completedJob("job-in", "2026-09-17T00:00:00Z", 7),
      completedJob("job-new", "2026-09-25T00:00:00Z", 100),
    ]);

    const body = await getSpend("?since=2026-09-15T00:00:00Z&until=2026-09-20T00:00:00Z");
    expect(body.totalCostUsd).toBe(7);
    expect(body.jobCount).toBe(1);
    expect(body.days).toEqual([{ day: "2026-09-17", costUsd: 7, jobCount: 1 }]);
    // The requested window is echoed back so a renderer can label the period
    // without re-deriving it.
    expect(body.since).toBe("2026-09-15T00:00:00Z");
    expect(body.until).toBe("2026-09-20T00:00:00Z");
  });

  it("counts a job once — a launch record carries no cost and is excluded", async () => {
    await ingest([
      // The launch half of the two-record lifecycle: no cost, no wall clock.
      ephemeralComputeEnvelope({
        job_id: "job-running",
        ended_at: undefined,
        wall_clock_sec: undefined,
        estimated_cost_usd: undefined,
      }),
      completedJob("job-done", "2026-09-19T06:00:00Z", 4),
    ]);

    const body = await getSpend();
    expect(body.jobCount).toBe(1);
    expect(body.totalCostUsd).toBe(4);
  });

  it("ignores a non-numeric estimated_cost_usd rather than coercing it to 0", async () => {
    await ingest([
      completedJob("job-good", "2026-09-19T06:00:00Z", 4),
      completedJob("job-bad", "2026-09-19T07:00:00Z", 0, { estimated_cost_usd: "1.23" }),
    ]);

    const body = await getSpend();
    // The malformed row is excluded from BOTH the sum and the count — a
    // silent `SUM` coercion would have added a phantom $0.00 job instead.
    expect(body.jobCount).toBe(1);
    expect(body.totalCostUsd).toBe(4);
  });

  it("reports wall clock as null, not 0, when no job in the window carried one", async () => {
    await ingest([completedJob("job-nowall", "2026-09-19T06:00:00Z", 3, { wall_clock_sec: undefined })]);

    const body = await getSpend();
    expect(body.totalCostUsd).toBe(3);
    expect(body.totalWallClockSec).toBeNull();
  });

  it("returns an empty $0 summary — not an error — for a window with no jobs", async () => {
    const body = await getSpend("?since=2030-01-01T00:00:00Z&until=2030-01-08T00:00:00Z");
    expect(body.totalCostUsd).toBe(0);
    expect(body.jobCount).toBe(0);
    expect(body.days).toEqual([]);
    // Unknown is not zero: no spend means no peak day, not a $0 peak day.
    expect(body.peakDailyCostUsd).toBeNull();
    expect(body.totalWallClockSec).toBeNull();
  });

  it("filters by the emitting host", async () => {
    await seedHost(env.DB, "host-xyz", "xyz-ingest-key");
    await ingest([completedJob("job-abc", "2026-09-19T06:00:00Z", 5)], "Bearer abc-ingest-key");
    await ingest(
      [{ ...completedJob("job-xyz", "2026-09-19T06:00:00Z", 50), host_id: "host-xyz" }],
      "Bearer xyz-ingest-key",
    );

    const body = await getSpend("?host=host-abc");
    expect(body.totalCostUsd).toBe(5);
    expect(body.jobCount).toBe(1);
  });

  it("rejects a malformed since/until with 400, matching /api/history", async () => {
    const response = await callWorker(await authedRequest("https://ingest.example/api/spend?since=yesterday"));
    expect(response.status).toBe(400);
    expect(await response.json()).toMatchObject({ error: "since must be an RFC 3339 datetime" });
  });

  it("requires authentication", async () => {
    const response = await callWorker(new Request("https://ingest.example/api/spend"));
    expect(response.status).toBe(401);
  });
});

describe("GET /public/spend — redaction", () => {
  it("withholds every figure rather than returning a zeroed summary", async () => {
    await ingest([completedJob("job-secret", "2026-09-19T06:00:00Z", 42.5)]);

    const response = await callWorker(
      new Request("https://ingest.example/public/spend?since=2026-09-01T00:00:00Z"),
    );
    expect(response.status).toBe(200);
    const body = (await response.json()) as SpendBody;

    // `withheld: true` and NOT a `$0.00` summary — a zero would read as a real
    // idle window (see src/redaction.ts's WithheldElasticSpend doc).
    expect(body.withheld).toBe(true);
    expect(body).not.toHaveProperty("totalCostUsd");
    expect(body).not.toHaveProperty("jobCount");
    expect(body).not.toHaveProperty("days");
    expect(body).not.toHaveProperty("peakDailyCostUsd");
    // The requester's own query parameter comes back; nothing is revealed by
    // repeating what the request already stated.
    expect(body.since).toBe("2026-09-01T00:00:00Z");
    // Belt and braces: no cost figure reaches the wire by any path.
    expect(JSON.stringify(body)).not.toContain("42.5");
  });

  it("still validates its query params", async () => {
    const response = await callWorker(new Request("https://ingest.example/public/spend?until=soon"));
    expect(response.status).toBe(400);
  });
});

describe("GET /api/events — live tail", () => {
  it("streams a newly-ingested record to a connected client", async () => {
    // The live-tail poll loop runs at its production default cadence
    // (`LIVE_TAIL_DEFAULT_POLL_INTERVAL_MS`, 1s) — this test's own timeout
    // below gives it several polls' worth of headroom.
    const response = await callWorker(
      await authedRequest("https://ingest.example/api/events", {
        headers: { accept: "text/event-stream" },
      }),
    );
    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toBe("text/event-stream");

    const reader = response.body?.getReader();
    expect(reader).toBeDefined();
    if (!reader) throw new Error("expected a readable stream body");

    const decoder = new TextDecoder();
    let buffer = "";

    // Drain the initial `retry:`/comment preamble.
    const preamble = await reader.read();
    if (preamble.value) buffer += decoder.decode(preamble.value);
    expect(buffer).toContain("retry:");

    // Ingest a fresh record now that the stream is open.
    await ingest([sweepStartedEnvelope({ sweep_id: "sweep-live-tail" })]);

    // Poll the stream, reading chunks as they arrive, until the frame for
    // our new record shows up.
    const deadline = Date.now() + 8_000;
    while (!buffer.includes("sweep-live-tail") && Date.now() < deadline) {
      const chunk = await reader.read();
      if (chunk.done) break;
      if (chunk.value) buffer += decoder.decode(chunk.value);
    }

    expect(buffer).toContain("sweep-live-tail");
    const dataLine = buffer.split("\n\n").find((frame) => frame.includes("sweep-live-tail"));
    expect(dataLine).toBeDefined();
    const json = JSON.parse((dataLine ?? "").replace(/^data: /, "")) as {
      topic: string;
      event: { hostId: string; record: { sweep_id?: string } };
    };
    expect(json.topic).toBe("sweep.started");
    expect(json.event.hostId).toBe("host-abc");
    expect(json.event.record.sweep_id).toBe("sweep-live-tail");

    await reader.cancel();
  }, 15_000);

  it("filters the live tail by host", async () => {
    await seedHost(env.DB, "host-xyz", "xyz-ingest-key");

    const response = await callWorker(await authedRequest("https://ingest.example/api/events?host=host-xyz"));
    const reader = response.body?.getReader();
    if (!reader) throw new Error("expected a readable stream body");
    const decoder = new TextDecoder();
    let buffer = "";
    buffer += decoder.decode((await reader.read()).value);

    // A record from host-abc must NOT show up on a host-xyz-filtered tail.
    await ingest([sweepStartedEnvelope({ sweep_id: "sweep-wrong-host" })], "Bearer abc-ingest-key");
    // A record from host-xyz must show up.
    await ingest(
      [{ ...sweepStartedEnvelope({ sweep_id: "sweep-right-host" }), host_id: "host-xyz" }],
      "Bearer xyz-ingest-key",
    );

    const deadline = Date.now() + 8_000;
    while (!buffer.includes("sweep-right-host") && Date.now() < deadline) {
      const chunk = await reader.read();
      if (chunk.done) break;
      if (chunk.value) buffer += decoder.decode(chunk.value);
    }

    expect(buffer).toContain("sweep-right-host");
    expect(buffer).not.toContain("sweep-wrong-host");

    await reader.cancel();
  }, 15_000);
});
