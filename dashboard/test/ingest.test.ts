import { createExecutionContext, env, waitOnExecutionContext } from "cloudflare:test";
import { beforeEach, describe, expect, it } from "vitest";
import worker from "../src/index";
import { hostHealthEnvelope, revokeHost, seedHost, sweepStartedEnvelope } from "./testHelpers";

function ingestRequest(body: unknown, authHeader?: string): Request {
  const headers: Record<string, string> = { "content-type": "application/json" };
  if (authHeader !== undefined) headers.authorization = authHeader;
  return new Request("https://ingest.example/ingest", {
    method: "POST",
    headers,
    body: JSON.stringify(body),
  });
}

async function callWorker(request: Request): Promise<Response> {
  const ctx = createExecutionContext();
  // `new Request(...)` types as `Request<unknown, CfProperties>` (the
  // outbound-fetch shape); the Worker's `fetch` handler expects the
  // incoming-request shape `Request<unknown, IncomingRequestCfProperties>`.
  // Both are the same Fetch-API `Request` at runtime — this cast only
  // reconciles workers-types' two request-side/response-side `cf` typings.
  const response = await worker.fetch(
    request as Request<unknown, IncomingRequestCfProperties>,
    env,
    ctx,
  );
  await waitOnExecutionContext(ctx);
  return response;
}

async function recordsForHost(hostId: string): Promise<Record<string, unknown>[]> {
  const { results } = await env.DB.prepare("SELECT * FROM records WHERE host_id = ? ORDER BY id").bind(hostId).all();
  return results as Record<string, unknown>[];
}

beforeEach(async () => {
  await seedHost(env.DB, "host-abc", "abc-ingest-key");
});

describe("POST /ingest — auth", () => {
  it("rejects a request with no Authorization header", async () => {
    const response = await callWorker(ingestRequest([sweepStartedEnvelope()]));
    expect(response.status).toBe(401);
  });

  it("rejects an unknown ingest key", async () => {
    const response = await callWorker(ingestRequest([sweepStartedEnvelope()], "Bearer not-a-real-key"));
    expect(response.status).toBe(401);
  });

  it("a revoked host's key stops accepting writes while other hosts keep working", async () => {
    await seedHost(env.DB, "host-xyz", "xyz-ingest-key");
    await revokeHost(env.DB, "host-abc");

    const revokedResponse = await callWorker(ingestRequest([sweepStartedEnvelope()], "Bearer abc-ingest-key"));
    expect(revokedResponse.status).toBe(401);

    const stillWorksResponse = await callWorker(
      ingestRequest([sweepStartedEnvelope({ sweep_id: "sweep-xyz-0" })], "Bearer xyz-ingest-key"),
    );
    expect(stillWorksResponse.status).toBe(200);
    expect(await recordsForHost("host-xyz")).toHaveLength(1);
  });

  it("the persisted host_id is the authenticated identity, not the envelope's own host_id field", async () => {
    // The envelope's host_id claims a different host than the key that
    // authenticated the request — the authoritative identity must win.
    const envelope = sweepStartedEnvelope();
    (envelope as Record<string, unknown>).host_id = "some-other-claimed-host";
    const response = await callWorker(ingestRequest([envelope], "Bearer abc-ingest-key"));
    expect(response.status).toBe(200);

    const rows = await recordsForHost("host-abc");
    expect(rows).toHaveLength(1);
    expect(await recordsForHost("some-other-claimed-host")).toHaveLength(0);
  });
});

describe("POST /ingest — validation", () => {
  it("accepts a valid batch and persists every record to D1", async () => {
    const batch = [sweepStartedEnvelope(), hostHealthEnvelope()];
    const response = await callWorker(ingestRequest(batch, "Bearer abc-ingest-key"));
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ accepted: 2 });

    const rows = await recordsForHost("host-abc");
    expect(rows).toHaveLength(2);
    expect(rows.map((r) => r.kind).sort()).toEqual(["host.health", "sweep.started"]);
  });

  it("rejects a non-array body", async () => {
    const response = await callWorker(ingestRequest({ not: "an array" }, "Bearer abc-ingest-key"));
    expect(response.status).toBe(400);
  });

  it("accepts an empty array batch as a no-op", async () => {
    const response = await callWorker(ingestRequest([], "Bearer abc-ingest-key"));
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ accepted: 0 });
  });

  it("rejects the WHOLE batch when any one envelope is missing schema_version", async () => {
    const good = sweepStartedEnvelope();
    const bad = hostHealthEnvelope();
    delete (bad as Record<string, unknown>).schema_version;

    const response = await callWorker(ingestRequest([good, bad], "Bearer abc-ingest-key"));
    expect(response.status).toBe(400);
    const body = (await response.json()) as { error: string };
    expect(body.error).toMatch(/envelope 1/);

    // Whole-batch rejection: the otherwise-valid envelope must NOT have
    // been persisted either.
    expect(await recordsForHost("host-abc")).toHaveLength(0);
  });

  it("accepts a batch whose schema_version is a recognized-higher (forward-compatible) version", async () => {
    const envelope = { ...sweepStartedEnvelope(), schema_version: 2 };
    const response = await callWorker(ingestRequest([envelope], "Bearer abc-ingest-key"));
    expect(response.status).toBe(200);

    const rows = await recordsForHost("host-abc");
    expect(rows).toHaveLength(1);
    expect(rows[0]?.schema_version).toBe(2);
  });
});

