import { hashIngestKey } from "../src/auth";

/** Insert a host + hashed ingest key directly into D1, bypassing the
 * `/admin/hosts` HTTP route — used by tests that only need a pre-existing
 * host, not to exercise host provisioning itself. */
export async function seedHost(db: D1Database, hostId: string, key: string): Promise<void> {
  const keyHash = await hashIngestKey(key);
  await db
    .prepare("INSERT INTO hosts (host_id, key_hash, created_at, revoked_at) VALUES (?, ?, ?, NULL)")
    .bind(hostId, keyHash, new Date().toISOString())
    .run();
}

export async function revokeHost(db: D1Database, hostId: string): Promise<void> {
  await db.prepare("UPDATE hosts SET revoked_at = ? WHERE host_id = ?").bind(new Date().toISOString(), hostId).run();
}

/** Build a minimal, valid `sweep.started` envelope for a batch fixture. */
export function sweepStartedEnvelope(overrides: Partial<Record<string, unknown>> = {}): Record<string, unknown> {
  return {
    schema_version: 1,
    emitted_at: "2026-07-30T12:00:00Z",
    host_id: "host-abc",
    record: {
      kind: "sweep.started",
      repo: "rjwalters/loom",
      visibility: "public",
      issue: 4703,
      sweep_id: "sweep-issue-4703-0",
      started_at: "2026-07-30T12:00:00Z",
      ...overrides,
    },
  };
}

export function hostHealthEnvelope(overrides: Partial<Record<string, unknown>> = {}): Record<string, unknown> {
  return {
    schema_version: 1,
    emitted_at: "2026-07-30T12:00:00Z",
    host_id: "host-abc",
    record: {
      kind: "host.health",
      captured_at: "2026-07-30T12:00:00Z",
      daemon_version: "0.16.0",
      uptime_sec: 100,
      logical_cpus: 8,
      ...overrides,
    },
  };
}
