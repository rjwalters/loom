import { createLocalJWKSet, exportJWK, generateKeyPair, SignJWT, type JWTVerifyGetKey } from "jose";
import { beforeAll, describe, expect, it } from "vitest";
import { extractAccessCookie, parseAcceptedAudiences, validateAccessJwt } from "../src/accessAuth";

const TEAM_DOMAIN = "test-team.cloudflareaccess.com";
const AUD = "test-login-app-aud-tag";

describe("extractAccessCookie", () => {
  it("extracts the cookie's value from a well-formed Cookie header", () => {
    expect(extractAccessCookie("CF_Authorization=abc.def.ghi")).toBe("abc.def.ghi");
  });

  it("finds the cookie among several others, in any position", () => {
    expect(extractAccessCookie("foo=bar; CF_Authorization=the-token; baz=qux")).toBe("the-token");
    expect(extractAccessCookie("CF_Authorization=the-token; foo=bar")).toBe("the-token");
  });

  it("returns null for a missing header", () => {
    expect(extractAccessCookie(null)).toBeNull();
  });

  it("returns null when the cookie is absent", () => {
    expect(extractAccessCookie("foo=bar; baz=qux")).toBeNull();
  });

  it("returns null for an empty cookie value", () => {
    expect(extractAccessCookie("CF_Authorization=")).toBeNull();
  });

  it("tolerates irregular spacing", () => {
    expect(extractAccessCookie("  CF_Authorization=the-token  ;  foo=bar")).toBe("the-token");
  });
});

describe("parseAcceptedAudiences", () => {
  it("parses one tag, several tags, and tolerates spacing", () => {
    expect(parseAcceptedAudiences("aud-one")).toEqual(["aud-one"]);
    expect(parseAcceptedAudiences("aud-one,aud-two")).toEqual(["aud-one", "aud-two"]);
    expect(parseAcceptedAudiences(" aud-one , aud-two ")).toEqual(["aud-one", "aud-two"]);
  });

  it("yields an empty list — never an empty-string audience — for blank input", () => {
    // An empty-string entry slipping into the allowlist would be a token
    // with no `aud` matching the pin, i.e. a fail-open.
    expect(parseAcceptedAudiences(undefined)).toEqual([]);
    expect(parseAcceptedAudiences("")).toEqual([]);
    expect(parseAcceptedAudiences("   ")).toEqual([]);
    expect(parseAcceptedAudiences(",,")).toEqual([]);
    expect(parseAcceptedAudiences("aud-one,,")).toEqual(["aud-one"]);
  });
});

