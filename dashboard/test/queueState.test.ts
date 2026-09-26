/**
 * `queue.snapshot` ingest, live state and redaction (Issue #8852, phase 3).
 * Pure-function coverage first, then the real `/ingest` → `/api|public/*`
 * routes end to end.
 */
import { createExecutionContext, env, waitOnExecutionContext } from "cloudflare:test";
import { beforeAll, beforeEach, describe, expect, it } from "vitest";
import worker from "../src/index";
import { LIVE_AFTER_SEC, PRUNE_AFTER_MS } from "../src/fleetState";
import {
  classifyAndPruneQueues,
  MAX_STORED_ROWS,
  normalizeQueueSnapshot,
  redactQueueSnapshot,
  shouldReplaceQueue,
  type HostQueueEntry,
} from "../src/queueState";
import { authedRequest, initAccessTestKeys, mockJwksFetch, seedHost } from "./testHelpers";

/** A `queue.snapshot` payload shaped exactly as the daemon's
 * `QueueSnapshotRecord` serializes (loom-daemon/src/telemetry/queue_snapshot.rs). */
function queueRecord(overrides: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    kind: "queue.snapshot",
    tick_at: "2026-09-25T12:00:00Z",
    max_concurrent: 4,
    seen: 3,
    counts: { running: 1, ready: 1, blocked: 1 },
    listing_failed: [{ repo: "acme/secret-infra", visibility: "private" }],
    rows: [
      {
        rank: 1,
        repo: "rjwalters/loom",
        visibility: "public",
        issue: 8852,
        workspace_priority: 100,
        urgent: true,
        created_at: "2026-09-20T00:00:00Z",
        tier: "tier:goal-advancing",
        disposition: "in_flight",
        state: "running",
        reason: "sweep already running",
      },
      {
        rank: 2,
        repo: "acme/secret-app",
        visibility: "private",
        issue: 77,
        workspace_priority: 100,
        urgent: false,
        created_at: "2026-09-21T00:00:00Z",
        tier: "tier:secret-tier",
        disposition: "open_pr",
        state: "blocked",
        reason: "blocked: open linked PR",
        detail: "open PR #78",
      },
      {
        rank: 3,
        repo: "acme/secret-app",
        issue: 79,
        workspace_priority: 100,
        urgent: false,
        disposition: "deferred_capacity",
        state: "ready",
        reason: "waiting: concurrency cap full",
      },
    ],
    unresolved_rows: 0,
    rows_truncated: 0,
    ...overrides,
  };
}

function envelope(record: Record<string, unknown>): Record<string, unknown> {
  return { schema_version: 11, emitted_at: "2026-09-25T12:00:01Z", host_id: "host-abc", record };
}

describe("normalizeQueueSnapshot", () => {
  it("keeps the known fields and decodes a missing row visibility as private", () => {
    const snapshot = normalizeQueueSnapshot(queueRecord());
    expect(snapshot?.seen).toBe(3);
    expect(snapshot?.counts).toEqual({ running: 1, ready: 1, blocked: 1 });
    expect(snapshot?.rows.map((row) => row.visibility)).toEqual(["public", "private", "private"]);
    expect(snapshot?.rows[1]?.detail).toBe("open PR #78");
  });

  it("drops fields it does not know, so they can never reach a response", () => {
    const snapshot = normalizeQueueSnapshot(
      queueRecord({ local_path: "/Users/someone/secret", rows: [{ rank: 1, state: "ready", root: "/Users/x" }] }),
    );
    expect(JSON.stringify(snapshot)).not.toContain("/Users/");
  });

  it("rejects a payload with no parseable tick_at", () => {
    expect(normalizeQueueSnapshot(queueRecord({ tick_at: undefined }))).toBeUndefined();
    expect(normalizeQueueSnapshot(queueRecord({ tick_at: "not a date" }))).toBeUndefined();
  });

  it("maps an unrecognized state to unknown rather than guessing", () => {
    const snapshot = normalizeQueueSnapshot(queueRecord({ rows: [{ rank: 1, state: "exploded", disposition: "x" }] }));
    expect(snapshot?.rows[0]?.state).toBe("unknown");
  });

  it("caps stored rows and counts the overflow as truncated", () => {
    const rows = Array.from({ length: MAX_STORED_ROWS + 5 }, (_, i) => ({ rank: i + 1, state: "ready" }));
    const snapshot = normalizeQueueSnapshot(queueRecord({ rows, rows_truncated: 2 }));
    expect(snapshot?.rows).toHaveLength(MAX_STORED_ROWS);
    expect(snapshot?.rows_truncated).toBe(7);
  });
});

