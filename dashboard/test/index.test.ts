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

async function signToken(
  overrides: { aud?: string; expiresIn?: string; email?: string } = {},
): Promise<string> {
  return new SignJWT({ email: overrides.email ?? "operator@2amlogic.com" })
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
/**
 * Assert which dataset `GET /` told the UI it may request.
 *
 * `handleRoot` serves the built SPA shell (`web/dist/index.html` via the
 * `ASSETS` binding) and stamps the auth decision into `window.__LOOM_FLEET__`
 * — so the injected flag, not a rendered heading, is where the Access-JWT
 * verdict is now observable. Asserted as the exact serialized form, since a
 * substring like `"authenticated"` alone would pass on either value. When no
 * UI build is present the Worker falls back to the server-rendered page,
 * which `publicPage.test.ts` covers.
 */
function expectAuthState(html: string, authenticated: boolean): void {
  // Asserted on the key/value pair rather than the whole serialized object,
  // which also carries the viewer's `email` when there is one. Still pinned
  // to the exact literal `true`/`false` — a substring like `"authenticated"`
  // alone would pass on either value.
  expect(html).toContain(`window.__LOOM_FLEET__={"authenticated":${authenticated}`);
}

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
    expectAuthState(await response.text(), false);
  });

  it("a valid, correctly-audienced CF_Authorization cookie entitles the UI to the authenticated dataset", async () => {
    restoreFetch = mockJwksFetch();
    const token = await signToken();

    const response = await callWorker(
      new Request("https://ingest.example/", { headers: { cookie: `CF_Authorization=${token}` } }),
    );

    expect(response.status).toBe(200);
    expectAuthState(await response.text(), true);
  });

  it("the shell embeds no fleet data in either variant — the dataset split is enforced at /api vs /public", async () => {
    // The SPA shell is the same static bytes for everyone plus a one-line
    // auth flag; it carries no snapshot at all. That is a stronger property
    // than the server-rendered page's "redact before embedding": there is
    // nothing to leak in the document, so the only surface that can leak is
    // the API, which `redaction.test.ts` pins directly.
    restoreFetch = mockJwksFetch();
    await ingestPrivateRecord();

    const anonymous = await callWorker(new Request("https://ingest.example/"));
    const anonymousHtml = await anonymous.text();
    expect(anonymousHtml).not.toContain(PRIVATE_REPO);
    expect(anonymousHtml).not.toContain(PRIVATE_SWEEP_ID);
    expectAuthState(anonymousHtml, false);

    const token = await signToken();
    const authenticated = await callWorker(
      new Request("https://ingest.example/", { headers: { cookie: `CF_Authorization=${token}` } }),
    );
    const authenticatedHtml = await authenticated.text();
    expect(authenticatedHtml).not.toContain(PRIVATE_REPO);
    expect(authenticatedHtml).not.toContain(PRIVATE_SWEEP_ID);
    expectAuthState(authenticatedHtml, true);

    // ...and the data itself still splits the way the flag promises. Asserted
    // against `/history`, not `/fleet-state`: the fixture is a terminal
    // `sweep.outcome`, which lands in D1 history rather than the Durable
    // Object's live-sweep snapshot.
    const publicHistory = await callWorker(new Request("https://ingest.example/public/history"));
    const publicBody = await publicHistory.text();
    expect(publicBody).not.toContain(PRIVATE_REPO);
    expect(publicBody).not.toContain(PRIVATE_SWEEP_ID);

    const apiHistory = await callWorker(
      new Request("https://ingest.example/api/history", { headers: { cookie: `CF_Authorization=${token}` } }),
    );
    expect(await apiHistory.text()).toContain(PRIVATE_REPO);
  });

  // The reason `/api/*` verifies the JWT itself: serving the public view at
  // `/` requires deleting the hostname-wide Access application, which is
  // also the only thing that was gating `/api/*` at the edge. If these ever
  // start passing without a cookie, the unredacted fleet is public.
  it.each([
    ["/api/fleet-state", "https://ingest.example/api/fleet-state"],
    ["/api/history", "https://ingest.example/api/history"],
    ["/api/events", "https://ingest.example/api/events"],
  ])("%s refuses an anonymous request", async (_label, url) => {
    restoreFetch = mockJwksFetch();
    const response = await callWorker(new Request(url));

    expect(response.status).toBe(401);
    expect(await response.text()).not.toContain(PRIVATE_REPO);
  });

  it("/api/* refuses a token this Worker's audience does not accept", async () => {
    restoreFetch = mockJwksFetch();
    const token = await signToken({ aud: "some-other-apps-aud-tag" });

    const response = await callWorker(
      new Request("https://ingest.example/api/fleet-state", {
        headers: { cookie: `CF_Authorization=${token}` },
      }),
    );
    expect(response.status).toBe(401);
  });

  it("/api/* refuses an expired token", async () => {
    restoreFetch = mockJwksFetch();
    const token = await signToken({ expiresIn: "-10m" });

    const response = await callWorker(
      new Request("https://ingest.example/api/history", {
        headers: { cookie: `CF_Authorization=${token}` },
      }),
    );
    expect(response.status).toBe(401);
  });

  it("hands the SPA the signed-in operator's email for the account menu", async () => {
    restoreFetch = mockJwksFetch();
    const token = await signToken();

    const response = await callWorker(
      new Request("https://ingest.example/", { headers: { cookie: `CF_Authorization=${token}` } }),
    );

    // `signToken` puts this address in the JWT's `email` claim.
    expect(await response.text()).toContain('"email":"operator@2amlogic.com"');
  });

  it("never leaks an identity to an anonymous viewer", async () => {
    restoreFetch = mockJwksFetch();
    const response = await callWorker(new Request("https://ingest.example/"));
    const html = await response.text();

    expect(html).not.toContain("operator@2amlogic.com");
    expect(html).not.toContain('"email"');
  });

  // The injected state sits inside an inline <script>. A value containing the
  // literal `</script>` would terminate the block early and the remainder
  // would parse as markup. The email comes from a signature-verified JWT so
  // this is defense in depth, but it costs nothing and removes the need to
  // reason about how far an IdP claim can be trusted.
  it("escapes an injected identity so it cannot break out of the script block", async () => {
    restoreFetch = mockJwksFetch();
    const token = await signToken({ email: "</script><script>alert(1)</script>@evil.test" });

    const response = await callWorker(
      new Request("https://ingest.example/", { headers: { cookie: `CF_Authorization=${token}` } }),
    );
    const html = await response.text();

    // Exactly the two <script> tags the shell legitimately contains (the
    // injection and the module bundle) — the payload contributed none.
    expect(html).not.toContain("</script><script>alert(1)");
    expect(html).toContain("\\u003c/script\\u003e");
  });

  // The display timezone is deployment config, not identity: an anonymous
  // visitor reads the same charts as a signed-in one and must bucket them the
  // same way, or the two would disagree about which day a sweep happened on.
  // `DISPLAY_TIMEZONE` is unset in the test bindings, so neither variant
  // carries a `timeZone` and the UI falls back to the viewer's browser zone.
  it("omits the display timezone when the deployment does not configure one", async () => {
    restoreFetch = mockJwksFetch();
    const anonymous = await callWorker(new Request("https://ingest.example/"));
    expect(await anonymous.text()).not.toContain('"timeZone"');

    const token = await signToken();
    const authenticated = await callWorker(
      new Request("https://ingest.example/", { headers: { cookie: `CF_Authorization=${token}` } }),
    );
    expect(await authenticated.text()).not.toContain('"timeZone"');
  });

  it("/public/* stays reachable anonymously — the split is auth, not obscurity", async () => {
    const response = await callWorker(new Request("https://ingest.example/public/fleet-state"));
    expect(response.status).toBe(200);
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
    expectAuthState(await response.text(), false);
  });

  it("an expired token falls back to the public view", async () => {
    restoreFetch = mockJwksFetch();
    const token = await signToken({ expiresIn: "-10m" });

    const response = await callWorker(
      new Request("https://ingest.example/", { headers: { cookie: `CF_Authorization=${token}` } }),
    );

    expect(response.status).toBe(200);
    expectAuthState(await response.text(), false);
  });
});
