import type { JwtPayload } from "../types";

/**
 * Create a JWT token using Web Crypto API (edge-compatible)
 */
export async function createJwt(
  payload: Omit<JwtPayload, "iat" | "exp">,
  secret: string,
  expiresIn: string
): Promise<string> {
  const now = Math.floor(Date.now() / 1000);
  const exp = now + parseExpiration(expiresIn);

  const fullPayload: JwtPayload = {
    ...payload,
    iat: now,
    exp,
  };

  const header = { alg: "HS256", typ: "JWT" };
  const encodedHeader = base64UrlEncode(JSON.stringify(header));
  const encodedPayload = base64UrlEncode(JSON.stringify(fullPayload));
  const signatureInput = `${encodedHeader}.${encodedPayload}`;

  const signature = await sign(signatureInput, secret);

  return `${signatureInput}.${signature}`;
}

/**
 * Verify and decode a JWT token
 */
export async function verifyJwt(token: string, secret: string): Promise<JwtPayload> {
  const parts = token.split(".");
  if (parts.length !== 3) {
    throw new Error("Invalid token format");
  }

  const [encodedHeader, encodedPayload, signature] = parts;
  const signatureInput = `${encodedHeader}.${encodedPayload}`;

  // Verify signature
  const expectedSignature = await sign(signatureInput, secret);
  if (signature !== expectedSignature) {
    throw new Error("Invalid signature");
  }

  // Decode payload
  const payload = JSON.parse(base64UrlDecode(encodedPayload)) as JwtPayload;

  // Check expiration
  const now = Math.floor(Date.now() / 1000);
  if (payload.exp < now) {
    throw new Error("Token expired");
  }

  return payload;
}

/**
 * Sign data using HMAC-SHA256
 */
async function sign(data: string, secret: string): Promise<string> {
  const encoder = new TextEncoder();
  const keyData = encoder.encode(secret);
  const dataBuffer = encoder.encode(data);

  const key = await crypto.subtle.importKey(
    "raw",
    keyData,
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"]
  );

  const signature = await crypto.subtle.sign("HMAC", key, dataBuffer);
  return base64UrlEncode(String.fromCharCode(...new Uint8Array(signature)));
}

/**
 * Parse expiration string (e.g., "7d", "1h", "30m")
 */
function parseExpiration(exp: string): number {
  const match = exp.match(/^(\d+)([smhd])$/);
  if (!match) {
    throw new Error(`Invalid expiration format: ${exp}`);
  }

  const value = parseInt(match[1], 10);
  const unit = match[2];

  switch (unit) {
    case "s":
      return value;
    case "m":
      return value * 60;
    case "h":
      return value * 60 * 60;
    case "d":
      return value * 60 * 60 * 24;
    default:
      throw new Error(`Unknown time unit: ${unit}`);
  }
}

function base64UrlEncode(str: string): string {
  const base64 = btoa(str);
  return base64.replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function base64UrlDecode(str: string): string {
  let base64 = str.replace(/-/g, "+").replace(/_/g, "/");
  const padding = base64.length % 4;
  if (padding) {
    base64 += "=".repeat(4 - padding);
  }
  return atob(base64);
}
