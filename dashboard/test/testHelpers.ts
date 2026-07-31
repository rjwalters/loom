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

/** Build a minimal, valid `sweep.phase` envelope for a batch fixture. */
export function sweepPhaseEnvelope(overrides: Partial<Record<string, unknown>> = {}): Record<string, unknown> {
  return {
    schema_version: 1,
    emitted_at: "2026-07-30T12:03:20Z",
    host_id: "host-abc",
    record: {
      kind: "sweep.phase",
      repo: "rjwalters/loom",
      visibility: "public",
      issue: 4703,
      sweep_id: "sweep-issue-4703-0",
      phase: "builder",
      entered_at: "2026-07-30T12:03:20Z",
      ...overrides,
    },
  };
}

/** Build a minimal, valid `sweep.completed` envelope for a batch fixture. */
export function sweepCompletedEnvelope(overrides: Partial<Record<string, unknown>> = {}): Record<string, unknown> {
  return {
    schema_version: 1,
    emitted_at: "2026-07-30T12:08:32Z",
    host_id: "host-abc",
    record: {
      kind: "sweep.completed",
      repo: "rjwalters/loom",
      visibility: "public",
      issue: 4703,
      sweep_id: "sweep-issue-4703-0",
      completed_at: "2026-07-30T12:08:32Z",
      result: "success",
      ...overrides,
    },
  };
}

/** Build a minimal, valid `sweep.outcome` envelope for a batch fixture. */
export function sweepOutcomeEnvelope(overrides: Partial<Record<string, unknown>> = {}): Record<string, unknown> {
  return {
    schema_version: 1,
    emitted_at: "2026-07-30T12:08:32Z",
    host_id: "host-abc",
    record: {
      kind: "sweep.outcome",
      repo: "rjwalters/loom",
      visibility: "public",
      issue: 4703,
      sweep_id: "sweep-issue-4703-0",
      model: "opus",
      effort: "high",
      config: { runtime: "claude" },
      phase_durations: [{ phase: "builder", duration_sec: 340 }],
      total_duration_sec: 512,
      result: "success",
      pr_number: 4710,
      ...overrides,
    },
  };
}

/** Build a minimal, valid `tokens.snapshot` envelope for a batch fixture
 * (host-level — no `repo`/`visibility`). */
export function tokensSnapshotEnvelope(overrides: Partial<Record<string, unknown>> = {}): Record<string, unknown> {
  return {
    schema_version: 1,
    emitted_at: "2026-07-30T12:00:00Z",
    host_id: "host-abc",
    record: {
      kind: "tokens.snapshot",
      captured_at: "2026-07-30T12:00:00Z",
      accounts: [{ account: "agent-1", rank: 0, usage_fraction: 0.42, exhausted: false }],
      ...overrides,
    },
  };
}

// ---------------------------------------------------------------------------
// Cloudflare Access test identity
// ---------------------------------------------------------------------------
//
// `/api/*` verifies the Access JWT in-Worker (see `src/index.ts`'s route
// table for why the edge no longer does it), so any suite that exercises an
// `/api/*` route needs a real, correctly-signed, correctly-audienced cookie.
// The signing key and the JWKS mock live here so all three suites share one
// identity rather than each growing its own copy.

/** Matches the `CF_ACCESS_TEAM_DOMAIN`/`CF_ACCESS_AUD` bindings in
 * `vitest.config.ts`. */
export const TEST_TEAM_DOMAIN = "test-team.cloudflareaccess.com";
export const TEST_AUD = "test-login-app-aud-tag";
export const TEST_CERTS_URL = `https://${TEST_TEAM_DOMAIN}/cdn-cgi/access/certs`;

let signingKey: CryptoKey | undefined;
let jwks: { keys: Record<string, unknown>[] } | undefined;

/** Generate the suite's RS256 keypair. Call once from `beforeAll`. */
export async function initAccessTestKeys(): Promise<void> {
  if (signingKey) return;
  const { exportJWK, generateKeyPair } = await import("jose");
  const keyPair = await generateKeyPair("RS256");
  signingKey = keyPair.privateKey;
  const jwk = await exportJWK(keyPair.publicKey);
  jwks = { keys: [{ ...jwk, alg: "RS256" }] };
}

/**
 * Intercept only the JWKS certs URL; every other fetch passes through.
 * Returns the restore function.
 *
 * NOTE on ordering: `src/accessAuth.ts` caches one resolver per team domain
 * per isolate (deliberately — see its module doc), so within a file a test
 * that needs the JWKS fetch to *fail* must run before any test that lets it
 * succeed. Suites that only need success can install this once in `beforeAll`.
 */
export function mockJwksFetch(): () => void {
  const realFetch = globalThis.fetch;
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
    if (url === TEST_CERTS_URL) {
      return new Response(JSON.stringify(jwks), { status: 200, headers: { "content-type": "application/json" } });
    }
    return realFetch(input as never, init as never);
  }) as typeof fetch;
  return () => {
    globalThis.fetch = realFetch;
  };
}

/** Sign an Access JWT for the test team domain/audience. */
export async function signAccessToken(
  overrides: { aud?: string; expiresIn?: string; email?: string } = {},
): Promise<string> {
  if (!signingKey) throw new Error("call initAccessTestKeys() in beforeAll first");
  const { SignJWT } = await import("jose");
  return new SignJWT({ email: overrides.email ?? "operator@2amlogic.com" })
    .setProtectedHeader({ alg: "RS256" })
    .setIssuedAt()
    .setIssuer(`https://${TEST_TEAM_DOMAIN}`)
    .setAudience(overrides.aud ?? TEST_AUD)
    .setExpirationTime(overrides.expiresIn ?? "10m")
    .sign(signingKey);
}

/** A `Request` carrying a valid Access session cookie — the shorthand for
 * "an operator hitting this route from the signed-in dashboard". */
export async function authedRequest(url: string, init: RequestInit = {}): Promise<Request> {
  const token = await signAccessToken();
  const headers = new Headers(init.headers);
  headers.set("cookie", `CF_Authorization=${token}`);
  return new Request(url, { ...init, headers });
}