describe("validateAccessJwt", () => {
  let privateKey: CryptoKey;
  let jwksResolver: (teamDomain: string) => JWTVerifyGetKey;
  let unreachableResolver: (teamDomain: string) => JWTVerifyGetKey;

  beforeAll(async () => {
    const keyPair = await generateKeyPair("RS256");
    privateKey = keyPair.privateKey;
    const jwk = await exportJWK(keyPair.publicKey);
    const localJwks = createLocalJWKSet({ keys: [{ ...jwk, alg: "RS256" }] });
    jwksResolver = () => localJwks;
    // Simulates a JWKS endpoint that is unreachable (network failure) — the
    // resolver itself throws synchronously, same failure shape a thrown
    // fetch rejection inside `createRemoteJWKSet`'s resolver would produce.
    unreachableResolver = () => {
      throw new Error("simulated JWKS fetch failure");
    };
  });

  async function signToken(overrides: {
    aud?: string;
    iss?: string;
    expiresIn?: string;
    email?: string;
  } = {}): Promise<string> {
    return new SignJWT({ email: overrides.email ?? "operator@2amlogic.com" })
      .setProtectedHeader({ alg: "RS256" })
      .setIssuedAt()
      .setIssuer(overrides.iss ?? `https://${TEAM_DOMAIN}`)
      .setAudience(overrides.aud ?? AUD)
      .setExpirationTime(overrides.expiresIn ?? "10m")
      .sign(privateKey);
  }

  const env = { CF_ACCESS_TEAM_DOMAIN: TEAM_DOMAIN, CF_ACCESS_AUD: AUD };

  it("accepts a validly signed token with the correct aud/iss, resolving the identity", async () => {
    const token = await signToken();
    const result = await validateAccessJwt(`CF_Authorization=${token}`, env, jwksResolver);
    expect(result).toEqual({ email: "operator@2amlogic.com", sub: undefined });
  });

  it("rejects (falls back to public — returns null) a token with the wrong aud", async () => {
    const token = await signToken({ aud: "some-other-apps-aud-tag" });
    const result = await validateAccessJwt(`CF_Authorization=${token}`, env, jwksResolver);
    expect(result).toBeNull();
  });

  it("rejects a token with the wrong issuer", async () => {
    const token = await signToken({ iss: "https://someone-elses-team.cloudflareaccess.com" });
    const result = await validateAccessJwt(`CF_Authorization=${token}`, env, jwksResolver);
    expect(result).toBeNull();
  });

  it("rejects an expired token", async () => {
    const token = await signToken({ expiresIn: "-10m" });
    const result = await validateAccessJwt(`CF_Authorization=${token}`, env, jwksResolver);
    expect(result).toBeNull();
  });

  it("rejects a malformed/garbage cookie value without throwing", async () => {
    await expect(
      validateAccessJwt("CF_Authorization=not-a-jwt-at-all", env, jwksResolver),
    ).resolves.toBeNull();
  });

  it("rejects a missing cookie", async () => {
    const result = await validateAccessJwt(null, env, jwksResolver);
    expect(result).toBeNull();
    const resultNoCookieHeader = await validateAccessJwt("foo=bar", env, jwksResolver);
    expect(resultNoCookieHeader).toBeNull();
  });

  it("fails closed (null, not a throw) when the JWKS endpoint is unreachable", async () => {
    const token = await signToken();
    await expect(
      validateAccessJwt(`CF_Authorization=${token}`, env, unreachableResolver),
    ).resolves.toBeNull();
  });

  it("fails closed when CF_ACCESS_TEAM_DOMAIN is not configured", async () => {
    const token = await signToken();
    const result = await validateAccessJwt(`CF_Authorization=${token}`, { CF_ACCESS_AUD: AUD }, jwksResolver);
    expect(result).toBeNull();
  });

  it("fails closed when CF_ACCESS_AUD is not configured", async () => {
    const token = await signToken();
    const result = await validateAccessJwt(
      `CF_Authorization=${token}`,
      { CF_ACCESS_TEAM_DOMAIN: TEAM_DOMAIN },
      jwksResolver,
    );
    expect(result).toBeNull();
  });

  it("accepts a token audienced for any tag in a comma-separated CF_ACCESS_AUD", async () => {
    // One hostname, several path-scoped Access apps, one cookie name: the
    // browser can hold a token minted by whichever app it most recently
    // traversed. Both configured tags must unlock the root.
    const multiAudEnv = { CF_ACCESS_TEAM_DOMAIN: TEAM_DOMAIN, CF_ACCESS_AUD: `${AUD},api-app-aud-tag` };

    const loginToken = await signToken();
    await expect(validateAccessJwt(`CF_Authorization=${loginToken}`, multiAudEnv, jwksResolver)).resolves.toEqual({
      email: "operator@2amlogic.com",
      sub: undefined,
    });

    const apiToken = await signToken({ aud: "api-app-aud-tag" });
    await expect(validateAccessJwt(`CF_Authorization=${apiToken}`, multiAudEnv, jwksResolver)).resolves.toEqual({
      email: "operator@2amlogic.com",
      sub: undefined,
    });
  });

  it("still rejects a tag that is NOT in the configured list", async () => {
    const multiAudEnv = { CF_ACCESS_TEAM_DOMAIN: TEAM_DOMAIN, CF_ACCESS_AUD: `${AUD},api-app-aud-tag` };
    const token = await signToken({ aud: "somebody-elses-app-aud-tag" });
    await expect(validateAccessJwt(`CF_Authorization=${token}`, multiAudEnv, jwksResolver)).resolves.toBeNull();
  });

  it("fails closed when CF_ACCESS_AUD is present but blank", async () => {
    const token = await signToken();
    const result = await validateAccessJwt(
      `CF_Authorization=${token}`,
      { CF_ACCESS_TEAM_DOMAIN: TEAM_DOMAIN, CF_ACCESS_AUD: " , " },
      jwksResolver,
    );
    expect(result).toBeNull();
  });

  it("rejects a well-formed token signed by somebody else's key", async () => {
    // Every claim is correct — issuer, audience, expiry — and only the
    // signature is wrong. This is the check that proves verification is
    // actually cryptographic and not just claim-shape inspection: forge a
    // token with the right `aud` and it must still be refused.
    const attackerKeys = await generateKeyPair("RS256");
    const forged = await new SignJWT({ email: "attacker@example.com" })
      .setProtectedHeader({ alg: "RS256" })
      .setIssuedAt()
      .setIssuer(`https://${TEAM_DOMAIN}`)
      .setAudience(AUD)
      .setExpirationTime("10m")
      .sign(attackerKeys.privateKey);

    const result = await validateAccessJwt(`CF_Authorization=${forged}`, env, jwksResolver);
    expect(result).toBeNull();
  });

  it("rejects an unsigned (alg: none) token", async () => {
    // `jose` refuses to *produce* an `alg: none` JWS, so hand-assemble one:
    // header + payload + empty signature, the classic algorithm-confusion
    // forgery. `validateAccessJwt` pins `algorithms: ["RS256"]` on top of
    // jose's own refusal to accept `none`.
    const b64 = (obj: unknown) =>
      btoa(JSON.stringify(obj)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
    const unsigned = `${b64({ alg: "none", typ: "JWT" })}.${b64({
      email: "attacker@example.com",
      iss: `https://${TEAM_DOMAIN}`,
      aud: AUD,
      exp: Math.floor(Date.now() / 1000) + 600,
    })}.`;

    await expect(validateAccessJwt(`CF_Authorization=${unsigned}`, env, jwksResolver)).resolves.toBeNull();
  });

  it("rejects a token whose header claims a symmetric algorithm (alg confusion)", async () => {
    // HS256-signed with the RSA public key's raw bytes as the MAC secret —
    // the textbook RS256→HS256 confusion attack. Must be refused both
    // because the JWKS holds no symmetric key and because HS256 is not in
    // the accepted-algorithms list.
    const b64 = (obj: unknown) =>
      btoa(JSON.stringify(obj)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
    const confused = `${b64({ alg: "HS256", typ: "JWT" })}.${b64({
      email: "attacker@example.com",
      iss: `https://${TEAM_DOMAIN}`,
      aud: AUD,
      exp: Math.floor(Date.now() / 1000) + 600,
    })}.bm90LWEtcmVhbC1tYWM`;

    await expect(validateAccessJwt(`CF_Authorization=${confused}`, env, jwksResolver)).resolves.toBeNull();
  });

  it("resolves the sub claim when the token carries one", async () => {
    const token = await new SignJWT({ email: "operator@2amlogic.com", sub: "user-123" })
      .setProtectedHeader({ alg: "RS256" })
      .setIssuedAt()
      .setIssuer(`https://${TEAM_DOMAIN}`)
      .setAudience(AUD)
      .setExpirationTime("10m")
      .setSubject("user-123")
      .sign(privateKey);
    const result = await validateAccessJwt(`CF_Authorization=${token}`, env, jwksResolver);
    expect(result).toEqual({ email: "operator@2amlogic.com", sub: "user-123" });
  });
});