describe("shouldReplaceQueue", () => {
  const stored: HostQueueEntry = {
    record: normalizeQueueSnapshot(queueRecord())!,
    updatedAt: "2026-09-25T12:00:05Z",
  };

  it("accepts only a strictly newer tick", () => {
    expect(shouldReplaceQueue(undefined, stored.record)).toBe(true);
    expect(shouldReplaceQueue(stored, normalizeQueueSnapshot(queueRecord({ tick_at: "2026-09-25T12:05:00Z" }))!)).toBe(
      true,
    );
    expect(shouldReplaceQueue(stored, stored.record)).toBe(false);
    expect(shouldReplaceQueue(stored, normalizeQueueSnapshot(queueRecord({ tick_at: "2026-09-25T11:55:00Z" }))!)).toBe(
      false,
    );
  });
});

describe("classifyAndPruneQueues", () => {
  const now = new Date("2026-09-25T12:00:00Z");
  const record = normalizeQueueSnapshot(queueRecord())!;
  const at = (secondsAgo: number) => new Date(now.getTime() - secondsAgo * 1000).toISOString();

  it("classifies freshness on the host cadence and prunes long-gone hosts", () => {
    const { queues, pruneKeys } = classifyAndPruneQueues(
      new Map<string, HostQueueEntry>([
        ["queue:live", { record, updatedAt: at(60) }],
        ["queue:stale", { record, updatedAt: at(LIVE_AFTER_SEC + 60) }],
        ["queue:gone", { record, updatedAt: at(PRUNE_AFTER_MS / 1000 + 60) }],
      ]),
      now,
    );
    expect(queues.live?.freshness?.status).toBe("live");
    expect(queues.stale?.freshness?.status).toBe("stale");
    expect(queues.gone).toBeUndefined();
    expect(pruneKeys).toEqual(["queue:gone"]);
  });
});

describe("redactQueueSnapshot", () => {
  it("strips every identifying field from private rows and keeps public rows whole", () => {
    const redacted = redactQueueSnapshot(normalizeQueueSnapshot(queueRecord())!);
    const text = JSON.stringify(redacted);
    expect(text).not.toContain("acme/");
    expect(text).not.toContain("secret");
    expect(text).not.toContain("#78");
    expect(redacted.rows[0]).toMatchObject({ repo: "rjwalters/loom", issue: 8852, tier: "tier:goal-advancing" });
    expect(redacted.rows[1]).toEqual({
      rank: 2,
      visibility: "private",
      urgent: false,
      disposition: "open_pr",
      state: "blocked",
      reason: "blocked: open linked PR",
    });
    expect(redacted.withheld_rows).toBe(2);
    expect(redacted.counts).toEqual({ running: 1, ready: 1, blocked: 1 });
    expect(redacted.listing_failed).toEqual([{ visibility: "private" }]);
  });
});

