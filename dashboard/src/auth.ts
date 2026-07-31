/**
 * Per-host ingest key authentication (Epic #4702, Phase 2 AC: "each daemon's
 * push is authenticated by a distinct key; a revoked key stops accepting
 * new writes without affecting other hosts").
 *
 * Keys are never stored in plaintext — only a SHA-256 hex digest is
 * persisted in D1 (`hosts.key_hash`), so a leaked database dump does not
 * hand over live credentials. The bearer token presented on `/ingest`
 * authenticates the *host identity* — the authoritative `host_id` for a
 * request is the one bound to the matched key, not whatever `host_id`
 * happens to be inside the envelope bodies (which is otherwise an opaque,
 * client-supplied string per the schema doc) — this prevents one host's key
 * from writing records under another host's identity.
 */

export interface HostAuthResult {
  hostId: string;
}

/** SHA-256 hex digest of `key`, using the Workers runtime's native
 * `crypto.subtle` (no extra dependency, no Node `Buffer`). */
export async function hashIngestKey(key: string): Promise<string> {
  const data = new TextEncoder().encode(key);
  const digest = await crypto.subtle.digest("SHA-256", data);
  return toHex(digest);
}

function toHex(buffer: ArrayBuffer): string {
  return Array.from(new Uint8Array(buffer))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

/** Extract the bearer token from an `Authorization` header value, or
 * `null` if the header is absent or not a `Bearer` scheme. */
export function extractBearerToken(authorizationHeader: string | null): string | null {
  if (!authorizationHeader) return null;
  const match = /^Bearer\s+(.+)$/i.exec(authorizationHeader.trim());
  return match?.[1] ? match[1].trim() : null;
}

/**
 * Authenticate an ingest request's `Authorization` header against the
 * `hosts` table. Returns the authoritative host id on success, or `null` on
 * any failure (missing header, unknown key, revoked key) — the caller
 * always responds 401 in every `null` case, deliberately not distinguishing
 * "unknown key" from "revoked key" in the HTTP response (a distinguishable
 * error would let an attacker enumerate which keys once existed).
 */
export async function authenticateHost(
  db: D1Database,
  authorizationHeader: string | null,
): Promise<HostAuthResult | null> {
  const token = extractBearerToken(authorizationHeader);
  if (!token) return null;

  const keyHash = await hashIngestKey(token);
  const row = await db
    .prepare("SELECT host_id, revoked_at FROM hosts WHERE key_hash = ?")
    .bind(keyHash)
    .first<{ host_id: string; revoked_at: string | null }>();

  if (!row || row.revoked_at !== null) {
    return null;
  }
  return { hostId: row.host_id };
}
