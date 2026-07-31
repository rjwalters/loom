/**
 * Route-level integration tests for the single-URL dashboard root (issue
 * #4795): `GET /` end to end through the real Worker, exercising the
 * *production* `validateAccessJwt` wiring in `src/index.ts` — i.e. the
 * default `createRemoteJWKSet`-backed resolver, not the injectable resolver
 * `test/accessAuth.test.ts` uses for its unit coverage. The JWKS HTTP fetch
 * itself is mocked (see `mockJwksFetch` below) so this suite never makes a
 * real network call, but every other step (cookie parsing, signature/aud/
 * iss/expiry verification, the route's authenticated-vs-public branch) runs
 * for real.
 */
import { createExecutionContext, env, waitOnExecutionContext } from "cloudflare:test";
import { exportJWK, generateKeyPair, SignJWT } from "jose";
import { afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest";
import worker from "../src/index";
import { seedHost, sweepOutcomeEnvelope } from "./testHelpers";

// Matches the `CF_ACCESS_TEAM_DOMAIN`/`CF_ACCESS_AUD` test bindings in
// vitest.config.ts.
const TEAM_DOMAIN = "test-team.cloudflareaccess.com";
const AUD = "test-login-app-aud-tag";
const CERTS_URL = `https://${TEAM_DOMAIN}/cdn-cgi/access/certs`;

async function callWorker(request: Request): Promise<Response> {
  const ctx = createExecutionContext();
  const response = await worker.fetch(request as Request<unknown, IncomingRequestCfProperties>, env, ctx);
  await waitOnExecutionContext(ctx);
  return response;
}

let privateKey: CryptoKey;
let jwks: { keys: Record<string, unknown>[] };

beforeAll(async () => {
  const keyPair = await generateKeyPair("RS256");
  privateKey = keyPair.privateKey;
  const jwk = await exportJWK(keyPair.publicKey);
  jwks = { keys: [{ ...jwk, alg: "RS256" }] };
});

/** Intercept only the JWKS certs URL; every other fetch (D1/DO bindings
 * don't go through `fetch`, so in practice nothing else calls this) passes
 * through to the real global fetch. */
function mockJwksFetch(): () => void {
  const realFetch = globalThis.fetch;
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
    if (url === CERTS_URL) {
      return new Response(JSON.stringify(jwks), { status: 200, headers: { "content-type": "application/json" } });
    }
    return realFetch(input as never, init as never);
  }) as typeof fetch;
  return () => {
    globalThis.fetch = realFetch;
  };
}

async function signToken(overrides: { aud?: string; expiresIn?: string } = {}): Promise<string> {
  return new SignJWT({ email: "operator@2amlogic.com" })
    .setProtectedHeader({ alg: "RS256" })
    .setIssuedAt()
    .setIssuer(`https://${TEAM_DOMAIN}`)
    .setAudience(overrides.aud ?? AUD)
    .setExpirationTime(overrides.expiresIn ?? "10m")
    .sign(privateKey);
}

/** The private-repo fixture below must never appear on the public variant of
 * `/` and must appear on the authenticated one — that pairing is what proves
 * the root route's branch actually swaps redacted for unredacted data, not
 * just the page's heading. */
const PRIVATE_REPO = "rjwalters/root-route-private-repo";
const PRIVATE_SWEEP_ID = "sweep-issue-7777-0";

async function ingestPrivateRecord(): Promise<void> {
  const response = await callWorker(
    new Request("https://ingest.example/ingest", {
      method: "POST",
      headers: { "content-type": "application/json", authorization: "Bearer root-route-ingest-key" },
      body: JSON.stringify([
        sweepOutcomeEnvelope({
          visibility: "private",
          repo: PRIVATE_REPO,
          issue: 7777,
          sweep_id: PRIVATE_SWEEP_ID,
        }),
      ]),
    }),
  );
  expect(response.status).toBe(200);
}

