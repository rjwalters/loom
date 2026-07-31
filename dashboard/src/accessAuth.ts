/**
 * Cloudflare Access JWT validation for the single-URL dashboard root (issue
 * #4795, Epic #4702). This is the Worker's first cryptographic-verification
 * code path — kept deliberately separate from `./auth.ts` (per-host
 * *ingest-key* auth: a SHA-256 hash comparison against D1, a completely
 * different trust boundary) rather than folding into it.
 *
 * ## Where the token comes from
 *
 * The re-plumbed Access layout (see `docs/cloudflare-access.md`) covers only
 * `/login` with an Access application — the dashboard root `/` deliberately
 * bypasses Access so anonymous visitors land on the public view instead of
 * an SSO wall. That means Access does **not** inject its
 * `Cf-Access-Jwt-Assertion` header on requests to `/` (it only does that for
 * requests that traversed an Access-gated app). What it *does* do, after a
 * successful `/login` round trip, is set a `CF_Authorization` session cookie
 * scoped to the whole zone — the browser keeps sending that cookie on every
 * subsequent request to `/`, gated or not. So this module reads the JWT out
 * of the **cookie**, not the header.
 *
 * ## Fail-closed, always
 *
 * Every error path here — missing config, missing/malformed cookie, network
 * failure fetching the JWKS, bad signature, wrong `aud`, wrong `iss`,
 * expired token — resolves to `null`, never throws. Callers MUST treat
 * `null` as "no valid identity" and fall back to rendering the public view.
 * This is the load-bearing security property of this module: a subtle bug
 * that turned any of the above into a fail-*open* branch would be a real
 * auth bypass that a superficial test pass could still look correct.
 *
 * ## AUD pinning
 *
 * `env.CF_ACCESS_AUD` is checked against the token's `aud` claim on every
 * verification. Without this, a JWT minted for a *different* Access
 * application on the same zone (e.g. someone else's app sharing the same
 * team/JWKS) would also validate here — the AUD tag scopes acceptance to
 * tokens issued specifically for this app. Neither the team domain nor the
 * AUD tag is a secret (both are shown on the Access app's own dashboard
 * page), so both are plain `[vars]`, not `wrangler secret`s.
 *
 * `CF_ACCESS_AUD` accepts a **comma-separated list** of AUD tags, and this
 * is not incidental. Access names the session cookie `CF_Authorization` for
 * the whole *hostname*, but mints its token per *application* — so on a
 * hostname running several path-scoped apps (this layout runs `/login`,
 * `/api/*`, and `/admin/*`), the single cookie the browser holds can carry
 * whichever app's `aud` was issued most recently. A visitor who signs in at
 * `/login` and whose dashboard page then opens the `/api/events` live tail
 * can end up holding an `/api`-app token; pinning to the `/login` AUD alone
 * would bounce that already-authenticated operator back to the public view
 * on their next page load. Listing every app AUD on the hostname whose
 * session should count as signed-in fixes that without weakening the pin:
 * acceptance is still an explicit allowlist of tags this operator
 * configured, never "any token from this team".
 */

import { createRemoteJWKSet, jwtVerify, type JWTVerifyGetKey } from "jose";

export interface AccessAuthEnv {
  CF_ACCESS_TEAM_DOMAIN?: string;
  /** One AUD tag, or several comma-separated — see the module doc's AUD
   * pinning section for why a list is the right shape here. */
  CF_ACCESS_AUD?: string;
}

export interface AccessIdentity {
  /** The Access-authenticated user's email, when the token carries one. */
  email?: string;
  /** The token's `sub` claim, if present. */
  sub?: string;
}

const COOKIE_NAME = "CF_Authorization";

/** Cloudflare Access signs its application tokens with RS256, and its
 * `/cdn-cgi/access/certs` JWKS serves RSA public keys only. Pinning the
 * accepted algorithm here is defense in depth against algorithm-confusion:
 * `jose` already refuses `alg: none` outright and would fail to match an
 * `HS256` token against an RSA JWK, but stating the expected algorithm
 * explicitly means a future JWKS that also published, say, a symmetric or
 * weaker key could not silently widen what this Worker accepts. */
const ACCEPTED_ALGORITHMS = ["RS256"];

