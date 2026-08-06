import { createExecutionContext, env, waitOnExecutionContext } from "cloudflare:test";
import { beforeAll, beforeEach, describe, expect, it } from "vitest";
import worker from "../src/index";
import {
  authedRequest,
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