describe("GET / — Access JWT wired end to end (real createRemoteJWKSet path)", () => {
  let restoreFetch: () => void;

  beforeEach(async () => {
    await seedHost(env.DB, "host-abc", "root-route-ingest-key");
  });

  afterEach(() => {
    restoreFetch?.();
  });

  // NOTE on ordering: `src/accessAuth.ts` caches one `createRemoteJWKSet`
  // resolver **per team domain per isolate** (deliberately — see its module
  // doc) so this file's tests share it across `it` blocks, same as
  // production sharing it across requests. That means "the JWKS fetch
  // fails" MUST run before any test that lets a fetch succeed for this team
  // domain — a successful fetch caches valid keys for the resolver's
  // `cacheMaxAge`, which would make a later simulated-failure fetch
  // irrelevant (the resolver would just serve its already-cached keys,
  // proving nothing). Run in this declared order (vitest runs an `it` block
  // in declaration order by default; do not mark these `.concurrent`).
  it("a JWKS fetch failure (simulated network error) fails closed to the public view, not a 500", async () => {
    const realFetch = globalThis.fetch;
    globalThis.fetch = (async (input: RequestInfo | URL) => {
      const url = typeof input === "string" ? input : input instanceof URL ? input.toString() : input.url;
      if (url === CERTS_URL) throw new Error("simulated network failure");
      return realFetch(input as never);
    }) as typeof fetch;
    restoreFetch = () => {
      globalThis.fetch = realFetch;
    };
    const token = await signToken();

    const response = await callWorker(
      new Request("https://ingest.example/", { headers: { cookie: `CF_Authorization=${token}` } }),
    );

    expect(response.status).toBe(200);
    expect(await response.text()).toContain("Public View");
  });

  it("a valid, correctly-audienced CF_Authorization cookie renders the authenticated dashboard", async () => {
    restoreFetch = mockJwksFetch();
    const token = await signToken();

    const response = await callWorker(
      new Request("https://ingest.example/", { headers: { cookie: `CF_Authorization=${token}` } }),
    );

    expect(response.status).toBe(200);
    const html = await response.text();
    expect(html).toContain("Dashboard");
    expect(html).not.toContain("Sign in");
    expect(html).toContain("/api/events");
  });

  it("the authenticated variant serves UNREDACTED data the public variant withholds", async () => {
    restoreFetch = mockJwksFetch();
    await ingestPrivateRecord();

    const anonymous = await callWorker(new Request("https://ingest.example/"));
    const anonymousHtml = await anonymous.text();
    expect(anonymousHtml).not.toContain(PRIVATE_REPO);
    expect(anonymousHtml).not.toContain(PRIVATE_SWEEP_ID);

    const token = await signToken();
    const authenticated = await callWorker(
      new Request("https://ingest.example/", { headers: { cookie: `CF_Authorization=${token}` } }),
    );
    const authenticatedHtml = await authenticated.text();
    expect(authenticatedHtml).toContain(PRIVATE_REPO);
  });

  it("never lets a shared cache store either variant of / under one URL", async () => {
    // `/` returns two different bodies for the same URL depending on a
    // cookie. A cache that stored the authenticated body and replayed it to
    // an anonymous visitor would leak private-repo data past every check in
    // this file, so both variants must be explicitly uncacheable.
    restoreFetch = mockJwksFetch();
    const token = await signToken();

    for (const request of [
      new Request("https://ingest.example/"),
      new Request("https://ingest.example/", { headers: { cookie: `CF_Authorization=${token}` } }),
    ]) {
      const response = await callWorker(request);
      expect(response.headers.get("cache-control")).toBe("private, no-store");
      expect(response.headers.get("vary")).toBe("Cookie");
    }
  });

  it("a wrong-aud token still falls back to the public view (never the full view, never a 500)", async () => {
    restoreFetch = mockJwksFetch();
    const token = await signToken({ aud: "some-other-apps-aud-tag" });

    const response = await callWorker(
      new Request("https://ingest.example/", { headers: { cookie: `CF_Authorization=${token}` } }),
    );

    expect(response.status).toBe(200);
    const html = await response.text();
    expect(html).toContain("Public View");
    expect(html).toContain("Sign in");
  });

  it("an expired token falls back to the public view", async () => {
    restoreFetch = mockJwksFetch();
    const token = await signToken({ expiresIn: "-10m" });

    const response = await callWorker(
      new Request("https://ingest.example/", { headers: { cookie: `CF_Authorization=${token}` } }),
    );

    expect(response.status).toBe(200);
    expect(await response.text()).toContain("Public View");
  });
});