/** Extract the `CF_Authorization` cookie's value from a `Cookie` request
 * header, or `null` if the header is absent or the cookie is not present.
 * Deliberately tolerant of extra cookies / odd spacing — a `Cookie` header
 * is a semicolon-separated list the browser controls, not a value this
 * Worker should be strict about parsing beyond finding its own cookie. */
export function extractAccessCookie(cookieHeader: string | null): string | null {
  if (!cookieHeader) return null;
  for (const part of cookieHeader.split(";")) {
    const eq = part.indexOf("=");
    if (eq === -1) continue;
    const name = part.slice(0, eq).trim();
    if (name !== COOKIE_NAME) continue;
    const value = part.slice(eq + 1).trim();
    return value.length > 0 ? value : null;
  }
  return null;
}

// One JWKS resolver per team domain per isolate. `createRemoteJWKSet` already
// caches the fetched keys internally (and re-fetches on a `kid` miss / after
// its own cache duration), so this map only avoids constructing a new
// resolver — and therefore a cold cache — on every single request.
const remoteJwksCache = new Map<string, JWTVerifyGetKey>();

function remoteJwks(teamDomain: string): JWTVerifyGetKey {
  let resolver = remoteJwksCache.get(teamDomain);
  if (!resolver) {
    resolver = createRemoteJWKSet(new URL(`https://${teamDomain}/cdn-cgi/access/certs`));
    remoteJwksCache.set(teamDomain, resolver);
  }
  return resolver;
}

/** Split `CF_ACCESS_AUD` into the allowlist of accepted AUD tags, dropping
 * empty entries so a trailing comma or a stray space cannot smuggle in an
 * empty-string audience. An all-blank value yields an empty list, which the
 * caller treats as "not configured" — i.e. fails closed, not open. */
export function parseAcceptedAudiences(raw: string | undefined): string[] {
  if (!raw) return [];
  return raw
    .split(",")
    .map((entry) => entry.trim())
    .filter((entry) => entry.length > 0);
}

/**
 * Validate the Access JWT carried in `cookieHeader` against
 * `env.CF_ACCESS_TEAM_DOMAIN`'s JWKS, pinning `aud` to one of
 * `env.CF_ACCESS_AUD`'s tags and `iss` to the team domain. Returns the
 * decoded identity on success, `null` on absolutely any failure (see the
 * module doc — this function never throws).
 *
 * `jwksResolver` is an injection point for tests (a local, in-memory JWKS
 * signed with a test key pair) — production callers should omit it and get
 * the real Cloudflare-hosted JWKS, cached per team domain.
 */
export async function validateAccessJwt(
  cookieHeader: string | null,
  env: AccessAuthEnv,
  jwksResolver: (teamDomain: string) => JWTVerifyGetKey = remoteJwks,
): Promise<AccessIdentity | null> {
  const teamDomain = env.CF_ACCESS_TEAM_DOMAIN;
  const audiences = parseAcceptedAudiences(env.CF_ACCESS_AUD);
  if (!teamDomain || audiences.length === 0) return null;

  const token = extractAccessCookie(cookieHeader);
  if (!token) return null;

  try {
    const jwks = jwksResolver(teamDomain);
    const { payload } = await jwtVerify(token, jwks, {
      issuer: `https://${teamDomain}`,
      // `jose` accepts the token when its `aud` matches ANY entry here — an
      // explicit allowlist, never a wildcard.
      audience: audiences,
      algorithms: ACCEPTED_ALGORITHMS,
      // No `clockTolerance`: an expired token is expired. `jose`'s default is
      // zero tolerance and this comment exists so nobody "helpfully" adds
      // slack to it later.
    });
    return {
      email: typeof payload.email === "string" ? payload.email : undefined,
      sub: typeof payload.sub === "string" ? payload.sub : undefined,
    };
  } catch {
    // Signature failure, expired/not-yet-valid token, wrong aud/iss, a JWKS
    // fetch that threw (network error, non-2xx, malformed JSON) — every one
    // of these lands here and fails closed. Deliberately not logging the
    // token or the raw error: this path runs on every anonymous request to
    // `/`, and a malformed/garbage cookie is expected, unremarkable input,
    // not an incident to record.
    return null;
  }
}