describe("POST /ingest — visibility fail-safe default", () => {
  it("persists a record with missing/unknown/wrong-typed visibility as private, never public", async () => {
    const missing = sweepStartedEnvelope({ sweep_id: "sweep-missing-vis" });
    delete (missing.record as Record<string, unknown>).visibility;

    const unknown = sweepStartedEnvelope({ sweep_id: "sweep-unknown-vis", visibility: "internal-only" });
    const wrongType = sweepStartedEnvelope({ sweep_id: "sweep-wrong-type-vis", visibility: 1 });

    const response = await callWorker(
      ingestRequest([missing, unknown, wrongType], "Bearer abc-ingest-key"),
    );
    expect(response.status).toBe(200);

    const rows = await recordsForHost("host-abc");
    expect(rows).toHaveLength(3);
    for (const row of rows) {
      expect(row.visibility).toBe("private");
    }
  });

  it("persists an explicit public visibility as public", async () => {
    const response = await callWorker(
      ingestRequest([sweepStartedEnvelope({ visibility: "public" })], "Bearer abc-ingest-key"),
    );
    expect(response.status).toBe(200);
    const rows = await recordsForHost("host-abc");
    expect(rows[0]?.visibility).toBe("public");
  });
});

describe("POST /ingest — Durable Object live state", () => {
  it("updates the fleet-state snapshot for an accepted batch", async () => {
    const response = await callWorker(
      ingestRequest([sweepStartedEnvelope(), hostHealthEnvelope()], "Bearer abc-ingest-key"),
    );
    expect(response.status).toBe(200);

    const snapshotResponse = await callWorker(
      new Request("https://ingest.example/admin/fleet-state", {
        headers: { authorization: "Bearer test-admin-token" },
      }),
    );
    expect(snapshotResponse.status).toBe(200);
    const snapshot = (await snapshotResponse.json()) as {
      hosts: Record<string, { health?: unknown }>;
      activeSweeps: { sweepId: string; hostId: string }[];
    };
    expect(snapshot.hosts["host-abc"]?.health).toBeDefined();
    expect(snapshot.activeSweeps).toHaveLength(1);
    expect(snapshot.activeSweeps[0]).toMatchObject({ sweepId: "sweep-issue-4703-0", hostId: "host-abc" });
  });

  it("removes a sweep from live state once sweep.completed is ingested", async () => {
    await callWorker(ingestRequest([sweepStartedEnvelope()], "Bearer abc-ingest-key"));

    const completed = {
      schema_version: 1,
      emitted_at: "2026-07-30T12:10:00Z",
      host_id: "host-abc",
      record: {
        kind: "sweep.completed",
        repo: "rjwalters/loom",
        visibility: "public",
        issue: 4703,
        sweep_id: "sweep-issue-4703-0",
        completed_at: "2026-07-30T12:10:00Z",
        result: "success",
      },
    };
    await callWorker(ingestRequest([completed], "Bearer abc-ingest-key"));

    const snapshotResponse = await callWorker(
      new Request("https://ingest.example/admin/fleet-state", {
        headers: { authorization: "Bearer test-admin-token" },
      }),
    );
    const snapshot = (await snapshotResponse.json()) as { activeSweeps: unknown[] };
    expect(snapshot.activeSweeps).toHaveLength(0);
  });
});

describe("POST /admin/hosts — host provisioning", () => {
  it("requires the admin token", async () => {
    const response = await callWorker(
      new Request("https://ingest.example/admin/hosts", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ host_id: "host-new" }),
      }),
    );
    expect(response.status).toBe(401);
  });

  it("creates a host and returns a usable ingest key exactly once", async () => {
    const createResponse = await callWorker(
      new Request("https://ingest.example/admin/hosts", {
        method: "POST",
        headers: { "content-type": "application/json", authorization: "Bearer test-admin-token" },
        body: JSON.stringify({ host_id: "host-new" }),
      }),
    );
    expect(createResponse.status).toBe(201);
    const { ingest_key: ingestKey } = (await createResponse.json()) as { ingest_key: string };
    expect(typeof ingestKey).toBe("string");
    expect(ingestKey.length).toBeGreaterThan(0);

    const ingestResponse = await callWorker(
      ingestRequest([sweepStartedEnvelope({ sweep_id: "sweep-new-host" })], `Bearer ${ingestKey}`),
    );
    expect(ingestResponse.status).toBe(200);
    expect(await recordsForHost("host-new")).toHaveLength(1);
  });

  it("rejects creating a duplicate host_id", async () => {
    const response = await callWorker(
      new Request("https://ingest.example/admin/hosts", {
        method: "POST",
        headers: { "content-type": "application/json", authorization: "Bearer test-admin-token" },
        body: JSON.stringify({ host_id: "host-abc" }),
      }),
    );
    expect(response.status).toBe(409);
  });
});

describe("POST /admin/hosts/:hostId/revoke", () => {
  it("revokes a host's key", async () => {
    const revokeResponse = await callWorker(
      new Request("https://ingest.example/admin/hosts/host-abc/revoke", {
        method: "POST",
        headers: { authorization: "Bearer test-admin-token" },
      }),
    );
    expect(revokeResponse.status).toBe(200);

    const ingestResponse = await callWorker(ingestRequest([sweepStartedEnvelope()], "Bearer abc-ingest-key"));
    expect(ingestResponse.status).toBe(401);
  });
});
