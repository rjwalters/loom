import { env } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { authenticateHost, extractBearerToken, hashIngestKey } from "../src/auth";
import { revokeHost, seedHost } from "./testHelpers";

describe("extractBearerToken", () => {
  it("extracts the token from a well-formed header", () => {
    expect(extractBearerToken("Bearer abc123")).toBe("abc123");
  });

  it("is case-insensitive on the scheme", () => {
    expect(extractBearerToken("bearer abc123")).toBe("abc123");
  });

  it("returns null for a missing header", () => {
    expect(extractBearerToken(null)).toBeNull();
  });

  it("returns null for a non-Bearer scheme", () => {
    expect(extractBearerToken("Basic abc123")).toBeNull();
  });
});

describe("hashIngestKey", () => {
  it("is deterministic and never returns the plaintext key", async () => {
    const a = await hashIngestKey("top-secret-key");
    const b = await hashIngestKey("top-secret-key");
    expect(a).toBe(b);
    expect(a).not.toContain("top-secret-key");
    expect(a).toMatch(/^[0-9a-f]{64}$/); // SHA-256 hex digest
  });

  it("different keys hash differently", async () => {
    const a = await hashIngestKey("key-one");
    const b = await hashIngestKey("key-two");
    expect(a).not.toBe(b);
  });
});

describe("authenticateHost", () => {
  it("authenticates a valid, non-revoked key and resolves the authoritative host id", async () => {
    await seedHost(env.DB, "host-alpha", "alpha-key");
    const result = await authenticateHost(env.DB, "Bearer alpha-key");
    expect(result).toEqual({ hostId: "host-alpha" });
  });

  it("rejects a missing Authorization header", async () => {
    const result = await authenticateHost(env.DB, null);
    expect(result).toBeNull();
  });

  it("rejects an unknown key", async () => {
    const result = await authenticateHost(env.DB, "Bearer no-such-key");
    expect(result).toBeNull();
  });

  it("rejects a revoked key while leaving other hosts' keys usable", async () => {
    await seedHost(env.DB, "host-beta", "beta-key");
    await seedHost(env.DB, "host-gamma", "gamma-key");
    await revokeHost(env.DB, "host-beta");

    const revoked = await authenticateHost(env.DB, "Bearer beta-key");
    expect(revoked).toBeNull();

    const stillGood = await authenticateHost(env.DB, "Bearer gamma-key");
    expect(stillGood).toEqual({ hostId: "host-gamma" });
  });
});