describe("forge-side labelled_blocked rows (#8957)", () => {
  const blockedRow = (repo: string, visibility: string) => ({
    rank: 0,
    repo,
    visibility,
    issue: 4242,
    workspace_priority: 100,
    urgent: false,
    created_at: "2026-09-01T00:00:00Z",
    tier: "tier:secret-tier",
    disposition: "labelled_blocked",
    state: "blocked",
    reason: "blocked: labelled loom:blocked",
    detail: "loom:operator",
  });

  it("keeps an unranked (rank 0) row with its new disposition", () => {
    const record = normalizeQueueSnapshot(queueRecord({ rows: [blockedRow("rjwalters/loom", "public")] }))!;
    expect(record.rows).toHaveLength(1);
    expect(record.rows[0]).toMatchObject({ rank: 0, disposition: "labelled_blocked", state: "blocked", detail: "loom:operator" });
  });

  it("redacts a private labelled_blocked row like any other private row", () => {
    const record = normalizeQueueSnapshot(queueRecord({ rows: [blockedRow("acme/secret-app", "private")] }))!;
    const redacted = redactQueueSnapshot(record);
    expect(redacted.rows[0]).toEqual({
      rank: 0,
      visibility: "private",
      urgent: false,
      disposition: "labelled_blocked",
      state: "blocked",
      reason: "blocked: labelled loom:blocked",
    });
    expect(JSON.stringify(redacted.rows)).not.toContain("4242");
  });
});

// ---------------------------------------------------------------------------
// End to end
// ---------------------------------------------------------------------------

async function callWorker(request: Request): Promise<Response> {
  const ctx = createExecutionContext();
  const response = await worker.fetch(request as Request<unknown, IncomingRequestCfProperties>, env, ctx);
  await waitOnExecutionContext(ctx);
  return response;
}

async function ingest(envelopes: unknown[]): Promise<Response> {
  return callWorker(
    new Request("https://ingest.example/ingest", {
      method: "POST",
      headers: { "content-type": "application/json", authorization: "Bearer abc-ingest-key" },
      body: JSON.stringify(envelopes),
    }),
  );
}

type FleetBody = { hosts: Record<string, { queue?: HostQueueEntry }> };

beforeAll(async () => {
  await initAccessTestKeys();
  mockJwksFetch();
});

beforeEach(async () => {
  await seedHost(env.DB, "host-abc", "abc-ingest-key");
});

describe("queue.snapshot through /ingest", () => {
  it("lands on the host's fleet-state entry with its freshness", async () => {
    expect((await ingest([envelope(queueRecord())])).status).toBe(200);
    const response = await callWorker(await authedRequest("https://ingest.example/api/fleet-state"));
    const body = (await response.json()) as FleetBody;
    const queue = body.hosts["host-abc"]?.queue;
    expect(queue?.record.tick_at).toBe("2026-09-25T12:00:00Z");
    expect(queue?.record.rows).toHaveLength(3);
    expect(queue?.freshness?.status).toBe("live");
    expect(JSON.stringify(queue)).toContain("acme/secret-app");
  });

  it("an older, redelivered snapshot does not overwrite a newer one", async () => {
    await ingest([envelope(queueRecord({ tick_at: "2026-09-25T12:05:00Z", seen: 9 }))]);
    await ingest([envelope(queueRecord({ tick_at: "2026-09-25T12:00:00Z", seen: 3 }))]);
    const response = await callWorker(await authedRequest("https://ingest.example/api/fleet-state"));
    const body = (await response.json()) as FleetBody;
    expect(body.hosts["host-abc"]?.queue?.record.seen).toBe(9);
  });

  it("the public fleet-state route never leaks a private row's repo, issue, tier or PR", async () => {
    await ingest([envelope(queueRecord())]);
    const response = await callWorker(new Request("https://ingest.example/public/fleet-state"));
    const text = await response.text();
    expect(text).not.toContain("acme/");
    expect(text).not.toContain("secret");
    expect(text).not.toContain("#78");
    expect(text).toContain("rjwalters/loom");
    const body = JSON.parse(text) as FleetBody;
    expect(body.hosts["host-abc"]?.queue?.record.counts).toEqual({ running: 1, ready: 1, blocked: 1 });
  });

  it("the public history route applies the same per-row redaction", async () => {
    await ingest([envelope(queueRecord())]);
    const publicText = await (await callWorker(new Request("https://ingest.example/public/history"))).text();
    expect(publicText).toContain("queue.snapshot");
    expect(publicText).not.toContain("acme/");
    expect(publicText).not.toContain("secret");
    expect(publicText).toContain("rjwalters/loom");

    const authText = await (await callWorker(await authedRequest("https://ingest.example/api/history"))).text();
    expect(authText).toContain("acme/secret-app");
  });
});
